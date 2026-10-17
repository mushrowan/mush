use std::io;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod commands;
mod config;
#[cfg(test)]
mod control_tests;
mod diag;
mod external_editor;
mod input;
mod last_model;
mod looping;
mod render;
mod runtime;
mod size_cached_backend;
mod streams;
mod subagents;
mod terminal;

use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;
use mush_runtime::AgentConnection;

use crate::screen::Screen;

pub use self::config::{
    AgentRoster, ModelResolver, PaneSnapshot, ReloadCallback, ReloadedContext, SessionSaver,
    SessionSnapshot, SkillCatalogue, TuiConfig, WorkerOpener, WorkerTarget,
};
use self::input::LoopAction;
pub use self::last_model::{LastModels, LastModelsSaver, canonical_dir};
use self::looping::run_loop_iteration;
use self::render::draw_panes;
pub use self::runtime::DiscoveryUpdate;
use self::runtime::RunnerRuntime;
use self::size_cached_backend::{SizeCachedBackend, spawn_sigwinch_listener};
use self::streams::StreamState;
use self::terminal::{
    TerminalStateGuard, cleanup, enter_tui_terminal, install_panic_cleanup_hook,
    probe_image_picker, restore_terminal_state,
};

use self::diag::verify_terminal_restored;

pub async fn run_tui(
    tui_config: TuiConfig,
    worker: mush_runtime::WorkerConfig,
    tools: &ToolRegistry,
    registry: &ApiRegistry,
    startup_discovery: Option<std::sync::mpsc::Receiver<DiscoveryUpdate>>,
) -> io::Result<()> {
    let mush_runtime::HostedWorker {
        connection,
        ipc_listener,
    } = mush_runtime::host_worker(worker, tools, registry)?;
    run_tui_with(
        tui_config,
        Box::new(connection),
        ipc_listener,
        ExitPolicy::Shutdown,
        startup_discovery,
    )
    .await
    .map(drop)
}

pub async fn run_tui_attached(
    tui_config: TuiConfig,
    connection: Box<dyn AgentConnection>,
    startup_discovery: Option<std::sync::mpsc::Receiver<DiscoveryUpdate>>,
) -> io::Result<mush_ai::types::SessionId> {
    run_tui_with(
        tui_config,
        connection,
        None,
        ExitPolicy::Detach,
        startup_discovery,
    )
    .await
}

pub enum ExitPolicy {
    Shutdown,
    Detach,
}

async fn run_tui_with(
    mut tui_config: TuiConfig,
    connection: Box<dyn AgentConnection>,
    ipc_listener: Option<mush_agent::IpcListener>,
    exit: ExitPolicy,
    startup_discovery: Option<std::sync::mpsc::Receiver<DiscoveryUpdate>>,
) -> io::Result<mush_ai::types::SessionId> {
    require_tty()?;
    restore_terminal_state();
    let mut startup_phases = vec![diag::StartupPhase::new(
        "after_preflight_restore",
        crossterm::terminal::size().map(Some),
    )];

    let image_picker = probe_image_picker(tui_config.terminal_policy);
    startup_phases.push(diag::StartupPhase::new(
        "after_image_probe",
        crossterm::terminal::size().map(Some),
    ));
    let mut terminal_guard = TerminalStateGuard::new();

    enter_tui_terminal(tui_config.terminal_policy)?;
    startup_phases.push(diag::StartupPhase::new(
        "after_enter_tui_terminal",
        crossterm::terminal::size().map(Some),
    ));
    install_panic_cleanup_hook();

    let backend = SizeCachedBackend::new(CrosstermBackend::new(io::stdout()));
    let size_cache = backend.cache_handle();
    let sigwinch_task = spawn_sigwinch_listener(size_cache.resize_pending_flag());
    let mut screen = Screen::new();
    let mut terminal = Terminal::new(backend)?;
    startup_phases.push(diag::StartupPhase::new(
        "after_terminal_new",
        terminal.size().map(|size| Some((size.width, size.height))),
    ));
    let mut runtime = RunnerRuntime::new(&mut tui_config, connection).await;
    if let Some(rx) = startup_discovery {
        runtime.install_discovery_updates(rx);
    }
    if tui_config.open_session_picker {
        crate::slash::commands::open_sessions_picker(
            &mut runtime.session.app,
            tui_config.agent_roster.clone(),
        );
    }
    if let Some(notice) = tui_config.startup_notice.take() {
        runtime.session.app.push_system_message(notice);
    }
    for command in std::mem::take(&mut tui_config.startup_commands) {
        commands::send_command(&mut runtime.session, &mut tui_config, command).await;
    }
    commands::refresh_decisions(&mut runtime.session, &mut tui_config).await;

    draw_panes(
        &mut terminal,
        &mut screen,
        &mut runtime.session,
        &tui_config.settings,
    )?;
    startup_phases.push(diag::StartupPhase::new(
        "after_first_draw",
        terminal.size().map(|size| Some((size.width, size.height))),
    ));
    diag::write_startup_diagnostics(startup_phases, Vec::new());

    let mut stream_state = StreamState::new();
    #[cfg(all(feature = "signal", not(test)))]
    {
        stream_state.signal = tokio::task::spawn_blocking(mush_notify::user_dispatcher)
            .await
            .ok()
            .flatten();
    }
    let mut last_draw = std::time::Instant::now();
    let mut progress = LoopProgress {
        watchdog: diag::StallWatchdog::from_env(),
        iteration: 0,
        draws: 1,
    };

    'ui: loop {
        progress.iteration = progress.iteration.wrapping_add(1);
        progress.report(
            &runtime,
            "start_pending_streams",
            "before start_pending_streams",
        );
        start_pending_streams(&mut runtime).await;
        progress.report(
            &runtime,
            "waiting_for_event",
            "parked before select/input handling",
        );
        let action = run_loop_iteration(
            &mut stream_state,
            &mut runtime,
            &mut tui_config,
            &image_picker,
            &size_cache,
            last_draw,
        )
        .await?;
        progress.report(
            &runtime,
            "post_loop_iteration",
            format!("action={action:?}"),
        );
        if matches!(action, LoopAction::Quit) {
            break 'ui;
        }

        runtime.apply_config_reload(&mut tui_config);
        let discovery_updated = runtime.apply_discovery_updates();
        if runtime.focused_should_quit() {
            break;
        }

        runtime.notify_cache_state(tui_config.cache_timer);
        stream_state.poll_signal().await;

        let maintenance_tick = last_draw.elapsed() >= std::time::Duration::from_secs(1)
            && runtime.needs_periodic_redraw(tui_config.cache_timer);

        let should_draw =
            matches!(action, LoopAction::Redraw) || discovery_updated || maintenance_tick;
        if should_draw {
            runtime.step_animation(std::time::Instant::now());
            runtime.tick_streaming_text_panes();
            trace_draw_reasons(action, discovery_updated, maintenance_tick);
            runtime.poll_usage();
            progress.report(&runtime, "draw", "before draw_panes");
            draw_panes(
                &mut terminal,
                &mut screen,
                &mut runtime.session,
                &tui_config.settings,
            )?;
            runtime.drew();
            progress.draws = progress.draws.wrapping_add(1);
            last_draw = std::time::Instant::now();
        }
    }

    if let ExitPolicy::Shutdown = exit {
        let _ = runtime
            .session
            .connection
            .send(mush_runtime::ClientCommand::Shutdown)
            .await;
    }
    if let Some(listener) = ipc_listener {
        listener.shutdown().await.map_err(io::Error::other)?;
    }
    cleanup(&mut terminal)?;
    sigwinch_task.abort();
    terminal_guard.disarm();
    verify_terminal_restored(vec!["exit path: run_tui clean return".into()]);
    Ok(runtime.session.connection.state().session_id)
}

fn require_tty() -> io::Result<()> {
    let env = crate::terminal_policy::TtyEnvironment::detect();
    if env.is_tty() {
        return Ok(());
    }
    let reason = env.broken_reason().unwrap_or("unknown");
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        format!(
            "mush TUI requires an interactive terminal: {reason}. \
             try `mush -p \"your prompt\"` for one-shot use, or run mush \
             directly in a terminal (not piped, not inside CI)"
        ),
    ))
}

async fn start_pending_streams(runtime: &mut RunnerRuntime) {
    let thinking_level = runtime.session.app.thinking_level.clone();
    let connection = &mut runtime.session.connection;
    if connection.state().thinking_level != thinking_level {
        let _ = connection
            .send(mush_runtime::ClientCommand::SetThinking(thinking_level))
            .await;
    }
    if let Some(prompt) = runtime.pending_prompt.take() {
        let _ = connection
            .send(mush_runtime::ClientCommand::QueuePrompt {
                text: prompt,
                images: Vec::new(),
            })
            .await;
    }
    connection.tick().await;
}

struct LoopProgress {
    watchdog: diag::StallWatchdog,
    iteration: u64,
    draws: u64,
}

impl LoopProgress {
    fn report(&self, runtime: &RunnerRuntime, phase: &'static str, detail: impl Into<String>) {
        self.watchdog.progress(
            phase,
            detail,
            diag::LoopSnapshot {
                loop_iteration: self.iteration,
                draw_count: self.draws,
                active_streams: runtime.session.connection.state().active_turns,
                active_tools: runtime.active_tool_labels(),
            },
        );
    }
}

fn trace_draw_reasons(action: LoopAction, discovery_updated: bool, maintenance_tick: bool) {
    if matches!(action, LoopAction::Redraw) {
        tracing::trace!(target: "mush_tui::draw_reason", reason = "loop_action", "draw requested");
    }
    if discovery_updated {
        tracing::trace!(target: "mush_tui::draw_reason", reason = "discovery_updated", "draw requested");
    }
    if maintenance_tick {
        tracing::trace!(target: "mush_tui::draw_reason", reason = "maintenance_tick", "draw requested");
    }
}
