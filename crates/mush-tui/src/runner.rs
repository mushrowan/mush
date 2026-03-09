//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use mush_agent::AgentEvent;
use mush_agent::tool::ToolRegistry;
use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_session::ConversationState;

use crate::app::{self, App, AppEvent};
use crate::event_handler::{self, EventCtx};
use crate::input::handle_key;
use crate::runner_commands::{SlashEnv, handle_slash_action, save_thinking_pref};
use crate::runner_panes::{close_focused_pane, drain_inboxes, fork_pane};
use crate::runner_render::{draw_panes, handle_mouse};
use crate::runner_streams::{
    StreamDeps, StreamState, abort_focused_stream, answer_confirmation, edit_last_queued_steering,
    handle_agent_event_side_effects, new_agent_streams, poll_confirmation_prompt,
    poll_live_tool_output, start_pending_streams, submit_streaming_input,
};
use crate::runner_terminal::{
    TerminalStateGuard, cleanup, enter_tui_terminal, install_panic_cleanup_hook,
    restore_terminal_state,
};
use crate::slash;

/// callback that returns a relevance hint for a user message.
/// used to nudge the model toward the most relevant skills.
/// wrapped in Arc so it can be shared with context transform closures.
pub type PromptEnricher = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// how to inject skill relevance hints into the conversation
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HintMode {
    /// prepend hint to user message (evaluated once per message)
    #[default]
    Message,
    /// inject via context transform (re-evaluated before each LLM call)
    Transform,
    /// no hint (all skills still loaded in system prompt)
    None,
}

/// callback to persist per-model thinking level
pub type ThinkingPrefsSaver =
    std::sync::Arc<dyn Fn(&std::collections::HashMap<String, ThinkingLevel>) + Send + Sync>;

/// callback to persist last selected model id
pub type LastModelSaver = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// callback to update session title
pub type TitleUpdater = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// callback to persist session state and selected model
pub type SessionSaver = std::sync::Arc<dyn Fn(&ConversationState, &str) + Send + Sync>;

/// configuration for the TUI runner (owned, 'static-friendly)
pub struct TuiConfig {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub options: StreamOptions,
    pub max_turns: usize,
    /// initial conversation history (for session resume)
    pub initial_messages: Vec<Message>,
    /// colour theme
    pub theme: crate::theme::Theme,
    /// optional callback to auto-inject context (e.g. skills) per user message
    pub prompt_enricher: Option<PromptEnricher>,
    /// where to inject skill relevance hints
    pub hint_mode: HintMode,
    /// path to config file for hot-reload
    pub config_path: Option<std::path::PathBuf>,
    /// per-provider api keys from config
    pub provider_api_keys: std::collections::HashMap<String, String>,
    /// per-model thinking level prefs (loaded from disk at startup)
    pub thinking_prefs: std::collections::HashMap<String, ThinkingLevel>,
    /// callback to save thinking prefs when they change
    pub save_thinking_prefs: Option<ThinkingPrefsSaver>,
    /// callback to persist last selected model id
    pub save_last_model: Option<LastModelSaver>,
    /// callback to auto-save session after each agent turn
    pub save_session: Option<SessionSaver>,
    /// callback to update session title (called with LLM-generated title)
    pub update_title: Option<TitleUpdater>,
    /// prompt for confirmation before executing tools (off by default)
    pub confirm_tools: bool,
    /// automatically compact conversation when approaching context limit (off by default)
    pub auto_compact: bool,
    /// show dollar cost in status bar (off by default, toggle with /cost)
    pub show_cost: bool,
    /// emit system messages when cache reads are observed
    pub debug_cache: bool,
    /// show cache warmth countdown in status bar and send desktop notifications
    pub cache_timer: bool,
    /// how to display thinking text (hidden, collapse, expanded)
    pub thinking_display: crate::app::ThinkingDisplay,
    /// shared live tool output (updated by bash sink, read by TUI)
    pub tool_output_live: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// callback to get recent log entries (returns last N lines)
    pub log_buffer: Option<std::sync::Arc<dyn Fn(usize) -> Vec<String> + Send + Sync>>,
    /// multi-pane file isolation mode
    pub isolation_mode: crate::file_tracker::IsolationMode,
}

/// run the interactive TUI
pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &ToolRegistry,
    registry: &ApiRegistry,
) -> io::Result<()> {
    restore_terminal_state();

    // detect image protocol before entering alternate screen to avoid probe artifacts
    let image_picker = ratatui_image::picker::Picker::from_query_stdio().ok();

    let mut terminal_guard = TerminalStateGuard::new();

    enter_tui_terminal()?;
    install_panic_cleanup_hook();

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(tui_config.model.id.clone(), tui_config.model.context_window);
    app.thinking_level = tui_config.options.thinking.unwrap_or(ThinkingLevel::Off);
    app.thinking_display = tui_config.thinking_display;
    app.show_cost = tui_config.show_cost;
    app.cache_ttl_secs = if tui_config.cache_timer {
        crate::app::cache_ttl_secs(
            &tui_config.model.provider,
            tui_config.options.cache_retention.as_ref(),
        )
    } else {
        0
    };
    // populate tab completions and slash command descriptions
    let slash_cmds: &[(&str, &str)] = &[
        ("help", "show available commands"),
        ("keys", "show keyboard shortcuts"),
        ("clear", "clear conversation"),
        ("model", "show or switch model"),
        ("sessions", "browse and resume sessions"),
        ("branch", "branch from nth user message"),
        ("tree", "show conversation tree"),
        ("compact", "summarise old messages to free context"),
        ("export", "save conversation as markdown"),
        ("undo", "revert last turn"),
        ("search", "search conversation"),
        ("cost", "show session cost"),
        ("logs", "show recent log entries"),
        ("injection", "toggle prompt injection preview"),
        ("close", "close focused pane"),
        ("broadcast", "send a message to all panes"),
        ("lock", "lock a file for this pane"),
        ("unlock", "release a file lock"),
        ("locks", "list all file locks"),
        ("label", "set pane label"),
        ("panes", "list all panes"),
        ("merge", "merge forked pane's work back"),
        ("quit", "exit mush"),
    ];
    app.completions = slash_cmds
        .iter()
        .map(|(name, _)| format!("/{name}"))
        .collect();
    app.slash_commands = slash_cmds
        .iter()
        .map(|(name, desc)| crate::app::SlashCommand {
            name: name.to_string(),
            description: desc.to_string(),
        })
        .collect();
    // add prompt template names as slash commands
    let cwd = std::env::current_dir().unwrap_or_default();
    for tmpl in mush_ext::discover_templates(&cwd) {
        app.completions.push(format!("/{}", tmpl.name));
        app.slash_commands.push(crate::app::SlashCommand {
            name: tmpl.name.clone(),
            description: tmpl.description.clone(),
        });
    }
    // add model ids for /model completion
    for m in models::all_models_with_user() {
        app.completions.push(m.id.to_string());
        app.model_completions.push(crate::app::ModelCompletion {
            id: m.id.to_string(),
            name: m.name.clone(),
        });
    }

    // pull prefs out so we can mutate them without borrowing tui_config
    let mut thinking_prefs = std::mem::take(&mut tui_config.thinking_prefs);
    let thinking_saver = tui_config.save_thinking_prefs.clone();
    let mut pending_prompt: Option<String> = None;
    let mut conversation: Vec<Message> = Vec::new();

    // config hot-reload watcher
    let (_config_watcher, config_rx) = if let Some(ref path) = tui_config.config_path {
        match crate::config_watcher::watch_config(path.clone()) {
            Some((w, rx)) => (Some(w), Some(rx)),
            None => (None, None),
        }
    } else {
        (None, None)
    };

    // load initial messages (session resume)
    if !tui_config.initial_messages.is_empty() {
        for msg in &tui_config.initial_messages {
            match msg {
                Message::User(u) => {
                    let text = match &u.content {
                        UserContent::Text(t) => t.clone(),
                        UserContent::Parts(p) => p
                            .iter()
                            .filter_map(|part| match part {
                                UserContentPart::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    };
                    app.push_user_message(text);
                }
                Message::Assistant(a) => {
                    let text: String = a
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            AssistantContentPart::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    let thinking: Option<String> = a.content.iter().find_map(|p| match p {
                        AssistantContentPart::Thinking(t) => Some(t.text().to_string()),
                        _ => None,
                    });
                    app.messages.push(crate::app::DisplayMessage {
                        role: crate::app::MessageRole::Assistant,
                        content: text.trim_start_matches('\n').to_string(),
                        tool_calls: vec![],
                        thinking,
                        thinking_expanded: app.thinking_display
                            == crate::app::ThinkingDisplay::Expanded,
                        usage: Some(a.usage),
                        cost: None,
                        model_id: Some(a.model.clone()),
                        queued: false,
                    });
                    app.stats.update(&a.usage, None);
                }
                _ => {} // tool results displayed inline with their tool calls
            }
        }
        conversation = tui_config.initial_messages.clone();
        app.status = Some(format!("resumed session ({} messages)", conversation.len()));
    }

    // wrap state into pane manager (single pane initially)
    let mut initial_pane = crate::pane::Pane::new(crate::pane::PaneId::new(1), app);
    initial_pane.conversation = ConversationState::from_messages(conversation);
    let mut pane_mgr = crate::pane::PaneManager::new(initial_pane);

    // inter-pane message bus (shared across all panes)
    let message_bus = crate::messaging::MessageBus::new();
    // shared state store (shared across all panes)
    let shared_state = crate::shared_state::SharedState::new();
    // file modification tracker (shared across all panes)
    let file_tracker = crate::file_tracker::FileTracker::new(cwd.clone());
    // register the initial pane (inbox unused until multi-pane)
    let initial_inbox = message_bus.register(crate::pane::PaneId::new(1));
    pane_mgr.focused_mut().inbox = Some(initial_inbox);

    // clean up stale worktrees from previous sessions
    if matches!(
        tui_config.isolation_mode,
        crate::file_tracker::IsolationMode::Worktree
    ) {
        let cleaned = crate::isolation::cleanup_stale_worktrees(&cwd).await;
        if cleaned > 0 {
            pane_mgr.focused_mut().app.status =
                Some(format!("cleaned {cleaned} stale worktree(s)"));
        }
    }

    // draw initial frame
    draw_panes(&mut terminal, &mut pane_mgr, &image_picker)?;

    let mut agent_streams = new_agent_streams();
    let mut stream_state = StreamState::new();

    loop {
        start_pending_streams(
            &mut agent_streams,
            &mut stream_state,
            &mut pane_mgr,
            &mut pending_prompt,
            StreamDeps {
                default_model: tui_config.model.clone(),
                system_prompt: tui_config.system_prompt.clone(),
                options: tui_config.options.clone(),
                max_turns: tui_config.max_turns,
                prompt_enricher: tui_config.prompt_enricher.clone(),
                hint_mode: tui_config.hint_mode,
                provider_api_keys: tui_config.provider_api_keys.clone(),
                confirm_tools: tui_config.confirm_tools,
                auto_compact: tui_config.auto_compact,
                tools,
                registry,
                message_bus: &message_bus,
                shared_state: &shared_state,
                file_tracker: &file_tracker,
            },
        )
        .await;

        // -- main event loop: streaming or idle --
        let any_streaming = !agent_streams.is_empty();

        if any_streaming {
            let tick = tokio::time::sleep(std::time::Duration::from_millis(16));
            tokio::pin!(tick);

            tokio::select! {
                result = agent_streams.next() => {
                    if let Some((pane_id, event)) = result {
                        // skip events for panes that were aborted
                        if stream_state.is_aborted(pane_id) {
                            if matches!(event, AgentEvent::AgentEnd) {
                                stream_state.finish_aborted(pane_id);
                            }
                            continue;
                        }

                        // route agent event to the correct pane
                        if let Some(pane) = pane_mgr.pane_mut(pane_id) {
                            let model = stream_state
                                .meta(pane_id)
                                .map(|meta| &meta.model)
                                .unwrap_or(&tui_config.model);
                            let (app, conversation, image_protos) = pane.fields_mut();
                            let mut ctx = EventCtx {
                                app,
                                conversation,
                                image_protos,
                            };
                            event_handler::handle_agent_event(
                                &mut ctx,
                                &event,
                                model,
                                tui_config.debug_cache,
                                &image_picker,
                            );
                        }

                        handle_agent_event_side_effects(
                            &mut pane_mgr,
                            &mut stream_state,
                            pane_id,
                            &event,
                            &file_tracker,
                            &tui_config,
                            registry,
                        )
                        .await;
                    }
                }
                _ = tick => {
                    poll_confirmation_prompt(&mut pane_mgr, &mut stream_state).await;

                    poll_live_tool_output(&mut pane_mgr, &tui_config.tool_output_live);

                    // drain inter-pane message inboxes into steering queues
                    drain_inboxes(&mut pane_mgr).await;

                    // handle terminal input during streaming
                    while event::poll(std::time::Duration::ZERO)? {
                        match event::read()? {
                            Event::Key(key) if key.kind == KeyEventKind::Press => {
                                if pane_mgr.focused().app.mode == app::AppMode::ToolConfirm {
                                    let answer = match key.code {
                                        KeyCode::Char('y') | KeyCode::Char('Y')
                                        | KeyCode::Enter => Some(true),
                                        KeyCode::Char('n') | KeyCode::Char('N')
                                        | KeyCode::Esc => Some(false),
                                        _ => None,
                                    };
                                    if let Some(allowed) = answer {
                                        answer_confirmation(
                                            &mut pane_mgr,
                                            &mut stream_state,
                                            allowed,
                                        )
                                        .await;
                                    }
                                } else {
                                    let app_event =
                                        handle_key(&mut pane_mgr.focused_mut().app, key);
                                    if let Some(app_event) = app_event {
                                        match app_event {
                                            AppEvent::Quit => {
                                                cleanup(&mut terminal)?;
                                                terminal_guard.disarm();
                                                return Ok(());
                                            }
                                            AppEvent::Abort => {
                                                abort_focused_stream(
                                                    &mut pane_mgr,
                                                    &mut stream_state,
                                                )
                                                .await;
                                            }
                                            AppEvent::UserSubmit { text } => {
                                                submit_streaming_input(
                                                    &mut pane_mgr,
                                                    &stream_state,
                                                    text,
                                                )
                                                .await;
                                            }
                                            AppEvent::CycleThinkingLevel => {
                                                let app = &pane_mgr.focused().app;
                                                save_thinking_pref(
                                                    &mut thinking_prefs,
                                                    &thinking_saver,
                                                    &app.model_id,
                                                    app.thinking_level,
                                                );
                                            }
                                            AppEvent::PasteImage => {
                                                paste_clipboard_image(
                                                    &mut pane_mgr.focused_mut().app,
                                                )
                                                .await;
                                            }
                                            AppEvent::FocusNextPane => pane_mgr.focus_next(),
                                            AppEvent::FocusPrevPane => pane_mgr.focus_prev(),
                                            AppEvent::FocusPaneByIndex(i) => {
                                                pane_mgr.focus_index(i)
                                            }
                                            AppEvent::ResizePane(delta) => {
                                                pane_mgr.resize_focused(delta);
                                            }
                                            AppEvent::SplitPane => {
                                                fork_pane(&mut pane_mgr, &tui_config, &message_bus, &tui_config.tool_output_live).await;
                                            }
                                            AppEvent::ClosePane
                                                if pane_mgr.is_multi_pane() => {
                                                    close_focused_pane(
                                                        &mut pane_mgr,
                                                        &message_bus,
                                                        &file_tracker,
                                                        &cwd,
                                                    )
                                                    .await;
                                                }
                                            AppEvent::EditSteering => {
                                                edit_last_queued_steering(
                                                    &mut pane_mgr,
                                                    &stream_state,
                                                )
                                                .await;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            Event::Mouse(mouse) => {
                                handle_mouse(&mut pane_mgr.focused_mut().app, mouse)
                            }
                            Event::Key(key) => {
                                tracing::trace!(code = ?key.code, kind = ?key.kind, "dropped non-press key event (streaming)");
                            }
                            event => {
                                tracing::trace!(?event, "dropped non-key event (streaming)");
                            }
                        }
                    }
                }
            }
        } else {
            // idle: no active streams, wait for terminal input
            // check for inter-pane messages that should auto-wake idle panes
            drain_inboxes(&mut pane_mgr).await;
            // start agent loops for any pane that received a message while idle
            for pane in pane_mgr.panes_mut() {
                if !pane.app.is_streaming && pane.pending_prompt.is_some() {
                    // pending_prompt was set by drain_inboxes, will be picked up
                    // at the top of the loop
                }
            }

            if event::poll(std::time::Duration::from_millis(50))? {
                loop {
                    match event::read()? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            let app_event = handle_key(&mut pane_mgr.focused_mut().app, key);
                            if let Some(app_event) = app_event {
                                match app_event {
                                    AppEvent::Quit => {
                                        pane_mgr.focused_mut().app.should_quit = true;
                                        break;
                                    }
                                    AppEvent::UserSubmit { text } => {
                                        let expanded = slash::expand_template(&text);
                                        // auto-label pane from first prompt
                                        let pane = pane_mgr.focused_mut();
                                        if pane.label.is_none() && pane.conversation.is_empty() {
                                            pane.label = Some(expanded.chars().take(30).collect());
                                        }
                                        let app = &mut pane_mgr.focused_mut().app;
                                        app.push_user_message(expanded.clone());
                                        app.active_tools.clear();
                                        app.start_streaming();
                                        pending_prompt = Some(expanded);
                                    }
                                    AppEvent::SlashCommand { action } => {
                                        let state_changed = handle_slash_action(
                                            &mut pane_mgr,
                                            action,
                                            SlashEnv {
                                                tui_config: &mut tui_config,
                                                thinking_prefs: &thinking_prefs,
                                                registry,
                                                message_bus: &message_bus,
                                                file_tracker: &file_tracker,
                                                cwd: &cwd,
                                                pending_prompt: &mut pending_prompt,
                                            },
                                        )
                                        .await;

                                        if state_changed
                                            && let Some(ref saver) = tui_config.save_session
                                        {
                                            let pane = pane_mgr.focused();
                                            saver(&pane.conversation, &pane.app.model_id);
                                        }
                                    }
                                    AppEvent::CycleThinkingLevel => {
                                        let app = &pane_mgr.focused().app;
                                        save_thinking_pref(
                                            &mut thinking_prefs,
                                            &thinking_saver,
                                            &app.model_id,
                                            app.thinking_level,
                                        );
                                    }
                                    AppEvent::PasteImage => {
                                        paste_clipboard_image(&mut pane_mgr.focused_mut().app)
                                            .await;
                                    }
                                    AppEvent::SplitPane => {
                                        fork_pane(
                                            &mut pane_mgr,
                                            &tui_config,
                                            &message_bus,
                                            &tui_config.tool_output_live,
                                        )
                                        .await;
                                    }
                                    AppEvent::ClosePane => {
                                        close_focused_pane(
                                            &mut pane_mgr,
                                            &message_bus,
                                            &file_tracker,
                                            &cwd,
                                        )
                                        .await;
                                    }
                                    AppEvent::FocusNextPane => pane_mgr.focus_next(),
                                    AppEvent::FocusPrevPane => pane_mgr.focus_prev(),
                                    AppEvent::FocusPaneByIndex(i) => pane_mgr.focus_index(i),
                                    AppEvent::ResizePane(delta) => {
                                        pane_mgr.resize_focused(delta);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Event::Mouse(mouse) => handle_mouse(&mut pane_mgr.focused_mut().app, mouse),
                        Event::Key(key) => {
                            tracing::trace!(code = ?key.code, kind = ?key.kind, "dropped non-press key event (idle)");
                        }
                        event => {
                            tracing::trace!(?event, "dropped non-key event (idle)");
                        }
                    }

                    if !event::poll(std::time::Duration::ZERO)? {
                        break;
                    }
                }
            }
        }

        // config hot-reload
        if let Some(ref rx) = config_rx
            && let Ok(new_theme) = rx.try_recv()
        {
            tui_config.theme = new_theme;
            pane_mgr.focused_mut().app.status = Some("config reloaded".into());
        }

        if pane_mgr.focused().app.should_quit {
            break;
        }

        // tick streaming panes and draw
        for pane in pane_mgr.panes_mut() {
            if pane.app.is_streaming {
                pane.app.tick();
            }
        }

        // cache warmth notifications
        if tui_config.cache_timer {
            for pane in pane_mgr.panes_mut() {
                if let Some(remaining) = pane.app.cache_remaining_secs() {
                    if remaining == 0 && !pane.app.cache_expired_sent {
                        pane.app.cache_expired_sent = true;
                        crate::notify::send_with_sound(
                            "cache expired",
                            "prompt cache has gone cold",
                            Some(crate::notify::Sound::Attention),
                        );
                    } else if remaining > 0 && remaining <= 60 && !pane.app.cache_warn_sent {
                        pane.app.cache_warn_sent = true;
                        crate::notify::send_with_sound(
                            "cache expiring soon",
                            &format!("prompt cache expires in {remaining}s"),
                            Some(crate::notify::Sound::Attention),
                        );
                    }
                }
            }
        }

        draw_panes(&mut terminal, &mut pane_mgr, &image_picker)?;
    }

    cleanup(&mut terminal)?;
    terminal_guard.disarm();
    Ok(())
}

/// read a clipboard image in a background thread and add it to the app
async fn paste_clipboard_image(app: &mut App) {
    app.status = Some("reading clipboard...".into());
    match tokio::task::spawn_blocking(crate::clipboard::read_clipboard_image).await {
        Ok(Some(image)) => {
            let mime = image.mime_type.as_str();
            let size = image.bytes.len();
            app.add_image(image);
            let n = app.pending_images.len();
            let kb = size / 1024;
            app.status = Some(format!("{n} image(s) attached ({mime}, {kb}kb)"));
        }
        Ok(None) => {
            app.status = Some("no image in clipboard".into());
        }
        Err(_) => {
            app.status = Some("failed to read clipboard".into());
        }
    }
}
