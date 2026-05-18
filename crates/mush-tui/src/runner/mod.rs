//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod caching_backend;
mod commands;
mod config;
mod diag;
mod input;
mod last_model;
mod looping;
mod panes;
mod render;
mod runtime;
mod streams;
mod terminal;
mod thinking_prefs;

use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;

use self::caching_backend::{CachingBackend, spawn_sigwinch_listener};
pub use self::config::{
    ModelResolver, PaneSnapshot, ReloadCallback, ReloadedContext, SessionSaver, SessionSnapshot,
    TitleUpdater, TuiConfig,
};
use self::input::LoopAction;
pub use self::last_model::{LastModels, LastModelsSaver};
use self::looping::run_loop_iteration;
use self::render::draw_panes;
pub use self::runtime::DiscoveryUpdate;
use self::runtime::RunnerRuntime;
use self::streams::{
    AgentStreams, StreamConfig, StreamDeps, StreamState, new_agent_streams, start_pending_streams,
};
use self::terminal::{
    TerminalStateGuard, cleanup, enter_tui_terminal, install_panic_cleanup_hook,
    probe_image_picker, restore_terminal_state,
};
pub use self::thinking_prefs::{ThinkingPrefs, ThinkingPrefsSaver, canonical_dir};

use self::diag::verify_terminal_restored;

/// run the interactive TUI
pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &ToolRegistry,
    registry: &ApiRegistry,
    startup_discovery: Option<std::sync::mpsc::Receiver<DiscoveryUpdate>>,
) -> io::Result<()> {
    // fail fast if we can't host a full TUI. piped stdin/stdout would
    // otherwise leak raw-mode escape sequences into the output stream,
    // hang on probe responses, or drop keyboard input. the matching fix
    // for users is `mush -p` / `mush --print` for one-shot prompts
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

    let backend = CachingBackend::new(CrosstermBackend::new(io::stdout()));
    let size_cache = backend.cache_handle();
    // listen for SIGWINCH off the main event loop so a fast-streaming
    // burst can't starve `Event::Resize` processing and leave the size
    // cache stale. flips `resize_pending` on the shared state; the
    // backend's `size()` / `window_size()` consume it on the next call
    let sigwinch_task = spawn_sigwinch_listener(size_cache.resize_pending_flag());
    let mut terminal = Terminal::new(backend)?;
    startup_phases.push(diag::StartupPhase::new(
        "after_terminal_new",
        terminal.size().map(|size| Some((size.width, size.height))),
    ));
    // alt screen may inherit content from the main screen buffer on some
    // terminals (esp. after pre-tui eprintln output like the startup report).
    // clear explicitly so no stale lines leak through cells that widgets
    // don't overwrite
    render::clear_for_first_frame(&mut terminal)?;
    startup_phases.push(diag::StartupPhase::new(
        "after_terminal_clear",
        terminal.size().map(|size| Some((size.width, size.height))),
    ));
    let (mut runtime, services) = RunnerRuntime::new(&mut tui_config).await;
    if let Some(rx) = startup_discovery {
        runtime.install_discovery_updates(rx);
    }

    // start IPC listener if we have an agent card
    let _ipc_listener = tui_config.agent_card.as_ref().and_then(|card| {
        let card = std::sync::Arc::new(card.clone());
        let sock = mush_agent::ipc::socket_path(&std::process::id().to_string());
        match mush_agent::IpcListener::start(sock, card) {
            Ok(listener) => Some(listener),
            Err(e) => {
                tracing::warn!("failed to start IPC listener: {e}");
                None
            }
        }
    });

    draw_panes(
        &mut terminal,
        &mut runtime.pane_mgr,
        &image_picker,
        &tui_config.settings,
    )?;
    startup_phases.push(diag::StartupPhase::new(
        "after_first_draw",
        terminal.size().map(|size| Some((size.width, size.height))),
    ));
    diag::write_startup_diagnostics(startup_phases, Vec::new());

    let mut agent_streams = new_agent_streams();
    let mut stream_state = StreamState::new();
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
            &agent_streams,
            loop_iteration,
            draw_count,
            "start_pending_streams",
            "before start_pending_streams",
        );
        start_pending_streams(
            &mut agent_streams,
            &mut stream_state,
            &mut runtime.pane_mgr,
            &mut runtime.pending_prompt,
            StreamDeps {
                config: StreamConfig {
                    default_model: tui_config.model.clone(),
                    system_prompt: tui_config.system_prompt.clone(),
                    options: tui_config.stream_options(),
                    max_turns: tui_config.max_turns,
                    provider_api_keys: tui_config.provider_api_keys.clone(),
                    confirm_tools: tui_config.confirm_tools,
                    auto_compact: tui_config.auto_compact,
                    compaction_model: tui_config.compaction_options(),
                    forbidden_chars: tui_config.forbidden_chars.clone(),
                },
                injections: mush_agent::AgentInjections {
                    lifecycle_hooks: tui_config.lifecycle_hooks.clone(),
                    cwd: Some(tui_config.cwd.clone()),
                    dynamic_system_context: tui_config.dynamic_system_context.clone(),
                    file_rules: tui_config.file_rules.clone(),
                    lsp_diagnostics: tui_config.lsp_diagnostics.clone(),
                },
                tools,
                registry,
                message_bus: &services.message_bus,
                shared_state: &services.shared_state,
                file_tracker: &services.file_tracker,
            },
        )
        .await;

        report_loop_progress(
            &stall_watchdog,
            &runtime,
            &agent_streams,
            loop_iteration,
            draw_count,
            "run_loop_iteration",
            "before select/input handling",
        );
        let action = run_loop_iteration(
            &mut agent_streams,
            &mut stream_state,
            &mut runtime,
            &services,
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
            &agent_streams,
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

        // process pending delegations (fork panes for delegate_task tool calls)
        panes::process_delegations(
            &mut runtime.pane_mgr,
            &tui_config,
            &services.message_bus,
            &services.delegation_queue,
        );

        runtime.notify_cache_state(tui_config.cache_timer);

        // finalise any background compaction tasks whose LLM call returned
        // since the last iteration; if any did, force a redraw so the new
        // summary, status, and token counter all show up immediately
        let compaction_finished = runtime.poll_pending_compactions().await;

        let maintenance_tick = last_draw.elapsed() >= std::time::Duration::from_secs(1)
            && runtime.needs_periodic_redraw(tui_config.cache_timer);

        // redraw on state changes, or every ~1s while visible timers tick
        let should_draw = matches!(action, LoopAction::Redraw)
            || discovery_updated
            || compaction_finished
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
            runtime.poll_usage().await;
            report_loop_progress(
                &stall_watchdog,
                &runtime,
                &agent_streams,
                loop_iteration,
                draw_count,
                "draw",
                "before draw_panes",
            );
            draw_panes(
                &mut terminal,
                &mut runtime.pane_mgr,
                &image_picker,
                &tui_config.settings,
            )?;
            draw_count = draw_count.wrapping_add(1);
            last_draw = std::time::Instant::now();
        }
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
    agent_streams: &AgentStreams<'_>,
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
            active_streams: agent_streams.len(),
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
