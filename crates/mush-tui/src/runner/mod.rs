use std::io;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod commands;
mod config;
mod control;
#[cfg(test)]
mod control_tests;
mod diag;
mod inbox;
mod input;
mod last_model;
mod looping;
mod render;
mod runtime;
mod size_cached_backend;
mod streams;
mod subagents;
mod terminal;
mod thinking_prefs;

use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;

use crate::screen::Screen;

pub use self::config::{
    KernelNotices, LiveToolOutput, ModelResolver, PaneSnapshot, ReloadCallback, ReloadedContext,
    SessionSaver, SessionSnapshot, SkillCatalogue, TuiConfig,
};
use self::input::LoopAction;
pub use self::last_model::{LastModels, LastModelsSaver};
use self::looping::run_loop_iteration;
use self::render::draw_panes;
pub use self::runtime::DiscoveryUpdate;
use self::runtime::RunnerRuntime;
use self::size_cached_backend::{SizeCachedBackend, spawn_sigwinch_listener};
use self::streams::{StreamDeps, StreamState, start_pending_streams};
use self::terminal::{
    TerminalStateGuard, cleanup, enter_tui_terminal, install_panic_cleanup_hook,
    probe_image_picker, restore_terminal_state,
};
pub use self::thinking_prefs::{ThinkingPrefs, ThinkingPrefsSaver, canonical_dir};

use self::diag::verify_terminal_restored;

pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &ToolRegistry,
    registry: &ApiRegistry,
    startup_discovery: Option<std::sync::mpsc::Receiver<DiscoveryUpdate>>,
) -> io::Result<()> {
    let env = crate::terminal_policy::TtyEnvironment::detect();
    if !env.is_tty() {
        let reason = env.broken_reason().unwrap_or("unknown");
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "mush TUI requires an interactive terminal: {reason}. \
                 try `mush -p \"your prompt\"` for one-shot use, or run mush \
                 directly in a terminal (not piped, not inside CI)"
            ),
        ));
    }

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
    let mut runtime = RunnerRuntime::new(&mut tui_config).await;
    if let Some(rx) = startup_discovery {
        runtime.install_discovery_updates(rx);
    }

    let ipc_listener = if tui_config.control {
        let card = tui_config
            .agent_card
            .as_ref()
            .ok_or_else(|| io::Error::other("control requires an agent card"))?;
        let card = std::sync::Arc::new(card.clone());
        let control = mush_agent::ipc::control::Control::new(tui_config.session_id.to_string());
        let sock = mush_agent::ipc::control_socket_path(&control.target().instance);
        let listener = mush_agent::IpcListener::start_control(sock.clone(), card, control.clone())?;
        runtime.session.runtime.control = Some(control);
        runtime.session.runtime.control_socket = Some(sock);
        Some(listener)
    } else {
        None
    };

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
    let mut orchestrator_inbox = inbox::attach(
        inbox::OrchestratorInbox::from_env(&tui_config),
        &mut runtime,
    );
    #[cfg(all(feature = "signal", not(test)))]
    {
        stream_state.signal = tokio::task::spawn_blocking(mush_notify::user_dispatcher)
            .await
            .ok()
            .flatten();
    }
    let mut last_draw = std::time::Instant::now();
    let mut last_animation = std::time::Instant::now();
    let mut loop_iteration = 0u64;
    let mut draw_count = 1u64;
    let stall_watchdog = diag::StallWatchdog::from_env();

    'ui: loop {
        loop_iteration = loop_iteration.wrapping_add(1);
        report_loop_progress(
            &stall_watchdog,
            &runtime,
            loop_iteration,
            draw_count,
            "start_pending_streams",
            "before start_pending_streams",
        );
        let inbox_connected = orchestrator_inbox.is_some();
        let inbox_ready = inbox::poll(
            &mut orchestrator_inbox,
            &stream_state,
            &mut runtime,
            &tui_config,
        )
        .await;
        let inbox_disabled = inbox_connected && orchestrator_inbox.is_none();
        let notice_submitted = streams::submit_kernel_notices(
            &mut runtime.session,
            &stream_state,
            &tui_config.kernel_notices,
        )
        .await;
        if inbox_ready {
            control::poll(&stream_state, &mut runtime, &tui_config).await?;
            let control_endpoint = runtime.session.control_endpoint();
            start_pending_streams(
                &mut stream_state,
                &mut runtime.session,
                &mut runtime.pending_prompt,
                StreamDeps {
                    config: tui_config.turn_config(),
                    injections: mush_agent::AgentInjections {
                        lifecycle_hooks: tui_config.lifecycle_hooks.clone(),
                        cwd: Some(tui_config.cwd.clone()),
                        dynamic_system_context: tui_config.dynamic_system_context.clone(),
                        file_rules: tui_config.file_rules.clone(),
                        lsp_diagnostics: tui_config.lsp_diagnostics.clone(),
                        control_endpoint,
                    },
                    tools,
                    registry,
                },
            )
            .await;
        }

        report_loop_progress(
            &stall_watchdog,
            &runtime,
            loop_iteration,
            draw_count,
            "waiting_for_event",
            "parked before select/input handling",
        );
        let action = run_loop_iteration(
            &mut stream_state,
            &mut runtime,
            &mut tui_config,
            registry,
            &image_picker,
            &size_cache,
            last_draw,
        )
        .await?;
        report_loop_progress(
            &stall_watchdog,
            &runtime,
            loop_iteration,
            draw_count,
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

        let compaction_finished = runtime.poll_pending_compactions(&tui_config).await;
        let side_question_finished = runtime.session.poll_side_questions().await;

        let maintenance_tick = last_draw.elapsed() >= std::time::Duration::from_secs(1)
            && runtime.needs_periodic_redraw(tui_config.cache_timer);

        let should_draw = matches!(action, LoopAction::Redraw)
            || inbox_disabled
            || notice_submitted
            || discovery_updated
            || compaction_finished
            || side_question_finished
            || maintenance_tick;
        if should_draw {
            if last_animation.elapsed() >= looping::ANIMATION_FRAME_INTERVAL
                && runtime.tick_animation_panes()
            {
                last_animation = std::time::Instant::now();
            }
            trace_draw_reasons(
                action,
                discovery_updated,
                compaction_finished,
                maintenance_tick,
            );
            runtime.poll_usage();
            report_loop_progress(
                &stall_watchdog,
                &runtime,
                loop_iteration,
                draw_count,
                "draw",
                "before draw_panes",
            );
            draw_panes(
                &mut terminal,
                &mut screen,
                &mut runtime.session,
                &tui_config.settings,
            )?;
            draw_count = draw_count.wrapping_add(1);
            last_draw = std::time::Instant::now();
        }
    }

    runtime
        .session
        .runtime
        .turns
        .abort(mush_ai::types::PaneId::new(1));
    let session_id = runtime.session.runtime.session_id.clone();
    runtime.session.runtime.snapshot_kernel(&session_id).await;
    if let Some(listener) = ipc_listener {
        listener.shutdown().await.map_err(io::Error::other)?;
    }
    cleanup(&mut terminal)?;
    sigwinch_task.abort();
    terminal_guard.disarm();
    verify_terminal_restored(vec!["exit path: run_tui clean return".into()]);
    Ok(())
}

fn report_loop_progress(
    watchdog: &diag::StallWatchdog,
    runtime: &RunnerRuntime,
    loop_iteration: u64,
    draw_count: u64,
    phase: &'static str,
    detail: impl Into<String>,
) {
    watchdog.progress(
        phase,
        detail,
        diag::LoopSnapshot {
            loop_iteration,
            draw_count,
            active_streams: runtime.session.runtime.turns.active_turns(),
            active_tools: runtime.active_tool_labels(),
        },
    );
}

fn trace_draw_reasons(
    action: LoopAction,
    discovery_updated: bool,
    compaction_finished: bool,
    maintenance_tick: bool,
) {
    if matches!(action, LoopAction::Redraw) {
        tracing::trace!(target: "mush_tui::draw_reason", reason = "loop_action", "draw requested");
    }
    if discovery_updated {
        tracing::trace!(target: "mush_tui::draw_reason", reason = "discovery_updated", "draw requested");
    }
    if compaction_finished {
        tracing::trace!(target: "mush_tui::draw_reason", reason = "compaction_finished", "draw requested");
    }
    if maintenance_tick {
        tracing::trace!(target: "mush_tui::draw_reason", reason = "maintenance_tick", "draw requested");
    }
}
