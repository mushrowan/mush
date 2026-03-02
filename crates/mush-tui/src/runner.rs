//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;
use std::sync::Arc;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, MouseEvent, MouseEventKind};
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
use crate::ui::Ui;
use crate::widgets;

/// callback that returns a relevance hint for a user message.
/// used to nudge the model toward the most relevant skills.
/// wrapped in Arc so it can be shared with context transform closures.
pub type PromptEnricher = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// how to inject skill relevance hints into the conversation
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HintMode {
    /// prepend hint to user message (evaluated once per message)
    #[default]
    Message,
    /// inject via context transform (re-evaluated before each LLM call)
    Transform,
    /// no hint (all skills still loaded in system prompt)
    None,
}

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
}

/// run the interactive TUI
pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &[Box<dyn AgentTool>],
    registry: &ApiRegistry,
) -> io::Result<()> {
    // set up terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    io::stdout().execute(crossterm::event::EnableMouseCapture)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // detect terminal image protocol (must be after entering alternate screen)
    let image_picker = ratatui_image::picker::Picker::from_query_stdio().ok();
    if let Some(ref p) = image_picker {
        eprintln!("\x1b[2mimage: {:?}\x1b[0m", p.protocol_type());
    }

    let mut app = App::new(tui_config.model.id.0.clone());
    app.thinking_level = tui_config
        .options
        .thinking
        .unwrap_or(ThinkingLevel::Off);
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
                        AssistantContentPart::Thinking(t) => Some(t.thinking.clone()),
                        _ => None,
                    });
                    app.messages.push(crate::app::DisplayMessage {
                        role: crate::app::MessageRole::Assistant,
                        content: text,
                        tool_calls: vec![],
                        thinking,
                        thinking_expanded: false,
                        usage: Some(a.usage),
                        cost: None,
                    });
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
            // in Message mode, prepend hint to user message (evaluated once)
            let user_text = if tui_config.hint_mode == HintMode::Message
                && let Some(ref enricher) = tui_config.prompt_enricher
                && let Some(hint) = enricher(&prompt)
            {
                format!("{hint}\n\n{prompt}")
            } else {
                prompt
            };

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

            let config = AgentConfig {
                model: &tui_config.model,
                system_prompt: tui_config.system_prompt.clone(),
                tools,
                registry,
                options: tui_config.options.clone(),
                max_turns: tui_config.max_turns,
                get_steering: steering,
                get_follow_up: None,
                transform_context: transform,
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
                                handle_agent_event(&mut app, &mut conversation, &mut session_tree, &event, &tui_config.model);
                                if matches!(event, AgentEvent::AgentEnd) {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = tick => {
                        // check for terminal input during streaming
                        if event::poll(std::time::Duration::ZERO)? {
                            match event::read()? {
                                Event::Key(key) => {
                                    if let Some(app_event) = handle_key(&mut app, key) {
                                        match app_event {
                                            AppEvent::Quit => {
                                                cleanup(&mut terminal)?;
                                                return Ok(());
                                            }
                                            AppEvent::Abort => {
                                                app.is_streaming = false;
                                                app.active_tool = None;
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

            draw(&mut terminal, &app, &mut image_protos)?;
            continue;
        }

        // idle: wait for terminal input
        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(app_event) = handle_key(&mut app, key) {
                        match app_event {
                            AppEvent::Quit => break,
                            AppEvent::UserSubmit { text } => {
                                let expanded = expand_template(&text);
                                app.push_user_message(expanded.clone());
                                app.start_streaming();
                                pending_prompt = Some(expanded);
                            }
                            AppEvent::SlashCommand { name, args } => {
                                if let Some(prompt) = handle_slash_command(
                                    &mut app,
                                    &mut conversation,
                                    &mut session_tree,
                                    &mut tui_config,
                                    &name,
                                    &args,
                                ) {
                                    app.start_streaming();
                                    pending_prompt = Some(prompt);
                                }
                            }
                            AppEvent::CycleThinkingLevel => {
                                tui_config.options.thinking = Some(app.thinking_level);
                            }
                            _ => {}
                        }
                    }
                }
                Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                _ => {}
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

fn handle_agent_event(
    app: &mut App,
    conversation: &mut Vec<Message>,
    session_tree: &mut SessionTree,
    event: &AgentEvent,
    model: &Model,
) {
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
            let msg = Message::Assistant(message.clone());
            session_tree.append_message(msg.clone());
            conversation.push(msg);
        }
        AgentEvent::ToolExecStart {
            tool_name, args, ..
        } => {
            let summary = summarise_tool_args(tool_name.as_str(), args);
            app.start_tool(tool_name.as_str(), &summary);
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
            app.end_tool(tool_name.as_str(), result.is_error, output_text, image_data);
            session_tree.append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
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

/// handle a slash command, returning Some(prompt) if it should trigger the agent
fn handle_slash_command(
    app: &mut App,
    conversation: &mut Vec<Message>,
    session_tree: &mut SessionTree,
    tui_config: &mut TuiConfig,
    name: &str,
    args: &str,
) -> Option<String> {
    match name {
        "help" => {
            let mut help = String::from("available commands:\n");
            help.push_str("  /help          - show this message\n");
            help.push_str("  /clear         - clear conversation\n");
            help.push_str("  /model [id]    - show or switch model\n");
            help.push_str("  /sessions      - browse and resume sessions\n");
            help.push_str("  /branch [n]    - branch from nth user message\n");
            help.push_str("  /tree          - show conversation tree\n");
            help.push_str("  /cost          - show session cost\n");
            help.push_str("  /quit          - exit mush\n");
            help.push_str("\ntip: type a prompt template name (e.g. /review file.rs) to expand it");
            app.push_system_message(help);
            None
        }
        "clear" => {
            app.clear_messages();
            conversation.clear();
            *session_tree = SessionTree::new();
            app.status = Some("conversation cleared".into());
            None
        }
        "tree" => {
            let branch = session_tree.current_branch();
            let user_msgs: Vec<_> = branch
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        mush_session::tree::EntryKind::Message {
                            message: Message::User(_)
                        }
                    )
                })
                .collect();

            if user_msgs.is_empty() {
                app.push_system_message("no messages yet".into());
            } else {
                let mut info = format!(
                    "tree: {} entries, {} branch points\n",
                    session_tree.len(),
                    session_tree
                        .entries()
                        .iter()
                        .filter(|e| session_tree.is_branch_point(&e.id))
                        .count()
                );
                for (i, entry) in user_msgs.iter().enumerate() {
                    if let mush_session::tree::EntryKind::Message {
                        message: Message::User(u),
                    } = &entry.kind
                    {
                        let text = match &u.content {
                            UserContent::Text(t) => t.as_str(),
                            _ => "(parts)",
                        };
                        let preview = if text.len() > 60 {
                            format!("{}...", &text[..57])
                        } else {
                            text.to_string()
                        };
                        let marker = if session_tree.is_branch_point(&entry.id) {
                            " ⑂"
                        } else {
                            ""
                        };
                        info.push_str(&format!("  {}: {preview}{marker}\n", i + 1));
                    }
                }
                app.push_system_message(info);
            }
            None
        }
        "branch" if !args.is_empty() => {
            // /branch N — branch from the Nth user message
            if let Ok(n) = args.trim().parse::<usize>() {
                // collect info we need before mutating the tree
                let user_msgs = session_tree.user_messages_in_branch();
                let count = user_msgs.len();
                let target_info = user_msgs.get(n.wrapping_sub(1)).map(|e| {
                    let parent = e.parent_id.clone();
                    let preview = match &e.kind {
                        mush_session::tree::EntryKind::Message {
                            message: Message::User(u),
                        } => match &u.content {
                            UserContent::Text(t) if t.len() > 40 => {
                                format!("{}...", &t[..37])
                            }
                            UserContent::Text(t) => t.clone(),
                            _ => "message".into(),
                        },
                        _ => "message".into(),
                    };
                    (parent, preview)
                });
                drop(user_msgs);

                if n == 0 || n > count {
                    app.push_system_message(format!(
                        "invalid: use 1-{count} (try /tree to see messages)"
                    ));
                } else if let Some((parent_id, preview)) = target_info {
                    if let Some(ref pid) = parent_id {
                        session_tree.branch(pid);
                    } else {
                        session_tree.reset_leaf();
                    }

                    // rebuild conversation from tree
                    *conversation = session_tree.build_context();
                    rebuild_display(app, conversation);
                    app.status = Some(format!("branched before: {preview}"));
                }
            } else {
                app.push_system_message("usage: /branch <number> (try /tree first)".into());
            }
            None
        }
        "branch" => {
            app.push_system_message(
                "usage: /branch <n> — branch from nth user message\ntry /tree to see messages"
                    .into(),
            );
            None
        }
        "sessions" => {
            let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
            match store.list() {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        app.push_system_message("no saved sessions".into());
                    } else {
                        app.open_session_picker(sessions);
                    }
                }
                Err(e) => app.push_system_message(format!("failed to list sessions: {e}")),
            }
            None
        }
        "resume" if !args.is_empty() => {
            // triggered by the session picker on enter
            let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
            let id = mush_session::SessionId(args.trim().to_string());
            match store.load(&id) {
                Ok(session) => {
                    *conversation = session.messages.clone();
                    *session_tree = session.tree;
                    rebuild_display(app, conversation);
                    let title = session.meta.title.as_deref().unwrap_or("untitled");
                    app.status = Some(format!("resumed: {title}"));
                }
                Err(e) => app.push_system_message(format!("failed to load session: {e}")),
            }
            None
        }
        "model" if args.is_empty() => {
            app.push_system_message(format!("model: {}", app.model_id));
            None
        }
        "model" => {
            let id = args.trim();
            if let Some(new_model) = models::find_model_by_id(id) {
                tui_config.model = new_model;
                app.model_id = id.to_string();
                app.push_system_message(format!("switched to {id}"));
            } else {
                let available = models::all_models_with_user()
                    .iter()
                    .map(|m| format!("  {}", m.id))
                    .collect::<Vec<_>>()
                    .join("\n");
                app.push_system_message(format!("unknown model: {id}\n\navailable:\n{available}"));
            }
            None
        }
        "cost" => {
            app.push_system_message(format!(
                "session: {}tok, ${:.4}",
                app.total_tokens, app.total_cost
            ));
            None
        }
        "quit" | "exit" | "q" => {
            app.should_quit = true;
            None
        }
        other => {
            // try as a prompt template
            let cwd = std::env::current_dir().unwrap_or_default();
            let templates = mush_ext::discover_templates(&cwd);
            if let Some(tmpl) = mush_ext::find_template(&templates, other) {
                let arg_list: Vec<&str> = if args.is_empty() {
                    vec![]
                } else {
                    args.split_whitespace().collect()
                };
                let expanded = mush_ext::substitute_args(&tmpl.content, &arg_list);
                app.push_user_message(expanded.clone());
                Some(expanded)
            } else {
                app.push_system_message(format!("unknown command: /{other}  (try /help)"));
                None
            }
        }
    }
}

/// expand /template_name args... into template content
fn expand_template(prompt: &str) -> String {
    if !prompt.starts_with('/') {
        return prompt.to_string();
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let templates = mush_ext::discover_templates(&cwd);

    let parts: Vec<&str> = prompt[1..].splitn(2, ' ').collect();
    let name = parts[0];
    let args_str = parts.get(1).unwrap_or(&"");

    if let Some(tmpl) = mush_ext::find_template(&templates, name) {
        let args: Vec<&str> = if args_str.is_empty() {
            vec![]
        } else {
            args_str.split_whitespace().collect()
        };
        mush_ext::substitute_args(&tmpl.content, &args)
    } else {
        prompt.to_string()
    }
}

/// rebuild the TUI display from a conversation (used after branching/resuming)
fn rebuild_display(app: &mut App, conversation: &[Message]) {
    app.clear_messages();
    for msg in conversation {
        match msg {
            Message::User(u) => {
                let text = match &u.content {
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
                app.push_user_message(text);
            }
            Message::Assistant(a) => {
                let text = a
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                app.start_streaming();
                app.push_text_delta(&text);
                app.finish_streaming(Some(a.usage), None);
            }
            _ => {}
        }
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
        if !app.is_streaming && app.mode == app::AppMode::Normal {
            frame.set_cursor_position((cx, cy));
        }
        // render inline images (after main UI so they overlay correctly)
        // images are rendered in the tool output area
        for (key, proto) in image_protos.iter_mut() {
            let (msg_idx, _tc_idx) = *key;
            // find approximate position for this image
            // for now, render at a fixed area near bottom of message list
            // (a proper implementation would track exact positions from layout)
            let _ = (msg_idx, proto);
            // TODO: track image positions from message_list layout and render here
        }
        // session picker overlay
        if let Some(ref picker) = app.session_picker {
            widgets::session_picker::render(frame, picker);
        }
    })?;
    Ok(())
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
        }
        _ => {}
    }
}

fn cleanup(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    io::stdout().execute(crossterm::event::DisableMouseCapture)?;
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
