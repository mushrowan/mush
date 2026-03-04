//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;
use std::sync::Arc;

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, Event, KeyCode, KeyboardEnhancementFlags, MouseEvent, MouseEventKind,
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
use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;
use mush_session::tree::SessionTree;

use crate::app::{self, App, AppEvent};
use crate::input::handle_key;
use crate::slash;
use crate::ui::Ui;
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
    /// callback to auto-save session after each agent turn
    pub save_session: Option<SessionSaver>,
    /// prompt for confirmation before executing tools (off by default)
    pub confirm_tools: bool,
    /// emit system messages when cache reads are observed
    pub debug_cache: bool,
    /// shared live tool output (updated by bash sink, read by TUI)
    pub tool_output_live: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
}

/// run the interactive TUI
pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &[Box<dyn AgentTool>],
    registry: &ApiRegistry,
) -> io::Result<()> {
    // detect image protocol before entering alternate screen to avoid probe artifacts
    let image_picker = ratatui_image::picker::Picker::from_query_stdio().ok();

    // set up terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(crossterm::event::EnableMouseCapture)?;
    // enable kitty keyboard protocol so shift+enter is distinguishable
    let _ = io::stdout().execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    ));
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(tui_config.model.id.clone(), tui_config.model.context_window);
    app.thinking_level = tui_config.options.thinking.unwrap_or(ThinkingLevel::Off);
    // populate tab completions
    let slash_cmds = [
        "/help",
        "/clear",
        "/model",
        "/sessions",
        "/branch",
        "/tree",
        "/compact",
        "/export",
        "/undo",
        "/search",
        "/cost",
        "/injection",
        "/quit",
    ];
    app.completions = slash_cmds.iter().map(|s| s.to_string()).collect();
    // add prompt template names as slash commands
    let cwd = std::env::current_dir().unwrap_or_default();
    for tmpl in mush_ext::discover_templates(&cwd) {
        app.completions.push(format!("/{}", tmpl.name));
    }
    // add model ids for /model completion
    for m in models::all_models_with_user() {
        app.completions.push(m.id.to_string());
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
                        thinking_expanded: false,
                        usage: Some(a.usage),
                        cost: None,
                        model_id: Some(a.model.clone()),
                    });
                    app.total_tokens += a.usage.total_tokens();
                    app.total_input_tokens += a.usage.input_tokens;
                    app.total_output_tokens += a.usage.output_tokens;
                    app.total_cache_read_tokens += a.usage.cache_read_tokens;
                    app.total_cache_write_tokens += a.usage.cache_write_tokens;
                    app.context_tokens = a.usage.total_input_tokens();
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
                content: UserContent::Text(user_text),
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
                    let mut msgs =
                        auto_compact(msgs, context_window, registry, &model, &options).await;
                    if let Some(ref enricher) = enricher {
                        inject_hint(&mut msgs, enricher.as_ref());
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
            let (api_key, account_id) =
                resolve_auth_for_model(&tui_config.model, &tui_config.provider_api_keys).await;
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
                                handle_agent_event(
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
                                Event::Key(key) => {
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
                                                break;
                                            }
                                            AppEvent::UserSubmit { text } => {
                                                let msg = Message::User(UserMessage {
                                                    content: UserContent::Text(text.clone()),
                                                    timestamp_ms: Timestamp::now(),
                                                });
                                                steering_queue.lock().await.push(msg);
                                                app.push_user_message(text);
                                                app.status = Some("steering message queued".into());
                                            }
                                            AppEvent::CycleThinkingLevel => {
                                                tui_config.options.thinking = Some(app.thinking_level);
                                                save_thinking_pref(&mut thinking_prefs, &thinking_saver, &app.model_id, app.thinking_level);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                                _ => {}
                            }
                        }
                    }
                }

                app.tick();
                draw(&mut terminal, &app, &mut image_protos)?;
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
                    Event::Key(key) => {
                        if let Some(app_event) = handle_key(&mut app, key) {
                            match app_event {
                                AppEvent::Quit => {
                                    app.should_quit = true;
                                    break;
                                }
                                AppEvent::UserSubmit { text } => {
                                    let expanded = slash::expand_template(&text);
                                    app.push_user_message(expanded.clone());
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
                                _ => {}
                            }
                        }
                    }
                    Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                    _ => {}
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

/// mutable state shared across the event loop
struct EventCtx<'a> {
    app: &'a mut App,
    conversation: &'a mut Vec<Message>,
    session_tree: &'a mut SessionTree,
    image_protos: &'a mut std::collections::HashMap<
        (usize, usize),
        ratatui_image::protocol::StatefulProtocol,
    >,
}

fn handle_agent_event(
    ctx: &mut EventCtx<'_>,
    event: &AgentEvent,
    model: &Model,
    debug_cache: bool,
    image_picker: &Option<ratatui_image::picker::Picker>,
) {
    let EventCtx {
        app,
        conversation,
        session_tree,
        image_protos,
    } = ctx;
    match event {
        AgentEvent::StreamEvent { event } => match event {
            StreamEvent::TextDelta { delta, .. } => app.push_text_delta(delta),
            StreamEvent::ThinkingDelta { delta, .. } => app.push_thinking_delta(delta),
            StreamEvent::ToolCallDelta { delta, .. } => app.push_tool_args_delta(delta),
            _ => {}
        },
        AgentEvent::MessageEnd { message } => {
            let cost = models::calculate_cost(model, &message.usage);
            app.finish_streaming(Some(message.usage), Some(cost.total()));
            if debug_cache && message.usage.cache_read_tokens > 0 {
                app.push_system_message(format!(
                    "cache read detected: {} tokens",
                    message.usage.cache_read_tokens
                ));
            }
            let msg = Message::Assistant(message.clone());
            session_tree.append_message(msg.clone());
            conversation.push(msg);
        }
        AgentEvent::ToolExecStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            let summary = summarise_tool_args(tool_name.as_str(), args);
            app.start_tool(tool_call_id.as_str(), tool_name.as_str(), &summary);
        }
        AgentEvent::ToolExecEnd {
            tool_call_id,
            tool_name,
            result,
        } => {
            let output_text = result.content.iter().find_map(|p| match p {
                ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            });
            // extract image data from tool result (base64 → raw bytes)
            let image_data = result.content.iter().find_map(|p| match p {
                ToolResultContentPart::Image(img) => {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .decode(&img.data)
                        .ok()
                }
                _ => None,
            });
            // create image protocol for inline rendering
            if let Some(ref data) = image_data
                && let Some(picker) = image_picker
                && let Ok(dyn_image) = image::load_from_memory(data)
            {
                let msg_idx = app.messages.len().saturating_sub(1);
                let tc_idx = app.messages.last().map(|m| m.tool_calls.len()).unwrap_or(0);
                let proto = picker.new_resize_protocol(dyn_image);
                image_protos.insert((msg_idx, tc_idx), proto);
            }
            app.end_tool(
                tool_call_id.as_str(),
                tool_name.as_str(),
                result.outcome,
                output_text,
                image_data,
            );
            session_tree.append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                content: result.content.clone(),
                outcome: result.outcome,
                timestamp_ms: Timestamp::now(),
            }));
        }
        AgentEvent::TurnStart { .. } if !app.is_streaming => {
            app.start_streaming();
        }
        AgentEvent::SteeringInjected { count } => {
            app.status = Some(format!("steering: {count} messages injected"));
        }
        AgentEvent::FollowUpInjected { count } => {
            app.status = Some(format!("follow-up: {count} messages queued"));
        }
        AgentEvent::ContextTransformed {
            before_count,
            after_count,
        } => {
            app.status = Some(format!(
                "compacted: {before_count} → {after_count} messages"
            ));
        }
        AgentEvent::MaxTurnsReached { max_turns } => {
            app.is_streaming = false;
            app.status = Some(format!("hit max turns limit ({max_turns})"));
        }
        AgentEvent::Error { error } => {
            app.is_streaming = false;
            app.status = Some(format!("error: {error}"));
        }
        AgentEvent::AgentEnd => {
            app.is_streaming = false;
        }
        _ => {}
    }
}

/// inject a relevance hint into the last user message.
/// operates on a clone of messages so the original conversation is untouched.
fn inject_hint(msgs: &mut [Message], enricher: &(dyn Fn(&str) -> Option<String> + Send + Sync)) {
    // find the last user message
    let Some(pos) = msgs.iter().rposition(|m| matches!(m, Message::User(_))) else {
        return;
    };

    let Message::User(ref user_msg) = msgs[pos] else {
        return;
    };

    let text = match &user_msg.content {
        UserContent::Text(t) => t.clone(),
        UserContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                UserContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    };

    if let Some(hint) = enricher(&text) {
        msgs[pos] = Message::User(UserMessage {
            content: UserContent::Text(format!("{hint}\n\n{text}")),
            timestamp_ms: user_msg.timestamp_ms,
        });
    }
}

async fn resolve_auth_for_model(
    model: &Model,
    provider_api_keys: &std::collections::HashMap<String, String>,
) -> (Option<ApiKey>, Option<String>) {
    if let Some(key) = mush_ai::env::env_api_key(&model.provider) {
        return (Some(key), None);
    }

    let provider_name = model.provider.to_string();
    if let Some(key) = provider_api_keys.get(&provider_name) {
        return (ApiKey::new(key.clone()), None);
    }

    match &model.provider {
        Provider::Anthropic => {
            let token = mush_ai::oauth::get_oauth_token("anthropic")
                .await
                .ok()
                .flatten()
                .and_then(ApiKey::new);
            (token, None)
        }
        Provider::Custom(name) if name == "openai-codex" => {
            let token = mush_ai::oauth::get_oauth_token("openai-codex")
                .await
                .ok()
                .flatten()
                .and_then(ApiKey::new);
            let account_id = oauth_account_id("openai-codex");
            (token, account_id)
        }
        _ => (None, None),
    }
}

fn oauth_account_id(provider_id: &str) -> Option<String> {
    mush_ai::oauth::load_credentials().ok().and_then(|store| {
        store
            .providers
            .get(provider_id)
            .and_then(|c| c.account_id.clone())
    })
}

async fn auto_compact(
    messages: Vec<Message>,
    context_window: usize,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
) -> Vec<Message> {
    use mush_session::compact;

    if !compact::needs_compaction(&messages, context_window) {
        return messages;
    }

    compact::llm_compact(messages, registry, model, options, Some(10))
        .await
        .messages
}

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
        if app.mode == app::AppMode::Normal
            || (app.is_streaming && app.mode != app::AppMode::ToolConfirm)
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
    })?;
    Ok(())
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
            app.scroll_offset = app.scroll_offset.saturating_add(SCROLL_LINES);
        }
        MouseEventKind::ScrollDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(SCROLL_LINES);
            if app.scroll_offset == 0 {
                app.has_unread = false;
            }
        }
        _ => {}
    }
}

fn cleanup(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    disable_raw_mode()?;
    io::stdout().execute(crossterm::event::DisableMouseCapture)?;
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
