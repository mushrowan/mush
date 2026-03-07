//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;
use std::sync::Arc;

use crossterm::ExecutableCommand;
use crossterm::cursor::{SetCursorStyle, Show};
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
    /// automatically compact conversation when approaching context limit (on by default)
    pub auto_compact: bool,
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
    /// multi-pane file isolation mode
    pub isolation_mode: crate::file_tracker::IsolationMode,
}

struct TerminalStateGuard {
    active: bool,
}

impl TerminalStateGuard {
    fn new() -> Self {
        Self { active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for TerminalStateGuard {
    fn drop(&mut self) {
        if self.active {
            restore_terminal_state();
        }
    }
}

/// run the interactive TUI
pub async fn run_tui(
    mut tui_config: TuiConfig,
    tools: &[Box<dyn AgentTool>],
    registry: &ApiRegistry,
) -> io::Result<()> {
    restore_terminal_state();

    // detect image protocol before entering alternate screen to avoid probe artifacts
    let image_picker = ratatui_image::picker::Picker::from_query_stdio().ok();

    let mut terminal_guard = TerminalStateGuard::new();

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
        restore_terminal_state();
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
    let session_tree = SessionTree::new();

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
    initial_pane.conversation = conversation;
    initial_pane.session_tree = session_tree;
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

    use crate::pane::PaneId;
    use futures::stream::SelectAll;
    type TaggedStream<'a> =
        std::pin::Pin<Box<dyn futures::Stream<Item = (PaneId, AgentEvent)> + Send + 'a>>;

    // per-stream state for tool confirmation
    struct StreamMeta {
        steering_queue: Arc<Mutex<Vec<Message>>>,
        confirm_req_rx: tokio::sync::mpsc::Receiver<(String, tokio::sync::oneshot::Sender<bool>)>,
        confirm_reply: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
        model: Model,
    }

    let mut agent_streams: SelectAll<TaggedStream<'_>> = SelectAll::new();
    let mut stream_metas: std::collections::HashMap<PaneId, StreamMeta> =
        std::collections::HashMap::new();

    loop {
        // -- start streams for any pane with a pending prompt --
        let mut prompts: Vec<(PaneId, String)> = Vec::new();
        if let Some(prompt) = pending_prompt.take() {
            prompts.push((pane_mgr.focused().id, prompt));
        }
        for pane in pane_mgr.panes_mut() {
            if let Some(prompt) = pane.pending_prompt.take() {
                if !prompts.iter().any(|(id, _)| *id == pane.id) {
                    prompts.push((pane.id, prompt));
                }
            }
        }

        for (pane_id, prompt) in prompts {
            let steering_queue = pane_mgr.pane(pane_id).unwrap().steering_queue.clone();

            let model = pane_mgr
                .pane(pane_id)
                .map(|p| {
                    models::find_model_by_id(p.app.model_id.as_str())
                        .unwrap_or_else(|| tui_config.model.clone())
                })
                .unwrap_or_else(|| tui_config.model.clone());
            let thinking_level = pane_mgr
                .pane(pane_id)
                .map(|p| p.app.thinking_level)
                .unwrap_or(ThinkingLevel::Off);

            let conversation_snapshot = {
                let pane = pane_mgr.pane_mut(pane_id).unwrap();
                let (app, conversation, session_tree, _) = pane.fields_mut();

                let prompt_preview = prompt.clone();
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
                conversation.clone()
            };

            // build callbacks (no pane borrows)
            let context_window = model.context_window as usize;
            let enricher_arc = if tui_config.hint_mode == HintMode::Transform {
                tui_config.prompt_enricher.clone()
            } else {
                None
            };
            let compact_model = model.clone();
            let compact_options = tui_config.options.clone();
            let do_auto_compact = tui_config.auto_compact;
            #[expect(clippy::type_complexity)]
            let compaction_cache: std::sync::Arc<
                tokio::sync::Mutex<Option<(usize, Vec<Message>)>>,
            > = std::sync::Arc::new(tokio::sync::Mutex::new(None));
            let transform: Option<mush_agent::ContextTransform<'_>> = Some(Box::new(move |msgs| {
                let enricher = enricher_arc.clone();
                let model = compact_model.clone();
                let options = compact_options.clone();
                let cache = compaction_cache.clone();
                Box::pin(async move {
                    let mut msgs = if do_auto_compact {
                        let mut guard = cache.lock().await;
                        if let Some((orig_len, ref compacted)) = *guard {
                            if msgs.len() >= orig_len {
                                let mut result = compacted.clone();
                                result.extend_from_slice(&msgs[orig_len..]);
                                result
                            } else {
                                *guard = None;
                                msgs
                            }
                        } else {
                            let orig_len = msgs.len();
                            let compacted = event_handler::auto_compact(
                                msgs,
                                context_window,
                                registry,
                                &model,
                                &options,
                            )
                            .await;
                            if compacted.len() < orig_len {
                                *guard = Some((orig_len, compacted.clone()));
                            }
                            compacted
                        }
                    } else {
                        msgs
                    };
                    if let Some(ref enricher) = enricher {
                        event_handler::inject_hint(&mut msgs, enricher.as_ref());
                    }
                    // mask old tool outputs to save context space
                    event_handler::mask_observations(&mut msgs);
                    msgs
                })
            }));

            let sq = steering_queue.clone();
            let steering: Option<mush_agent::MessageCallback<'_>> = Some(Box::new(move || {
                let sq = sq.clone();
                Box::pin(async move {
                    let mut q = sq.lock().await;
                    q.drain(..).collect()
                })
            }));

            let sq_follow = steering_queue.clone();
            let follow_up: Option<mush_agent::MessageCallback<'_>> = Some(Box::new(move || {
                let sq = sq_follow.clone();
                Box::pin(async move {
                    let mut q = sq.lock().await;
                    q.drain(..).collect()
                })
            }));

            let (confirm_req_tx, confirm_req_rx) =
                tokio::sync::mpsc::channel::<(String, tokio::sync::oneshot::Sender<bool>)>(1);
            let confirm: Option<mush_agent::ConfirmCallback<'_>> =
                if tui_config.confirm_tools || pane_mgr.is_multi_pane() {
                    let ft = file_tracker.clone();
                    let lock_pane_id = pane_id;
                    let do_prompt = tui_config.confirm_tools;
                    Some(Box::new(move |name: &str, args: &serde_json::Value| {
                        let ft = ft.clone();
                        let tx = confirm_req_tx.clone();
                        let summary = mush_agent::summarise_tool_args(name, args);
                        let prompt = format!("{name} {summary}");
                        let name = name.to_string();
                        let args = args.clone();
                        Box::pin(async move {
                            // check file locks for write/edit tools
                            if matches!(name.as_str(), "write" | "edit")
                                && let Some(path) = args["path"].as_str()
                                && let Some(owner) = ft.check_lock(lock_pane_id, path)
                            {
                                return mush_agent::ConfirmAction::DenyWithReason(
                                    format!(
                                        "file \"{}\" is locked by pane {}",
                                        path,
                                        owner.as_u32()
                                    ),
                                );
                            }
                            // user confirmation if enabled
                            if do_prompt {
                                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                if tx.send((prompt, resp_tx)).await.is_err() {
                                    return mush_agent::ConfirmAction::Allow;
                                }
                                match resp_rx.await {
                                    Ok(true) => mush_agent::ConfirmAction::Allow,
                                    _ => mush_agent::ConfirmAction::Deny,
                                }
                            } else {
                                mush_agent::ConfirmAction::Allow
                            }
                        })
                    }))
                } else {
                    None
                };

            let mut call_options = tui_config.options.clone();
            let (api_key, account_id) =
                event_handler::resolve_auth_for_model(&model, &tui_config.provider_api_keys).await;
            call_options.api_key = api_key;
            call_options.account_id = account_id;
            call_options.thinking = Some(thinking_level);

            // build per-pane extra tools (send_message, shared state etc)
            let mut extra_tools: Vec<Box<dyn mush_agent::tool::AgentTool>> = Vec::new();
            if pane_mgr.is_multi_pane() {
                extra_tools.push(Box::new(crate::messaging::SendMessageTool {
                    sender_id: pane_id,
                    bus: message_bus.clone(),
                }));
                extra_tools.push(Box::new(crate::shared_state::ReadStateTool {
                    state: shared_state.clone(),
                }));
                extra_tools.push(Box::new(crate::shared_state::WriteStateTool {
                    state: shared_state.clone(),
                }));
            }

            // inject sibling awareness into system prompt when multi-pane
            let mut system_prompt = if pane_mgr.is_multi_pane() {
                let sibling_info = build_sibling_prompt(pane_id, &pane_mgr);
                match tui_config.system_prompt.as_ref() {
                    Some(base) => Some(format!("{base}\n\n{sibling_info}")),
                    None => Some(sibling_info),
                }
            } else {
                tui_config.system_prompt.clone()
            };

            // inject VCS isolation context into system prompt
            if let Some(pane) = pane_mgr.pane(pane_id) {
                match &pane.isolation {
                    Some(crate::isolation::PaneIsolation::Worktree { path, branch }) => {
                        let note = format!(
                            "\n\n## worktree isolation\n\
                             you are working in a git worktree at `{}`.\n\
                             your branch is `{branch}`. all file operations are isolated \
                             from the main working directory. use /merge when your work \
                             is ready to be merged back.",
                            path.display()
                        );
                        system_prompt = Some(
                            system_prompt.map_or(note.clone(), |s| format!("{s}{note}")),
                        );
                    }
                    Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
                        let short = &change_id[..change_id.len().min(12)];
                        let note = format!(
                            "\n\n## jj isolation\n\
                             you are working on jj change `{short}`. \
                             your edits are tracked as a separate jj change. \
                             use /merge when your work is ready to be squashed \
                             into the parent change."
                        );
                        system_prompt = Some(
                            system_prompt.map_or(note.clone(), |s| format!("{s}{note}")),
                        );
                    }
                    None => {}
                }
            }

            // take per-pane tools from the pane (if any, e.g. worktree isolation)
            let pane_tools: Option<Vec<Box<dyn mush_agent::tool::AgentTool>>> =
                pane_mgr.pane_mut(pane_id).and_then(|p| p.tools.take());

            // when a pane has its own tools (worktree isolation), merge them into
            // extra_tools to avoid holding a borrow on any external storage.
            // extra_tools chain with tools in the agent loop
            if let Some(mut pt) = pane_tools {
                // prepend pane tools before the coordination tools
                pt.append(&mut extra_tools);
                extra_tools = pt;
            }

            stream_metas.insert(
                pane_id,
                StreamMeta {
                    steering_queue: steering_queue.clone(),
                    confirm_req_rx,
                    confirm_reply: Arc::new(Mutex::new(None)),
                    model: model.clone(),
                },
            );

            let config = AgentConfig {
                model: model.clone(),
                system_prompt,
                tools,
                extra_tools,
                registry,
                options: call_options,
                max_turns: tui_config.max_turns,
                get_steering: steering,
                get_follow_up: follow_up,
                transform_context: transform,
                confirm_tool: confirm,
            };

            let stream = agent_loop(config, conversation_snapshot);
            let tagged: TaggedStream<'_> =
                Box::pin(futures::StreamExt::map(stream, move |ev| (pane_id, ev)));
            agent_streams.push(tagged);
        }

        // -- main event loop: streaming or idle --
        let any_streaming = !agent_streams.is_empty();

        if any_streaming {
            let tick = tokio::time::sleep(std::time::Duration::from_millis(16));
            tokio::pin!(tick);

            tokio::select! {
                result = agent_streams.next() => {
                    if let Some((pane_id, event)) = result {
                        // route agent event to the correct pane
                        if let Some(pane) = pane_mgr.pane_mut(pane_id) {
                            let model = stream_metas
                                .get(&pane_id)
                                .map(|m| &m.model)
                                .unwrap_or(&tui_config.model);
                            let (app, conversation, session_tree, image_protos) =
                                pane.fields_mut();
                            let mut ctx = EventCtx {
                                app,
                                conversation,
                                session_tree,
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

                        // file modification tracking (none isolation mode)
                        if pane_mgr.is_multi_pane() {
                            match &event {
                                AgentEvent::ToolExecStart {
                                    tool_call_id,
                                    tool_name,
                                    args,
                                } => {
                                    file_tracker.record_tool_start(
                                        pane_id,
                                        tool_call_id.as_str(),
                                        tool_name.as_str(),
                                        args,
                                    );
                                }
                                AgentEvent::ToolExecEnd {
                                    tool_call_id,
                                    tool_name: _,
                                    result,
                                } => {
                                    if let Some(conflict) = file_tracker.record_tool_end(
                                        pane_id,
                                        tool_call_id.as_str(),
                                        result.outcome.is_success(),
                                    ) {
                                        let others: Vec<String> = conflict
                                            .other_panes
                                            .iter()
                                            .map(|p| p.to_string())
                                            .collect();
                                        let warning = format!(
                                            "⚠ file conflict: {} also modified by pane {}",
                                            conflict.path.display(),
                                            others.join(", ")
                                        );
                                        if let Some(pane) = pane_mgr.pane_mut(pane_id) {
                                            pane.app.status = Some(warning);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        if matches!(event, AgentEvent::AgentEnd) {
                            let stream_model = stream_metas
                                .get(&pane_id)
                                .map(|m| m.model.clone())
                                .unwrap_or_else(|| tui_config.model.clone());
                            stream_metas.remove(&pane_id);
                            // auto-save for this pane
                            if let Some(pane) = pane_mgr.pane(pane_id) {
                                if let Some(ref saver) = tui_config.save_session {
                                    saver(
                                        &pane.conversation,
                                        &pane.session_tree,
                                        &pane.app.model_id,
                                    );
                                }
                            }
                            // auto-compact for this pane
                            if tui_config.auto_compact
                            && let Some(pane) = pane_mgr.pane(pane_id) {
                                let needs = mush_session::compact::needs_compaction(
                                    &pane.conversation,
                                    stream_model.context_window as usize,
                                );
                                if needs {
                                    if let Some(pane) = pane_mgr.pane_mut(pane_id) {
                                        pane.app.status = Some("auto-compacting…".into());
                                    }
                                    draw_panes(&mut terminal, &mut pane_mgr, &image_picker)?;
                                    if let Some(pane) = pane_mgr.pane_mut(pane_id) {
                                        let (app, conversation, session_tree, _) =
                                            pane.fields_mut();
                                        slash::handle_compact(
                                            app,
                                            conversation,
                                            session_tree,
                                            &stream_model,
                                            &tui_config.options,
                                            registry,
                                        )
                                        .await;
                                    }
                                    if let Some(pane) = pane_mgr.pane(pane_id) {
                                        if let Some(ref saver) = tui_config.save_session {
                                            saver(
                                                &pane.conversation,
                                                &pane.session_tree,
                                                &pane.app.model_id,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ = tick => {
                    // check for tool confirmation on focused pane
                    let focused_id = pane_mgr.focused().id;
                    if let Some(meta) = stream_metas.get_mut(&focused_id) {
                        if let Ok((prompt, reply_tx)) = meta.confirm_req_rx.try_recv() {
                            let app = &mut pane_mgr.focused_mut().app;
                            app.mode = app::AppMode::ToolConfirm;
                            app.confirm_prompt = Some(prompt);
                            *meta.confirm_reply.lock().await = Some(reply_tx);
                        }
                    }

                    // poll live tool output for focused pane
                    if let Some(ref live) = tui_config.tool_output_live {
                        let app = &mut pane_mgr.focused_mut().app;
                        if let Ok(guard) = live.lock()
                            && let Some(last) = guard.as_ref()
                            && let Some(active) =
                                app.active_tools.last().map(|t| t.tool_call_id.clone())
                        {
                            app.push_tool_output(active.as_str(), last);
                        }
                    }

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
                                        let fid = pane_mgr.focused().id;
                                        if let Some(meta) = stream_metas.get_mut(&fid) {
                                            if let Some(tx) =
                                                meta.confirm_reply.lock().await.take()
                                            {
                                                let _ = tx.send(allowed);
                                            }
                                        }
                                        let app = &mut pane_mgr.focused_mut().app;
                                        app.mode = app::AppMode::Normal;
                                        app.confirm_prompt = None;
                                        if !allowed {
                                            app.status = Some("tool denied".into());
                                        }
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
                                                // abort focused pane's stream
                                                let app = &mut pane_mgr.focused_mut().app;
                                                app.is_streaming = false;
                                                app.active_tools.clear();
                                                app.status = Some("aborted".into());
                                                // note: can't cancel a specific stream in
                                                // SelectAll, so the stream continues but
                                                // events will be ignored (pane not streaming)
                                            }
                                            AppEvent::UserSubmit { text } => {
                                                let fid = pane_mgr.focused().id;
                                                if let Some(meta) = stream_metas.get(&fid) {
                                                    let msg = Message::User(UserMessage {
                                                        content: UserContent::Text(text.clone()),
                                                        timestamp_ms: Timestamp::now(),
                                                    });
                                                    meta.steering_queue
                                                        .lock()
                                                        .await
                                                        .push(msg);
                                                }
                                                pane_mgr
                                                    .focused_mut()
                                                    .app
                                                    .push_queued_message(text);
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
                                            AppEvent::ClosePane => {
                                                if pane_mgr.is_multi_pane() {
                                                    let closed_id = pane_mgr.focused().id;
                                                    let isolation = pane_mgr.focused().isolation.clone();
                                                    file_tracker.release_pane(closed_id);
                                                    message_bus.unregister(closed_id);
                                                    cleanup_pane_isolation(&cwd, &isolation).await;
                                                    pane_mgr.close_focused();
                                                }
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
                                        if pane.label.is_none() && pane.conversation.is_empty()
                                        {
                                            pane.label = Some(
                                                expanded.chars().take(30).collect(),
                                            );
                                        }
                                        let app = &mut pane_mgr.focused_mut().app;
                                        app.push_user_message(expanded.clone());
                                        app.active_tools.clear();
                                        app.start_streaming();
                                        pending_prompt = Some(expanded);
                                    }
                                    AppEvent::SlashCommand { name, args } => {
                                        // track whether the command mutated conversation state
                                        let mut state_changed = false;

                                        if name == "search" {
                                            let app = &mut pane_mgr.focused_mut().app;
                                            app.mode = app::AppMode::Search;
                                            app.search.query = args.to_string();
                                            app.update_search();
                                        } else if name == "compact" {
                                            let pane = pane_mgr.focused_mut();
                                            let (app, conversation, session_tree, _) =
                                                pane.fields_mut();
                                            slash::handle_compact(
                                                app,
                                                conversation,
                                                session_tree,
                                                &models::find_model_by_id(app.model_id.as_str())
                                                    .unwrap_or_else(|| tui_config.model.clone()),
                                                &tui_config.options,
                                                registry,
                                            )
                                            .await;
                                            state_changed = true;
                                        } else if name == "export" {
                                            let pane = pane_mgr.focused_mut();
                                            slash::handle_export(
                                                &mut pane.app,
                                                &pane.conversation,
                                                &args,
                                            );
                                        } else if name == "broadcast" {
                                            if !pane_mgr.is_multi_pane() {
                                                pane_mgr.focused_mut().app.push_system_message(
                                                    "no sibling panes to broadcast to",
                                                );
                                            } else if args.trim().is_empty() {
                                                pane_mgr.focused_mut().app.push_system_message(
                                                    "usage: /broadcast <message>",
                                                );
                                            } else {
                                                let from = pane_mgr.focused().id;
                                                let sent = message_bus.broadcast(
                                                    from,
                                                    args.trim().to_string(),
                                                );
                                                pane_mgr.focused_mut().app.push_system_message(
                                                    format!("broadcast sent to {sent} pane(s)"),
                                                );
                                            }
                                        } else if name == "lock" {
                                            let pane_id = pane_mgr.focused().id;
                                            if args.is_empty() {
                                                pane_mgr.focused_mut().app.status =
                                                    Some("usage: /lock <path>".into());
                                            } else {
                                                match file_tracker.lock(pane_id, args.trim()) {
                                                    Ok(()) => {
                                                        pane_mgr.focused_mut().app.status =
                                                            Some(format!("locked {}", args.trim()));
                                                    }
                                                    Err(owner) => {
                                                        pane_mgr.focused_mut().app.status =
                                                            Some(format!(
                                                                "already locked by pane {}",
                                                                owner.as_u32()
                                                            ));
                                                    }
                                                }
                                            }
                                        } else if name == "unlock" {
                                            let pane_id = pane_mgr.focused().id;
                                            if args.is_empty() {
                                                pane_mgr.focused_mut().app.status =
                                                    Some("usage: /unlock <path>".into());
                                            } else if file_tracker.unlock(pane_id, args.trim()) {
                                                pane_mgr.focused_mut().app.status =
                                                    Some(format!("unlocked {}", args.trim()));
                                            } else {
                                                pane_mgr.focused_mut().app.status =
                                                    Some("not locked by this pane".into());
                                            }
                                        } else if name == "locks" {
                                            let locks = file_tracker.list_locks();
                                            if locks.is_empty() {
                                                pane_mgr.focused_mut().app.push_system_message(
                                                    "no file locks active".to_string(),
                                                );
                                            } else {
                                                let mut msg = String::from("file locks:\n");
                                                for (path, owner) in &locks {
                                                    msg.push_str(&format!(
                                                        "  {} (pane {})\n",
                                                        path.display(),
                                                        owner.as_u32()
                                                    ));
                                                }
                                                pane_mgr
                                                    .focused_mut()
                                                    .app
                                                    .push_system_message(msg.trim_end().to_string());
                                            }
                                        } else if name == "label" {
                                            let pane_id = pane_mgr.focused().id;
                                            if args.trim().is_empty() {
                                                // auto-generate from first user message
                                                let label = pane_mgr
                                                    .focused()
                                                    .conversation
                                                    .iter()
                                                    .find_map(|m| match m {
                                                        Message::User(u) => {
                                                            let t = u.text();
                                                            if t.is_empty() {
                                                                None
                                                            } else {
                                                                Some(
                                                                    t.chars().take(30).collect::<String>(),
                                                                )
                                                            }
                                                        }
                                                        _ => None,
                                                    })
                                                    .unwrap_or_else(|| {
                                                        format!("pane {}", pane_id.as_u32())
                                                    });
                                                pane_mgr.focused_mut().label = Some(label.clone());
                                                pane_mgr.focused_mut().app.status =
                                                    Some(format!("label: {label}"));
                                            } else {
                                                let label = args.trim().to_string();
                                                pane_mgr.focused_mut().label = Some(label.clone());
                                                pane_mgr.focused_mut().app.status =
                                                    Some(format!("label: {label}"));
                                            }
                                        } else if name == "panes" {
                                            let mut msg = String::from("active panes:\n");
                                            for (i, pane) in pane_mgr.panes().iter().enumerate()
                                            {
                                                let idx = i + 1;
                                                let label = pane
                                                    .label
                                                    .as_deref()
                                                    .unwrap_or("(unlabelled)");
                                                let status = if pane.app.is_streaming {
                                                    "streaming"
                                                } else {
                                                    "idle"
                                                };
                                                let model = &pane.app.model_id;
                                                let cost = if pane.app.stats.total_cost > 0.0 {
                                                    format!(
                                                        " ${:.4}",
                                                        pane.app.stats.total_cost
                                                    )
                                                } else {
                                                    String::new()
                                                };
                                                let focused =
                                                    if i == pane_mgr.focused_index() {
                                                        " *"
                                                    } else {
                                                        ""
                                                    };
                                                msg.push_str(&format!(
                                                    "  {idx}. {label} [{status}] {model}{cost}{focused}\n"
                                                ));
                                            }
                                            pane_mgr
                                                .focused_mut()
                                                .app
                                                .push_system_message(msg.trim_end().to_string());
                                        } else if name == "cost"
                                            && pane_mgr.is_multi_pane()
                                        {
                                            // aggregate cost across all panes
                                            let focused = &mut pane_mgr.focused_mut().app;
                                            focused.show_cost = !focused.show_cost;
                                            let show = focused.show_cost;
                                            let mut total_cost = 0.0_f64;
                                            let mut total_tokens = 0_u64;
                                            let mut lines = Vec::new();
                                            for (i, pane) in
                                                pane_mgr.panes().iter().enumerate()
                                            {
                                                let idx = i + 1;
                                                let label = pane
                                                    .label
                                                    .as_deref()
                                                    .unwrap_or("(unlabelled)");
                                                let s = &pane.app.stats;
                                                total_cost += s.total_cost;
                                                total_tokens += s.total_tokens;
                                                if s.total_tokens > 0 {
                                                    lines.push(format!(
                                                        "  pane {idx} ({label}): {}tok ${:.4}",
                                                        s.total_tokens, s.total_cost
                                                    ));
                                                }
                                            }
                                            if show {
                                                let mut msg = format!(
                                                    "total: {}tok ${:.4}\n",
                                                    total_tokens, total_cost
                                                );
                                                for line in &lines {
                                                    msg.push_str(line);
                                                    msg.push('\n');
                                                }
                                                pane_mgr
                                                    .focused_mut()
                                                    .app
                                                    .push_system_message(
                                                        msg.trim_end().to_string(),
                                                    );
                                            }
                                            // sync show_cost to all panes
                                            for pane in pane_mgr.panes_mut() {
                                                pane.app.show_cost = show;
                                            }
                                        } else if name == "close" {
                                            if pane_mgr.is_multi_pane() {
                                                let closed_id = pane_mgr.focused().id;
                                                let isolation = pane_mgr.focused().isolation.clone();
                                                file_tracker.release_pane(closed_id);
                                                message_bus.unregister(closed_id);
                                                cleanup_pane_isolation(&cwd, &isolation).await;
                                                pane_mgr.close_focused();
                                            } else {
                                                pane_mgr.focused_mut().app.status =
                                                    Some("can't close the last pane".into());
                                            }
                                        } else if name == "merge" {
                                            let pane = pane_mgr.focused();
                                            match &pane.isolation {
                                                Some(crate::isolation::PaneIsolation::Worktree { branch, .. }) => {
                                                    let branch = branch.clone();
                                                    let pane_id = pane.id;
                                                    match crate::isolation::merge_worktree(&cwd, pane_id).await {
                                                        Ok(msg) => {
                                                            pane_mgr.focused_mut().app.push_system_message(
                                                                format!("merged {branch}: {msg}"),
                                                            );
                                                        }
                                                        Err(e) => {
                                                            pane_mgr.focused_mut().app.push_system_message(
                                                                format!("merge failed: {e}"),
                                                            );
                                                        }
                                                    }
                                                }
                                                Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
                                                    let change_id = change_id.clone();
                                                    match crate::isolation::squash_jj_change(&cwd, &change_id).await {
                                                        Ok(msg) => {
                                                            pane_mgr.focused_mut().app.push_system_message(
                                                                format!("squashed jj change: {msg}"),
                                                            );
                                                            // clear isolation since it's been merged
                                                            pane_mgr.focused_mut().isolation = None;
                                                        }
                                                        Err(e) => {
                                                            pane_mgr.focused_mut().app.push_system_message(
                                                                format!("squash failed: {e}"),
                                                            );
                                                        }
                                                    }
                                                }
                                                None => {
                                                    pane_mgr.focused_mut().app.status =
                                                        Some("no isolation to merge (mode is none)".into());
                                                }
                                            }
                                        } else {
                                            let pane = pane_mgr.focused_mut();
                                            let (app, conversation, session_tree, _) =
                                                pane.fields_mut();
                                            if let Some(prompt) = slash::handle(
                                                app,
                                                conversation,
                                                session_tree,
                                                &mut tui_config,
                                                &thinking_prefs,
                                                &name,
                                                &args,
                                            ) {
                                                app.start_streaming();
                                                pending_prompt = Some(prompt);
                                            }
                                            // undo, clear, branch mutate state
                                            if matches!(name.as_str(), "undo" | "clear" | "branch") {
                                                state_changed = true;
                                            }
                                        }

                                        // persist after state-mutating commands
                                        if state_changed {
                                            if let Some(ref saver) = tui_config.save_session {
                                                let pane = pane_mgr.focused();
                                                saver(
                                                    &pane.conversation,
                                                    &pane.session_tree,
                                                    &pane.app.model_id,
                                                );
                                            }
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
                                        fork_pane(&mut pane_mgr, &tui_config, &message_bus, &tui_config.tool_output_live).await;
                                    }
                                    AppEvent::ClosePane => {
                                        if pane_mgr.is_multi_pane() {
                                            let closed_id = pane_mgr.focused().id;
                                            let isolation = pane_mgr.focused().isolation.clone();
                                            file_tracker.release_pane(closed_id);
                                            message_bus.unregister(closed_id);
                                            cleanup_pane_isolation(&cwd, &isolation).await;
                                            pane_mgr.close_focused();
                                        } else {
                                            pane_mgr.focused_mut().app.status =
                                                Some("can't close the last pane".into());
                                        }
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
        draw_panes(&mut terminal, &mut pane_mgr, &image_picker)?;
    }

    cleanup(&mut terminal)?;
    terminal_guard.disarm();
    Ok(())
}

use crate::pane::{LayoutMode, PaneManager};
use crate::ui::Ui;

fn draw_panes(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    pane_mgr: &mut PaneManager,
    _image_picker: &Option<ratatui_image::picker::Picker>,
) -> io::Result<()> {
    // inject pane info for status bar display
    let pane_count = pane_mgr.pane_count() as u16;
    let focused_idx = pane_mgr.focused_index();
    if pane_count > 1 {
        // build background alert: list busy non-focused panes
        let alert: Option<String> = {
            let busy: Vec<String> = pane_mgr
                .panes()
                .iter()
                .enumerate()
                .filter(|(i, p)| *i != focused_idx && p.app.is_busy())
                .map(|(i, _)| format!("pane {}", i + 1))
                .collect();
            if busy.is_empty() {
                None
            } else {
                Some(format!("{}: busy", busy.join(", ")))
            }
        };
        for (i, pane) in pane_mgr.panes_mut().iter_mut().enumerate() {
            pane.app.pane_info = Some(((i + 1) as u16, pane_count));
            pane.app.background_alert = if i == focused_idx {
                alert.clone()
            } else {
                None
            };
        }
    } else {
        pane_mgr.panes_mut()[0].app.pane_info = None;
        pane_mgr.panes_mut()[0].app.background_alert = None;
    }
    terminal.draw(|frame| {
        let area = frame.area();
        let mode = pane_mgr.compute_layout(area);
        let focused_idx = pane_mgr.focused_index();
        let pane_count = pane_mgr.pane_count();

        // tab bar in tabs mode
        if mode == LayoutMode::Tabs && pane_count > 1 {
            let tab_area = ratatui::layout::Rect::new(area.x, area.y, area.width, 1);
            frame.render_widget(crate::widgets::tab_bar::TabBar::new(&*pane_mgr), tab_area);
        }

        // cursor position for focused pane (computed before mutable iteration)
        let focused_area = pane_mgr.panes()[focused_idx].area;
        let (cx, cy) = Ui::new(&pane_mgr.panes()[focused_idx].app).cursor_position(focused_area);

        // draw column separators between panes
        if mode == LayoutMode::Columns && pane_count > 1 {
            let buf = frame.buffer_mut();
            for (i, pane) in pane_mgr.panes().iter().enumerate() {
                if i == 0 {
                    continue;
                }
                let sep_x = pane.area.x.saturating_sub(1);
                let is_adjacent_to_focus = i == focused_idx || i == focused_idx + 1;
                let style = if is_adjacent_to_focus {
                    ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray)
                } else {
                    ratatui::style::Style::default().fg(ratatui::style::Color::Rgb(50, 50, 50))
                };
                for y in area.y..area.y + area.height {
                    if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(sep_x, y)) {
                        cell.set_symbol("│").set_style(style);
                    }
                }
            }
        }

        // render each visible pane
        for (i, pane) in pane_mgr.panes_mut().iter_mut().enumerate() {
            if mode == LayoutMode::Tabs && i != focused_idx {
                continue;
            }
            let pane_area = pane.area;
            frame.render_widget(Ui::new(&pane.app), pane_area);

            // render inline images
            let render_areas = pane.app.image_render_areas.borrow().clone();
            for img_area in &render_areas {
                if let Some(proto) = pane
                    .image_protos
                    .get_mut(&(img_area.msg_idx, img_area.tc_idx))
                {
                    let widget = ratatui_image::StatefulImage::new()
                        .resize(ratatui_image::Resize::Fit(None));
                    frame.render_stateful_widget(widget, img_area.area, proto);
                }
            }
        }

        // cursor for focused pane
        let focused_app = &pane_mgr.panes()[focused_idx].app;
        let streaming_idle = focused_app.is_busy() && focused_app.input.is_empty();
        if !streaming_idle
            && (focused_app.mode == app::AppMode::Normal
                || focused_app.mode == app::AppMode::SlashComplete
                || (focused_app.is_streaming && focused_app.mode != app::AppMode::ToolConfirm))
        {
            frame.set_cursor_position((cx, cy));
        }

        // overlays render on top of the focused pane
        if let Some(ref picker) = focused_app.session_picker {
            widgets::session_picker::render(frame, picker);
        }
        if let Some(ref menu) = focused_app.slash_menu {
            let input_h = crate::ui::input_height(
                &focused_app.input,
                focused_area.width,
                &focused_app.pending_images,
            );
            let tools_h = crate::widgets::tool_panels::tool_panels_height(
                &focused_app.active_tools,
                focused_area.width,
            );
            let status_h =
                crate::widgets::status_bar::status_bar_height(focused_app, focused_area.width);
            let regions = crate::ui::layout(focused_area, input_h, tools_h, status_h);
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

/// fork the focused pane's conversation into a new pane.
/// if the input has text, it becomes the new pane's prompt.
async fn fork_pane(
    pane_mgr: &mut PaneManager,
    tui_config: &TuiConfig,
    bus: &crate::messaging::MessageBus,
    tool_output_live: &Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
) {
    // take input text as the new pane's prompt (if any)
    let prompt = {
        let app = &mut pane_mgr.focused_mut().app;
        let text = std::mem::take(&mut app.input);
        app.cursor = 0;
        if text.is_empty() {
            None
        } else {
            Some(slash::expand_template(&text))
        }
    };

    // clone state from parent, with context isolation for focused work
    let (
        conversation,
        session_tree,
        display_msgs,
        model_id,
        context_window,
        thinking_level,
        thinking_display,
        completions,
        slash_commands,
        model_completions,
        cwd,
    ) = {
        let parent = pane_mgr.focused();
        (
            event_handler::slim_for_fork(&parent.conversation),
            parent.session_tree.clone(),
            parent.app.messages.clone(),
            parent.app.model_id.clone(),
            parent.app.stats.context_window,
            parent.app.thinking_level,
            parent.app.thinking_display,
            parent.app.completions.clone(),
            parent.app.slash_commands.clone(),
            parent.app.model_completions.clone(),
            parent.app.cwd.clone(),
        )
    };

    let new_id = pane_mgr.next_id();
    let mut new_app = App::new(model_id.clone(), context_window);
    new_app.messages = display_msgs;
    new_app.thinking_level = thinking_level;
    new_app.thinking_display = thinking_display;
    new_app.completions = completions;
    new_app.slash_commands = slash_commands;
    new_app.model_completions = model_completions;
    new_app.cwd = cwd.clone();
    // copy options that should carry over
    new_app.show_cost = tui_config.show_cost;

    let mut new_pane =
        crate::pane::Pane::with_conversation(new_id, new_app, conversation, session_tree);

    // apply VCS isolation based on configured mode
    let cwd_path = std::path::PathBuf::from(&cwd);
    match tui_config.isolation_mode {
        crate::file_tracker::IsolationMode::Worktree => {
            match crate::isolation::create_worktree(&cwd_path, new_id).await {
                Ok(info) => {
                    // create per-pane tools rooted at the worktree.
                    // convert the tool_output_live mutex into a sink closure
                    let sink: Option<mush_tools::bash::OutputSink> =
                        tool_output_live.as_ref().map(|live| {
                            let live = live.clone();
                            let sink: mush_tools::bash::OutputSink =
                                std::sync::Arc::new(move |line: &str| {
                                    if let Ok(mut guard) = live.lock() {
                                        *guard = Some(line.to_string());
                                    }
                                });
                            sink
                        });
                    let pane_tools = mush_tools::builtin_tools_with_sink(
                        info.path.clone(),
                        sink,
                    );
                    new_pane.tools = Some(pane_tools);
                    new_pane.cwd_override = Some(info.path.clone());
                    new_pane.app.cwd = info.path.display().to_string();
                    new_pane.isolation = Some(crate::isolation::PaneIsolation::Worktree {
                        path: info.path,
                        branch: info.branch,
                    });
                }
                Err(e) => {
                    tracing::warn!("failed to create worktree: {e}");
                    // fall back to none mode
                }
            }
        }
        crate::file_tracker::IsolationMode::Jj => {
            match crate::isolation::create_jj_change(&cwd_path).await {
                Ok(info) => {
                    new_pane.isolation = Some(crate::isolation::PaneIsolation::Jj {
                        change_id: info.change_id,
                    });
                }
                Err(e) => {
                    tracing::warn!("failed to create jj change: {e}");
                }
            }
        }
        crate::file_tracker::IsolationMode::None => {}
    }

    // register new pane with message bus
    let inbox = bus.register(new_id);
    new_pane.inbox = Some(inbox);

    if let Some(ref text) = prompt {
        new_pane.app.push_user_message(text.clone());
        new_pane.app.active_tools.clear();
        new_pane.app.start_streaming();
        new_pane.pending_prompt = Some(text.clone());
        new_pane.label = Some(text.chars().take(20).collect());
    }

    let isolation_label = match &new_pane.isolation {
        Some(crate::isolation::PaneIsolation::Worktree { branch, .. }) => {
            format!(" (worktree: {branch})")
        }
        Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
            let short = &change_id[..change_id.len().min(8)];
            format!(" (jj: {short})")
        }
        None => String::new(),
    };

    let idx = pane_mgr.add_pane(new_pane);
    pane_mgr.focus_index(idx);

    pane_mgr.focused_mut().app.status = Some(if prompt.is_some() {
        format!("forked conversation{isolation_label}")
    } else {
        format!("forked conversation (idle){isolation_label}")
    });
}

/// clean up VCS isolation state when a pane is closed
async fn cleanup_pane_isolation(
    cwd: &std::path::Path,
    isolation: &Option<crate::isolation::PaneIsolation>,
) {
    match isolation {
        Some(crate::isolation::PaneIsolation::Worktree { .. }) => {
            // extract pane id from the branch name
            if let Some(crate::isolation::PaneIsolation::Worktree { path, .. }) = isolation {
                // parse pane id from path (pane-N)
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if let Some(id_str) = name.strip_prefix("pane-") {
                        if let Ok(id) = id_str.parse::<u32>() {
                            if let Err(e) = crate::isolation::remove_worktree(
                                cwd,
                                crate::pane::PaneId::new(id),
                            )
                            .await
                            {
                                tracing::warn!("failed to remove worktree: {e}");
                            }
                        }
                    }
                }
            }
        }
        Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
            // abandon the jj change (discard work)
            if let Err(e) = crate::isolation::abandon_jj_change(cwd, change_id).await {
                tracing::warn!("failed to abandon jj change: {e}");
            }
        }
        None => {}
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

fn restore_terminal_state() {
    use std::io::Write;
    let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    let _ = io::stdout().execute(SetCursorStyle::DefaultUserShape);
    let _ = disable_raw_mode();
    disable_mouse_scroll();
    let _ = io::stdout().execute(LeaveAlternateScreen);
    let _ = io::stdout().execute(Show);
    let _ = io::stdout().execute(crossterm::style::ResetColor);
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

fn cleanup(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    restore_terminal_state();
    terminal.show_cursor()?;
    Ok(())
}

/// drain each pane's inter-pane message inbox and inject into steering queues.
/// for idle panes, sets pending_prompt to auto-wake the agent loop.
async fn drain_inboxes(pane_mgr: &mut PaneManager) {
    for pane in pane_mgr.panes_mut() {
        let Some(ref mut inbox) = pane.inbox else {
            continue;
        };
        while let Ok(msg) = inbox.try_recv() {
            // structured envelope in the injected text
            let task_suffix = msg
                .task_id
                .as_deref()
                .map(|t| format!(" task={t}"))
                .unwrap_or_default();
            let text = format!(
                "[{} from pane {}{}]: {}",
                msg.intent,
                msg.from.as_u32(),
                task_suffix,
                msg.content,
            );
            let display = format!(
                "↶ {} from pane {}{}: {}",
                msg.intent,
                msg.from.as_u32(),
                task_suffix,
                msg.content,
            );
            // if pane has an active agent (streaming), inject via steering
            if pane.app.is_streaming {
                let steering_msg = Message::User(UserMessage {
                    content: UserContent::Text(text.clone()),
                    timestamp_ms: msg.timestamp,
                });
                pane.steering_queue.lock().await.push(steering_msg);
                pane.app.push_system_message(display);
            } else {
                // idle pane: show the message and auto-wake
                pane.app.push_system_message(display);
                pane.app.push_user_message(text.clone());
                pane.app.start_streaming();
                pane.pending_prompt = Some(text);
            }
        }
    }
}

/// build system prompt fragment describing sibling panes
fn build_sibling_prompt(pane_id: crate::pane::PaneId, pane_mgr: &PaneManager) -> String {
    let panes = pane_mgr.panes();
    let total = panes.len();
    let idx = panes
        .iter()
        .position(|p| p.id == pane_id)
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut prompt = format!(
        "You are pane {idx} of {total} agents working in parallel on the same codebase."
    );
    prompt.push_str(
        " You have a `send_message` tool to communicate with siblings. \
         Use it to share findings, avoid duplicating work, or ask for help.",
    );

    for p in panes {
        if p.id == pane_id {
            continue;
        }
        let p_idx = panes
            .iter()
            .position(|pp| pp.id == p.id)
            .map(|i| i + 1)
            .unwrap_or(0);
        let label = p
            .label
            .as_deref()
            .or_else(|| {
                p.app
                    .messages
                    .last()
                    .filter(|m| m.role == crate::app::MessageRole::User)
                    .map(|m| m.content.as_str())
            })
            .unwrap_or("idle");
        // truncate label for prompt space
        let label_preview: String = label.chars().take(60).collect();
        prompt.push_str(&format!("\n- Pane {p_idx}: {label_preview}"));
    }

    prompt
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
