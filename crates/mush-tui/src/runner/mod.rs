//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

mod caching_backend;
mod commands;
mod config;
mod input;
mod looping;
mod panes;
mod render;
mod runtime;
mod streams;
mod terminal;

use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;

use self::caching_backend::CachingBackend;
pub use self::config::{
    HintMode, LastModelSaver, PaneSnapshot, PromptEnricher, SessionSaver, SessionSnapshot,
    ThinkingPrefsSaver, TitleUpdater, TuiConfig,
};
use self::input::LoopAction;
use self::looping::run_loop_iteration;
use self::render::draw_panes;
use self::runtime::RunnerRuntime;
use self::streams::{
    StreamConfig, StreamDeps, StreamState, new_agent_streams, start_pending_streams,
};
use self::terminal::{
    TerminalStateGuard, cleanup, enter_tui_terminal, install_panic_cleanup_hook,
    probe_image_picker, restore_terminal_state,
};

/// run the interactive TUI
pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &ToolRegistry,
    registry: &ApiRegistry,
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

    let image_picker = probe_image_picker(tui_config.terminal_policy);
    let mut terminal_guard = TerminalStateGuard::new();

    enter_tui_terminal(tui_config.terminal_policy)?;
    install_panic_cleanup_hook();

    let backend = CachingBackend::new(CrosstermBackend::new(io::stdout()));
    let size_cache = backend.cache_handle();
    let mut terminal = Terminal::new(backend)?;
    // alt screen may inherit content from the main screen buffer on some
    // terminals (esp. after pre-tui eprintln output like the startup report).
    // clear explicitly so no stale lines leak through cells that widgets
    // don't overwrite
    terminal.clear()?;
    let (mut runtime, services) = RunnerRuntime::new(&mut tui_config).await;

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

    let mut agent_streams = new_agent_streams();
    let mut stream_state = StreamState::new();
    let mut last_draw = std::time::Instant::now();

    'ui: loop {
        start_pending_streams(
            &mut agent_streams,
            &mut stream_state,
            &mut runtime.pane_mgr,
            &mut runtime.pending_prompt,
            StreamDeps {
                config: StreamConfig {
                    default_model: tui_config.model.clone(),
                    system_prompt: tui_config.system_prompt.clone(),
                    options: tui_config.options.clone(),
                    max_turns: tui_config.max_turns,
                    prompt_enricher: tui_config.prompt_enricher.clone(),
                    hint_mode: tui_config.hint_mode,
                    provider_api_keys: tui_config.provider_api_keys.clone(),
                    confirm_tools: tui_config.confirm_tools,
                    auto_compact: tui_config.auto_compact,
                    compaction_model: tui_config.compaction_model.clone(),
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

        let action = run_loop_iteration(
            &mut agent_streams,
            &mut stream_state,
            &mut runtime,
            &services,
            &mut tui_config,
            registry,
            &image_picker,
            &size_cache,
        )
        .await?;
        if matches!(action, LoopAction::Quit) {
            break 'ui;
        }

        runtime.apply_config_reload(&mut tui_config);
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

        runtime.tick_streaming_panes();
        runtime.notify_cache_state(tui_config.cache_timer);

        // redraw on state changes, or every ~1s so timers tick
        let should_draw = matches!(action, LoopAction::Redraw)
            || last_draw.elapsed() >= std::time::Duration::from_secs(1);
        if should_draw {
            runtime.poll_usage().await;
            draw_panes(
                &mut terminal,
                &mut runtime.pane_mgr,
                &image_picker,
                &tui_config.settings,
            )?;
            last_draw = std::time::Instant::now();
        }
    }

    cleanup(&mut terminal)?;
    terminal_guard.disarm();
    Ok(())
}
