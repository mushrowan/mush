//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;
use std::sync::Arc;

use crossterm::ExecutableCommand;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::Mutex;

use mush_agent::tool::AgentTool;
use mush_agent::{AgentConfig, AgentEvent, agent_loop};
use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_session::tree::SessionTree;

use crate::app::{self, App, AppEvent};
use crate::event_handler::{self, EventCtx};
use crate::input::handle_key;
use crate::slash;
use crate::widgets;

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

/// callback to persist session state (messages + tree + model_id)
pub type SessionSaver = std::sync::Arc<dyn Fn(&[Message], &SessionTree, &str) + Send + Sync>;

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
    /// prompt for confirmation before executing tools (off by default)
    pub confirm_tools: bool,
    /// show dollar cost in status bar (off by default, toggle with /cost)
    pub show_cost: bool,
    /// emit system messages when cache reads are observed
    pub debug_cache: bool,
    /// how to display thinking text (hidden, collapse, expanded)
    pub thinking_display: crate::app::ThinkingDisplay,
    /// shared live tool output (updated by bash sink, read by TUI)
    pub tool_output_live: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// callback to get recent log entries (returns last N lines)
    pub log_buffer: Option<std::sync::Arc<dyn Fn(usize) -> Vec<String> + Send + Sync>>,
}

/// run the interactive TUI
pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &[Box<dyn AgentTool>],
    registry: &ApiRegistry,
) -> io::Result<()> {
    // reset terminal state in case prior output (e.g. model download progress
    // bars from fastembed/indicatif) left it dirty
    let _ = io::stdout().execute(crossterm::cursor::Show);
    let _ = io::stdout().execute(crossterm::style::ResetColor);
    {
        use std::io::Write;
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
    }

    // detect image protocol before entering alternate screen to avoid probe artifacts
    let image_picker = ratatui_image::picker::Picker::from_query_stdio().ok();

    // set up terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    enable_mouse_scroll()?;
    // enable kitty keyboard protocol so shift+enter is distinguishable
    let _ = io::stdout().execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    ));
    let _ = io::stdout().execute(SetCursorStyle::BlinkingBar);

    // drain any stale input from terminal probes (e.g. device attribute
    // responses from the image picker that arrived after the probe finished)
    std::thread::sleep(std::time::Duration::from_millis(50));
    while event::poll(std::time::Duration::ZERO)? {
        let stale = event::read()?;
        tracing::debug!(?stale, "drained stale event from terminal probe");
    }

    // install panic hook that restores the terminal so a crash doesn't leave
    // the user's shell in raw mode / alternate screen
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
        let _ = io::stdout().execute(SetCursorStyle::DefaultUserShape);
        let _ = disable_raw_mode();
        disable_mouse_scroll();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = crossterm::cursor::Show;
        prev_hook(info);
    }));

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(tui_config.model.id.clone(), tui_config.model.context_window);
    app.thinking_level = tui_config.options.thinking.unwrap_or(ThinkingLevel::Off);
    app.thinking_display = tui_config.thinking_display;
    app.show_cost = tui_config.show_cost;
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
    let mut session_tree = SessionTree::new();
    // image protocol states keyed by (message_idx, tool_call_idx)
    let mut image_protos: std::collections::HashMap<
        (usize, usize),
        ratatui_image::protocol::StatefulProtocol,
    > = std::collections::HashMap::new();

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

    // shared steering queue: user can type while agent is running
    let steering_queue: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));

    // draw initial frame
    draw(&mut terminal, &app, &mut image_protos)?;

    loop {
        // if there's a pending prompt and we're not streaming, start the agent
        if let Some(prompt) = pending_prompt.take() {
            let prompt_preview = prompt.clone();

            // in Message mode, prepend hint to user message (evaluated once)
            let mut injection_preview: Option<String> = None;
            let user_text = if tui_config.hint_mode == HintMode::Message
                && let Some(ref enricher) = tui_config.prompt_enricher
                && let Some(hint) = enricher(&prompt)
            {
                if app.show_prompt_injection {
                    injection_preview = Some(format!("message hint\n{hint}"));
                }
                format!("{hint}\n\n{prompt}")
            } else {
                prompt
            };

            if app.show_prompt_injection
                && tui_config.hint_mode == HintMode::Transform
                && let Some(ref enricher) = tui_config.prompt_enricher
                && let Some(hint) = enricher(&prompt_preview)
            {
                injection_preview = Some(format!(
                    "transform hint\n{hint}\n\n(applied before each llm call)"
                ));
            }

            if app.show_prompt_injection {
                if let Some(preview) = injection_preview {
                    app.push_system_message(preview);
                } else {
                    let note = match tui_config.hint_mode {
                        HintMode::None => "no injection (hint mode is none)",
                        _ if tui_config.prompt_enricher.is_none() => {
                            "no injection (enricher unavailable)"
                        }
                        _ => "no injection hint matched",
                    };
                    app.push_system_message(note);
                }
            }

            let user_message = Message::User(UserMessage {
                content: if app.pending_images.is_empty() {
                    UserContent::Text(user_text)
                } else {
                    let images = app.take_images();
                    let mut parts: Vec<UserContentPart> =
                        vec![UserContentPart::Text(TextContent { text: user_text })];
                    for img in images {
                        use base64::Engine;
                        parts.push(UserContentPart::Image(ImageContent {
                            data: base64::engine::general_purpose::STANDARD.encode(&img.data),
                            mime_type: img.mime_type,
                        }));
                    }
                    UserContent::Parts(parts)
                },
                timestamp_ms: Timestamp::now(),
            });
            session_tree.append_message(user_message.clone());
            conversation.push(user_message);

            // in Transform mode, the hint is injected before each LLM call
            let context_window = tui_config.model.context_window as usize;
            let enricher_arc = if tui_config.hint_mode == HintMode::Transform {
                tui_config.prompt_enricher.clone()
            } else {
                None
            };
            let compact_model = tui_config.model.clone();
            let compact_options = tui_config.options.clone();
            let transform: Option<mush_agent::ContextTransform<'_>> = Some(Box::new(move |msgs| {
                let enricher = enricher_arc.clone();
                let model = compact_model.clone();
                let options = compact_options.clone();
                Box::pin(async move {
                    let mut msgs = event_handler::auto_compact(
                        msgs,
                        context_window,
                        registry,
                        &model,
                        &options,
                    )
                    .await;
                    if let Some(ref enricher) = enricher {
                        event_handler::inject_hint(&mut msgs, enricher.as_ref());
                    }
                    msgs
                })
            }));

            // steering callback: drains any messages queued by user input
            let sq = steering_queue.clone();
            let steering: Option<mush_agent::MessageCallback<'_>> = Some(Box::new(move || {
                let sq = sq.clone();
                Box::pin(async move {
                    let mut q = sq.lock().await;
                    q.drain(..).collect()
                })
            }));

            // follow-up callback: picks up any remaining queued messages after agent finishes
            let sq_follow = steering_queue.clone();
            let follow_up: Option<mush_agent::MessageCallback<'_>> = Some(Box::new(move || {
                let sq = sq_follow.clone();
                Box::pin(async move {
                    let mut q = sq.lock().await;
                    q.drain(..).collect()
                })
            }));
            type ConfirmRequest = (String, tokio::sync::oneshot::Sender<bool>);
            let (confirm_req_tx, mut confirm_req_rx) =
                tokio::sync::mpsc::channel::<ConfirmRequest>(1);

            let confirm: Option<mush_agent::ConfirmCallback<'_>> = if tui_config.confirm_tools {
                Some(Box::new(move |name: &str, args: &serde_json::Value| {
                    let tx = confirm_req_tx.clone();
                    let summary = mush_agent::summarise_tool_args(name, args);
                    let prompt = format!("{name} {summary}");
                    Box::pin(async move {
                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                        if tx.send((prompt, resp_tx)).await.is_err() {
                            return mush_agent::ConfirmAction::Allow;
                        }
                        match resp_rx.await {
                            Ok(true) => mush_agent::ConfirmAction::Allow,
                            _ => mush_agent::ConfirmAction::Deny,
                        }
                    })
                }))
            } else {
                None
            };
            // stash receiver for the tick handler to pick up pending confirms
            let confirm_reply: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>> =
                Arc::new(Mutex::new(None));

            let mut call_options = tui_config.options.clone();
            let (api_key, account_id) = event_handler::resolve_auth_for_model(
                &tui_config.model,
                &tui_config.provider_api_keys,
            )
            .await;
            call_options.api_key = api_key;
            call_options.account_id = account_id;

            let config = AgentConfig {
                model: &tui_config.model,
                system_prompt: tui_config.system_prompt.clone(),
                tools,
                registry,
                options: call_options,
                max_turns: tui_config.max_turns,
                get_steering: steering,
                get_follow_up: follow_up,
                transform_context: transform,
                confirm_tool: confirm,
            };

            let mut stream = std::pin::pin!(agent_loop(config, conversation.clone()));

            let mut aborted = false;

            // inner loop: process agent events while also handling terminal input
            loop {
                let tick = tokio::time::sleep(std::time::Duration::from_millis(16));
                tokio::pin!(tick);

                tokio::select! {
                    agent_event = stream.next() => {
                        match agent_event {
                            Some(event) => {
                                let mut ctx = EventCtx {
                                    app: &mut app,
                                    conversation: &mut conversation,
                                    session_tree: &mut session_tree,
                                    image_protos: &mut image_protos,
                                };
                                event_handler::handle_agent_event(
                                    &mut ctx,
                                    &event,
                                    &tui_config.model,
                                    tui_config.debug_cache,
                                    &image_picker,
                                );
                                if matches!(event, AgentEvent::AgentEnd) {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = tick => {
                        // check for pending tool confirmation requests
                        if let Ok((prompt, reply_tx)) = confirm_req_rx.try_recv() {
                            app.mode = app::AppMode::ToolConfirm;
                            app.confirm_prompt = Some(prompt);
                            *confirm_reply.lock().await = Some(reply_tx);
                        }

                        // poll live tool output from bash sink
                        if let Some(ref live) = tui_config.tool_output_live
                            && let Ok(guard) = live.lock()
                            && let Some(last) = guard.as_ref()
                            && let Some(active) = app.active_tools.last().map(|t| t.tool_call_id.clone())
                        {
                            app.push_tool_output(active.as_str(), last);
                        }

                        // check for terminal input during streaming
                        while event::poll(std::time::Duration::ZERO)? {
                            match event::read()? {
                                Event::Key(key) if key.kind == KeyEventKind::Press => {
                                    // handle tool confirmation y/n
                                    if app.mode == app::AppMode::ToolConfirm {
                                        let answer = match key.code {
                                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(true),
                                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(false),
                                            _ => None,
                                        };
                                        if let Some(allowed) = answer {
                                            if let Some(tx) = confirm_reply.lock().await.take() {
                                                let _ = tx.send(allowed);
                                            }
                                            app.mode = app::AppMode::Normal;
                                            app.confirm_prompt = None;
                                            if !allowed {
                                                app.status = Some("tool denied".into());
                                            }
                                        }
                                    } else if let Some(app_event) = handle_key(&mut app, key) {
                                        match app_event {
                                            AppEvent::Quit => {
                                                cleanup(&mut terminal)?;
                                                return Ok(());
                                            }
                                            AppEvent::Abort => {
                                                app.is_streaming = false;
                                                app.active_tools.clear();
                                                app.status = Some("aborted".into());
                                                aborted = true;
                                                break;
                                            }
                                            AppEvent::UserSubmit { text } => {
                                                let msg = Message::User(UserMessage {
                                                    content: UserContent::Text(text.clone()),
                                                    timestamp_ms: Timestamp::now(),
                                                });
                                                steering_queue.lock().await.push(msg);
                                                app.push_queued_message(text);
                                            }
                                            AppEvent::CycleThinkingLevel => {
                                                tui_config.options.thinking = Some(app.thinking_level);
                                                save_thinking_pref(&mut thinking_prefs, &thinking_saver, &app.model_id, app.thinking_level);
                                            }
                                            AppEvent::PasteImage => {
                                                paste_clipboard_image(&mut app).await;
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
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

                app.tick();
                draw(&mut terminal, &app, &mut image_protos)?;

                if aborted {
                    // drop the stream, cancelling any in-flight agent work
                    break;
                }
            }

            // auto-save after each agent turn
            if let Some(ref saver) = tui_config.save_session {
                saver(&conversation, &session_tree, &app.model_id);
            }

            // auto-compact when approaching context limit
            if mush_session::compact::needs_compaction(
                &conversation,
                tui_config.model.context_window as usize,
            ) {
                app.status = Some("auto-compacting…".into());
                draw(&mut terminal, &app, &mut image_protos)?;

                slash::handle_compact(
                    &mut app,
                    &mut conversation,
                    &mut session_tree,
                    &tui_config,
                    registry,
                )
                .await;
                // save again after compaction
                if let Some(ref saver) = tui_config.save_session {
                    saver(&conversation, &session_tree, &app.model_id);
                }
            }

            draw(&mut terminal, &app, &mut image_protos)?;
            continue;
        }

        // idle: wait for terminal input
        if event::poll(std::time::Duration::from_millis(50))? {
            loop {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(app_event) = handle_key(&mut app, key) {
                            match app_event {
                                AppEvent::Quit => {
                                    app.should_quit = true;
                                    break;
                                }
                                AppEvent::UserSubmit { text } => {
                                    let expanded = slash::expand_template(&text);
                                    app.push_user_message(expanded.clone());
                                    app.active_tools.clear();
                                    app.start_streaming();
                                    pending_prompt = Some(expanded);
                                }
                                AppEvent::SlashCommand { name, args } => {
                                    if name == "search" {
                                        app.mode = app::AppMode::Search;
                                        app.search.query = args.to_string();
                                        app.update_search();
                                    } else if name == "compact" {
                                        slash::handle_compact(
                                            &mut app,
                                            &mut conversation,
                                            &mut session_tree,
                                            &tui_config,
                                            registry,
                                        )
                                        .await;
                                    } else if name == "export" {
                                        slash::handle_export(&mut app, &conversation, &args);
                                    } else if let Some(prompt) = slash::handle(
                                        &mut app,
                                        &mut conversation,
                                        &mut session_tree,
                                        &mut tui_config,
                                        &thinking_prefs,
                                        &name,
                                        &args,
                                    ) {
                                        app.start_streaming();
                                        pending_prompt = Some(prompt);
                                    }
                                }
                                AppEvent::CycleThinkingLevel => {
                                    tui_config.options.thinking = Some(app.thinking_level);
                                    save_thinking_pref(
                                        &mut thinking_prefs,
                                        &thinking_saver,
                                        &app.model_id,
                                        app.thinking_level,
                                    );
                                }
                                AppEvent::PasteImage => {
                                    paste_clipboard_image(&mut app).await;
                                }
                                _ => {}
                            }
                        }
                    }
                    Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
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

        // check for config hot-reload
        if let Some(ref rx) = config_rx
            && let Ok(new_theme) = rx.try_recv()
        {
            tui_config.theme = new_theme;
            app.status = Some("config reloaded".into());
        }

        if app.should_quit {
            break;
        }

        draw(&mut terminal, &app, &mut image_protos)?;
    }

    cleanup(&mut terminal)?;
    Ok(())
}

use crate::ui::Ui;

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &App,
    image_protos: &mut std::collections::HashMap<
        (usize, usize),
        ratatui_image::protocol::StatefulProtocol,
    >,
) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let ui = Ui::new(app);
        let (cx, cy) = ui.cursor_position(area);
        frame.render_widget(ui, area);
        let streaming_idle = app.is_busy() && app.input.is_empty();
        if !streaming_idle
            && (app.mode == app::AppMode::Normal
                || app.mode == app::AppMode::SlashComplete
                || (app.is_streaming && app.mode != app::AppMode::ToolConfirm))
        {
            frame.set_cursor_position((cx, cy));
        }
        // render inline images at positions computed by MessageList
        let render_areas = app.image_render_areas.borrow().clone();
        for img_area in &render_areas {
            if let Some(proto) = image_protos.get_mut(&(img_area.msg_idx, img_area.tc_idx)) {
                let widget =
                    ratatui_image::StatefulImage::new().resize(ratatui_image::Resize::Fit(None));
                frame.render_stateful_widget(widget, img_area.area, proto);
            }
        }
        // session picker overlay
        if let Some(ref picker) = app.session_picker {
            widgets::session_picker::render(frame, picker);
        }
        // slash command menu (above input box)
        if let Some(ref menu) = app.slash_menu {
            let input_h = crate::ui::input_height(&app.input, area.width, &app.pending_images);
            let tools_h =
                crate::widgets::tool_panels::tool_panels_height(&app.active_tools, area.width);
            let status_h = crate::widgets::status_bar::status_bar_height(app, area.width);
            let regions = crate::ui::layout(area, input_h, tools_h, status_h);
            widgets::slash_menu::render(frame, menu, regions.input);
        }
    })?;
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

/// persist a thinking level change for the current model
fn save_thinking_pref(
    prefs: &mut std::collections::HashMap<String, ThinkingLevel>,
    saver: &Option<ThinkingPrefsSaver>,
    model_id: &str,
    level: ThinkingLevel,
) {
    prefs.insert(model_id.to_string(), level);
    if let Some(saver) = saver {
        saver(prefs);
    }
}

/// handle mouse scroll events
fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    const SCROLL_LINES: u16 = 3;
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.is_mouse_over_input(mouse.column, mouse.row) {
                app.scroll_input_by(-(SCROLL_LINES as i16));
            } else {
                app.scroll_offset = app.scroll_offset.saturating_add(SCROLL_LINES);
            }
        }
        MouseEventKind::ScrollDown => {
            if app.is_mouse_over_input(mouse.column, mouse.row) {
                app.scroll_input_by(SCROLL_LINES as i16);
            } else {
                app.scroll_offset = app.scroll_offset.saturating_sub(SCROLL_LINES);
                if app.scroll_offset == 0 {
                    app.has_unread = false;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_scroll_over_messages_scrolls_conversation() {
        let mut app = App::new("test".into(), 200_000);
        app.input_area.set(ratatui::layout::Rect::new(0, 10, 40, 5));
        let before = app.scroll_offset;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert!(app.scroll_offset > before);
    }

    #[test]
    fn mouse_scroll_over_input_scrolls_input() {
        let mut app = App::new("test".into(), 200_000);
        app.input_area.set(ratatui::layout::Rect::new(0, 10, 40, 5));
        app.input_visible_lines.set(2);
        app.input_total_lines.set(8);
        app.input_scroll.set(2);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 11,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.input_scroll.get(), 0);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 1,
                row: 11,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );
        assert_eq!(app.input_scroll.get(), 3);
    }
}

/// enable minimal mouse tracking: clicks + scroll with SGR coordinates
///
/// crossterm's `EnableMouseCapture` also enables `?1003h` (any-event tracking)
/// which floods the event stream with movement events. when events accumulate
/// faster than the TUI polls them, SGR escape sequence fragments can leak
/// through crossterm's parser as spurious key events, causing garbled text
fn enable_mouse_scroll() -> io::Result<()> {
    use std::io::Write;
    // ?1000h = normal tracking (press/release/scroll)
    // ?1006h = SGR extended coordinates
    io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h")?;
    io::stdout().flush()
}

fn disable_mouse_scroll() {
    use std::io::Write;
    let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1006l");
    let _ = io::stdout().flush();
}

fn cleanup(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    let _ = io::stdout().execute(SetCursorStyle::DefaultUserShape);
    disable_raw_mode()?;
    disable_mouse_scroll();
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
