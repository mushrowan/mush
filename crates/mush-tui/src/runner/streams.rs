use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;

use futures::stream::SelectAll;
use mush_agent::tool::{SharedTool, ToolRegistry};
use mush_agent::{AgentConfig, AgentEvent, agent_loop};
use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    ImageContent, Message, Model, StreamOptions, TextContent, ThinkingLevel, Timestamp, TokenCount,
    ToolCallId, UserContent, UserContentPart, UserMessage,
};
use tokio::sync::Mutex;

use crate::event_handler;
use crate::file_tracker::FileTracker;
use crate::pane::{PaneId, PaneManager};

use super::{HintMode, PromptEnricher, TuiConfig};

pub(super) type TaggedStream<'a> =
    Pin<Box<dyn futures::Stream<Item = (PaneId, AgentEvent)> + Send + 'a>>;
pub(super) type AgentStreams<'a> = SelectAll<TaggedStream<'a>>;
pub(super) type ConfirmReply = tokio::sync::oneshot::Sender<bool>;

pub(super) struct ConfirmRequest {
    pub tool_call_id: ToolCallId,
    pub prompt: String,
    pub reply: ConfirmReply,
}

pub(super) struct PendingConfirmation {
    pub tool_call_id: ToolCallId,
    pub reply: ConfirmReply,
}

pub(super) fn new_agent_streams<'a>() -> AgentStreams<'a> {
    SelectAll::new()
}

pub(super) struct StreamMeta {
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    pub confirm_req_rx: tokio::sync::mpsc::Receiver<ConfirmRequest>,
    pub confirm_reply: Arc<Mutex<Option<PendingConfirmation>>>,
    pub model: Model,
    pub context_tokens: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
pub(super) struct StreamState {
    metas: HashMap<PaneId, StreamMeta>,
    aborted_panes: HashSet<PaneId>,
}

pub(super) struct StreamDeps<'a> {
    pub default_model: Model,
    pub system_prompt: Option<String>,
    pub options: StreamOptions,
    pub max_turns: usize,
    pub prompt_enricher: Option<PromptEnricher>,
    pub hint_mode: HintMode,
    pub provider_api_keys: HashMap<String, String>,
    pub confirm_tools: bool,
    pub auto_compact: bool,
    pub tools: &'a ToolRegistry,
    pub registry: &'a ApiRegistry,
    pub message_bus: &'a crate::messaging::MessageBus,
    pub shared_state: &'a crate::shared_state::SharedState,
    pub file_tracker: &'a FileTracker,
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    pub cwd: std::path::PathBuf,
    pub dynamic_system_context: Option<mush_agent::DynamicContext>,
    pub file_rules: Option<mush_agent::FileRuleCallback>,
    pub lsp_diagnostics: Option<mush_agent::DiagnosticCallback>,
    pub delegation_queue: &'a crate::delegate::DelegationQueue,
}

impl StreamState {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn register_active(&mut self, pane_id: PaneId, meta: StreamMeta) {
        self.aborted_panes.remove(&pane_id);
        self.metas.insert(pane_id, meta);
    }

    pub(super) fn meta(&self, pane_id: PaneId) -> Option<&StreamMeta> {
        self.metas.get(&pane_id)
    }

    pub(super) fn meta_mut(&mut self, pane_id: PaneId) -> Option<&mut StreamMeta> {
        self.metas.get_mut(&pane_id)
    }

    pub(super) fn remove(&mut self, pane_id: PaneId) -> Option<StreamMeta> {
        self.metas.remove(&pane_id)
    }

    pub(super) fn abort(&mut self, pane_id: PaneId) {
        self.aborted_panes.insert(pane_id);
        self.metas.remove(&pane_id);
    }

    pub(super) fn is_aborted(&self, pane_id: PaneId) -> bool {
        self.aborted_panes.contains(&pane_id)
    }

    pub(super) fn finish_aborted(&mut self, pane_id: PaneId) {
        self.aborted_panes.remove(&pane_id);
        self.metas.remove(&pane_id);
    }
}

pub(super) fn take_pending_prompts(
    pane_mgr: &mut PaneManager,
    pending_prompt: &mut Option<String>,
) -> Vec<(PaneId, String)> {
    let mut prompts = Vec::new();
    if let Some(prompt) = pending_prompt.take() {
        prompts.push((pane_mgr.focused().id, prompt));
    }
    for pane in pane_mgr.panes_mut() {
        if let Some(prompt) = pane.pending_prompt.take()
            && !prompts.iter().any(|(id, _)| *id == pane.id)
        {
            prompts.push((pane.id, prompt));
        }
    }
    prompts
}

pub(super) async fn poll_confirmation_prompt(
    pane_mgr: &mut PaneManager,
    stream_state: &mut StreamState,
) {
    let focused_id = pane_mgr.focused().id;
    if let Some(meta) = stream_state.meta_mut(focused_id)
        && let Ok(confirm) = meta.confirm_req_rx.try_recv()
    {
        let app = &mut pane_mgr.focused_mut().app;
        app.mode = crate::app::AppMode::ToolConfirm;
        app.confirm_tool_call_id = Some(confirm.tool_call_id.clone());
        app.confirm_prompt = Some(confirm.prompt);
        *meta.confirm_reply.lock().await = Some(PendingConfirmation {
            tool_call_id: confirm.tool_call_id,
            reply: confirm.reply,
        });
    }
}

pub(super) async fn answer_confirmation(
    pane_mgr: &mut PaneManager,
    stream_state: &mut StreamState,
    allowed: bool,
) {
    let focused_id = pane_mgr.focused().id;
    let pending_tool_call_id = if let Some(meta) = stream_state.meta_mut(focused_id)
        && let Some(pending) = meta.confirm_reply.lock().await.take()
    {
        let tool_call_id = pending.tool_call_id.clone();
        let _ = pending.reply.send(allowed);
        Some(tool_call_id)
    } else {
        None
    };

    let app = &mut pane_mgr.focused_mut().app;
    app.mode = crate::app::AppMode::Normal;
    app.confirm_prompt = None;
    app.confirm_tool_call_id = None;
    if !allowed {
        app.status = Some(match pending_tool_call_id {
            Some(tool_call_id) => format!("tool denied: {tool_call_id}"),
            None => "tool denied".into(),
        });
    }
}

pub(super) fn poll_live_tool_output(
    pane_mgr: &mut PaneManager,
    live: &Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
) {
    if let Some(live) = live {
        let app = &mut pane_mgr.focused_mut().app;
        if let Ok(guard) = live.lock()
            && let Some(last) = guard.as_ref()
            && let Some(active) = app.active_tools.last().map(|t| t.tool_call_id.clone())
        {
            app.push_tool_output(&active, last);
        }
    }
}

pub(super) async fn handle_agent_event_side_effects(
    pane_mgr: &mut PaneManager,
    stream_state: &mut StreamState,
    pane_id: PaneId,
    event: &AgentEvent,
    file_tracker: &FileTracker,
    tui_config: &TuiConfig,
    registry: &ApiRegistry,
) {
    if let AgentEvent::MessageEnd { message } = event
        && let Some(meta) = stream_state.meta(pane_id)
    {
        meta.context_tokens.store(
            message.usage.total_input_tokens().get(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    if pane_mgr.is_multi_pane() {
        match event {
            AgentEvent::ToolExecStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                file_tracker.record_tool_start(pane_id, tool_call_id, tool_name.as_str(), args);
            }
            AgentEvent::ToolExecEnd {
                tool_call_id,
                tool_name: _,
                result,
            } => {
                if let Some(conflict) =
                    file_tracker.record_tool_end(pane_id, tool_call_id, result.outcome.is_success())
                {
                    let others: Vec<String> = conflict
                        .other_panes
                        .iter()
                        .map(|pane_id| pane_id.to_string())
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

    if !matches!(event, AgentEvent::AgentEnd) {
        return;
    }

    stream_state.remove(pane_id);
    crate::notify::play(crate::notify::Sound::Complete);

    let title_request = if let Some(pane) = pane_mgr.pane(pane_id)
        && !pane.title_generated
        && pane.conversation.context_len() >= 2
        && let Some(ref updater) = tui_config.update_title
    {
        Some((pane.conversation.context_prefix(4), updater.clone()))
    } else {
        None
    };

    if let Some((msgs, updater)) = title_request {
        let model = tui_config.model.clone();
        let opts = StreamOptions {
            api_key: tui_config.options.api_key.clone(),
            ..Default::default()
        };
        if let Ok(Some(title)) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mush_session::title::generate_title(msgs, registry, &model, &opts),
        )
        .await
        {
            updater(title);
        }
        if let Some(pane) = pane_mgr.pane_mut(pane_id) {
            pane.title_generated = true;
        }
    }

    if tui_config.cache_timer
        && let Some(pane) = pane_mgr.pane(pane_id)
        && let Some(remaining) = pane.app.cache_remaining_secs()
        && remaining > 60
    {
        crate::notify::send_with_sound(
            "awaiting input",
            &format!("cache warm for {remaining}s"),
            Some(crate::notify::Sound::Attention),
        );
    }

    // auto fork-and-compact: fork the session tree and persist compacted state
    if tui_config.auto_fork_compact {
        auto_fork_compact(pane_mgr, pane_id, tui_config, registry).await;
    }

    if let Some(ref saver) = tui_config.save_session {
        saver(build_session_snapshot(pane_mgr));
    }
}

/// fork the session tree and compact the new branch when the conversation
/// is approaching the context limit. the original uncompacted branch is
/// preserved and navigable via /tree + /branch.
async fn auto_fork_compact(
    pane_mgr: &mut PaneManager,
    pane_id: PaneId,
    tui_config: &TuiConfig,
    registry: &ApiRegistry,
) {
    let Some(pane) = pane_mgr.pane(pane_id) else {
        return;
    };

    let context_tokens = pane.app.stats.context_tokens;
    let context_window = tui_config.model.context_window;
    let message_count = pane.conversation.context_len();

    if !mush_session::compact::needs_compaction_at(context_tokens, context_window, message_count) {
        return;
    }

    let pane = pane_mgr.pane_mut(pane_id).expect("pane exists after check");
    let before = pane.conversation.context_len();

    let result = crate::slash::fork_and_compact(
        &mut pane.conversation,
        "auto-forked",
        &tui_config.model,
        &tui_config.options,
        registry,
        Some(&tui_config.lifecycle_hooks),
        Some(tui_config.cwd.as_path()),
    )
    .await;

    if let Some((after, tokens_before, tokens_after)) = result {
        let pane = pane_mgr.pane_mut(pane_id).expect("pane exists after compact");
        crate::conversation_display::rebuild_display(&mut pane.app, &pane.conversation.context());
        pane.app.status = Some(format!(
            "auto-fork-compacted: {before} → {after} messages, ~{tokens_before} → ~{tokens_after} tokens (original preserved)",
        ));
    }
}

pub(super) async fn abort_focused_stream(
    pane_mgr: &mut PaneManager,
    stream_state: &mut StreamState,
) {
    let focused_id = pane_mgr.focused().id;
    let app = &mut pane_mgr.focused_mut().app;
    app.is_streaming = false;
    app.active_tools.clear();
    app.status = Some("aborted".into());

    stream_state.abort(focused_id);

    let pane = pane_mgr.focused_mut();
    pane.app.finish_streaming(None, None);
    let mut restored = pane.app.take_queued_messages();
    {
        let mut steering_queue = pane.steering_queue.lock().await;
        for message in steering_queue.drain(..) {
            if let Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) = message
                && !text.is_empty()
            {
                restored.push(text);
            }
        }
    }
    if !restored.is_empty() {
        let text = restored.join("\n");
        pane.app.input = text.clone();
        pane.app.cursor = text.len();
    }
}

pub(super) async fn submit_streaming_input(
    pane_mgr: &mut PaneManager,
    stream_state: &StreamState,
    text: String,
) {
    let focused_id = pane_mgr.focused().id;
    if let Some(meta) = stream_state.meta(focused_id) {
        meta.steering_queue
            .lock()
            .await
            .push(Message::User(UserMessage {
                content: UserContent::Text(text.clone()),
                timestamp_ms: Timestamp::now(),
            }));
        pane_mgr.focused_mut().app.push_queued_message(text);
    } else {
        let pane = pane_mgr.focused_mut();
        pane.app.push_user_message(text.clone());
        pane.app.active_tools.clear();
        pane.app.start_streaming();
        pane.pending_prompt = Some(text);
    }
}

pub(super) async fn edit_last_queued_steering(
    pane_mgr: &mut PaneManager,
    stream_state: &StreamState,
) {
    let focused_id = pane_mgr.focused().id;
    let app = &mut pane_mgr.focused_mut().app;

    if let Some(queued_text) = app.pop_last_queued_message() {
        let current_input = app.take_input();
        if !current_input.trim().is_empty() {
            app.push_queued_message(&current_input);
            if let Some(meta) = stream_state.meta(focused_id) {
                meta.steering_queue
                    .lock()
                    .await
                    .push(Message::User(UserMessage {
                        content: UserContent::Text(current_input),
                        timestamp_ms: Timestamp::now(),
                    }));
            }
        }

        app.input.clone_from(&queued_text);
        app.cursor = app.input.len();
        app.ensure_cursor_visible();

        if let Some(meta) = stream_state.meta(focused_id) {
            let mut steering_queue = meta.steering_queue.lock().await;
            if let Some(pos) = steering_queue.iter().rposition(|message| {
                if let Message::User(user_message) = message {
                    user_message.content.text() == queued_text
                } else {
                    false
                }
            }) {
                steering_queue.remove(pos);
            }
        }
    } else {
        app.status = Some("no queued messages to edit".into());
    }
}

pub(super) async fn start_pending_streams<'a>(
    agent_streams: &mut AgentStreams<'a>,
    stream_state: &mut StreamState,
    pane_mgr: &mut PaneManager,
    pending_prompt: &mut Option<String>,
    deps: StreamDeps<'a>,
) {
    for (pane_id, prompt) in take_pending_prompts(pane_mgr, pending_prompt) {
        start_stream_for_prompt(
            agent_streams,
            stream_state,
            pane_mgr,
            pane_id,
            prompt,
            &deps,
        )
        .await;
    }
}

async fn start_stream_for_prompt<'a>(
    agent_streams: &mut AgentStreams<'a>,
    stream_state: &mut StreamState,
    pane_mgr: &mut PaneManager,
    pane_id: PaneId,
    prompt: String,
    deps: &StreamDeps<'a>,
) {
    let tools = deps.tools;
    let registry = deps.registry;
    let message_bus = deps.message_bus;
    let shared_state = deps.shared_state;
    let file_tracker = deps.file_tracker;
    let steering_queue = pane_mgr.pane(pane_id).unwrap().steering_queue.clone();
    let model = pane_model(pane_mgr, pane_id, &deps.default_model);
    let thinking_level = pane_thinking_level(pane_mgr, pane_id);
    let conversation_snapshot = {
        let pane = pane_mgr.pane_mut(pane_id).unwrap();
        let (app, conversation, _) = pane.fields_mut();
        append_prompt_and_snapshot(
            app,
            conversation,
            prompt,
            deps.hint_mode,
            &deps.prompt_enricher,
        )
    };

    let context_window = model.context_window;
    let enricher_arc = if deps.hint_mode == HintMode::Transform {
        deps.prompt_enricher.clone()
    } else {
        None
    };
    let compact_model = model.clone();
    let compact_options = deps.options.clone();
    let do_auto_compact = deps.auto_compact;
    let initial_ctx = pane_mgr
        .pane(pane_id)
        .map(|pane| pane.app.stats.context_tokens.get())
        .unwrap_or(0);
    let context_tokens_shared = Arc::new(std::sync::atomic::AtomicU64::new(initial_ctx));
    let transform = build_context_transform(
        enricher_arc,
        compact_model,
        compact_options,
        do_auto_compact,
        context_window,
        registry,
        context_tokens_shared.clone(),
        deps.lifecycle_hooks.clone(),
        deps.cwd.clone(),
    );

    let steering = build_steering_callback(steering_queue.clone());
    let follow_up = build_steering_callback(steering_queue.clone());
    let (confirm_req_rx, confirm) = build_confirm_callback(
        pane_id,
        deps.confirm_tools,
        pane_mgr.is_multi_pane(),
        file_tracker,
    );

    let mut call_options = deps.options.clone();
    let (api_key, account_id) =
        event_handler::resolve_auth_for_model(&model, &deps.provider_api_keys).await;
    call_options.api_key = api_key;
    call_options.account_id = account_id;
    call_options.thinking = Some(thinking_level);

    let extra_tools =
        build_extra_tools(pane_mgr.is_multi_pane(), pane_id, message_bus, shared_state, deps.delegation_queue);
    let system_prompt = build_system_prompt(pane_mgr, pane_id, &deps.system_prompt);
    let pane_tools = pane_mgr
        .pane_mut(pane_id)
        .and_then(|pane| pane.tools.take());

    let mut tool_registry = tools.clone();
    if let Some(pane_tools) = pane_tools {
        tool_registry = tool_registry.with_shared(pane_tools.iter().cloned());
    }
    tool_registry.extend_shared(extra_tools);

    stream_state.register_active(
        pane_id,
        StreamMeta {
            steering_queue,
            confirm_req_rx,
            confirm_reply: Arc::new(Mutex::new(None)),
            model: model.clone(),
            context_tokens: context_tokens_shared,
        },
    );

    let config = AgentConfig {
        model,
        system_prompt,
        tools: tool_registry,
        registry,
        options: call_options,
        max_turns: deps.max_turns,
        hooks: mush_agent::AgentHooks {
            get_steering: steering,
            get_follow_up: follow_up,
            transform_context: transform,
            confirm_tool: confirm,
        },
        lifecycle_hooks: deps.lifecycle_hooks.clone(),
        cwd: Some(deps.cwd.clone()),
        dynamic_system_context: deps.dynamic_system_context.clone(),
        file_rules: deps.file_rules.clone(),
        lsp_diagnostics: deps.lsp_diagnostics.clone(),
    };

    let stream = agent_loop(config, conversation_snapshot);
    let tagged: TaggedStream<'a> = Box::pin(futures::StreamExt::map(stream, move |event| {
        (pane_id, event)
    }));
    agent_streams.push(tagged);
}

fn pane_model(pane_mgr: &PaneManager, pane_id: PaneId, default_model: &Model) -> Model {
    pane_mgr
        .pane(pane_id)
        .map(|pane| {
            models::find_model_by_id(pane.app.model_id.as_str())
                .unwrap_or_else(|| default_model.clone())
        })
        .unwrap_or_else(|| default_model.clone())
}

fn pane_thinking_level(pane_mgr: &PaneManager, pane_id: PaneId) -> ThinkingLevel {
    pane_mgr
        .pane(pane_id)
        .map(|pane| pane.app.thinking_level)
        .unwrap_or(ThinkingLevel::Off)
}

fn append_prompt_and_snapshot(
    app: &mut crate::app::App,
    conversation: &mut mush_session::ConversationState,
    prompt: String,
    hint_mode: HintMode,
    prompt_enricher: &Option<PromptEnricher>,
) -> Vec<Message> {
    let prompt_preview = prompt.clone();
    let mut injection_preview: Option<String> = None;
    let user_text = if hint_mode == HintMode::Message
        && let Some(ref enricher) = *prompt_enricher
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
        && hint_mode == HintMode::Transform
        && let Some(ref enricher) = *prompt_enricher
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
            let note = match hint_mode {
                HintMode::None => "no injection (hint mode is none)",
                _ if prompt_enricher.is_none() => "no injection (enricher unavailable)",
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
            for image in images {
                use base64::Engine;

                parts.push(UserContentPart::Image(ImageContent {
                    data: base64::engine::general_purpose::STANDARD.encode(&image.data),
                    mime_type: image.mime_type,
                }));
            }
            UserContent::Parts(parts)
        },
        timestamp_ms: Timestamp::now(),
    });
    conversation.append_message(user_message);
    conversation.context()
}

fn build_context_transform<'a>(
    enricher_arc: Option<PromptEnricher>,
    compact_model: Model,
    compact_options: StreamOptions,
    do_auto_compact: bool,
    context_window: TokenCount,
    registry: &'a ApiRegistry,
    context_tokens_shared: Arc<std::sync::atomic::AtomicU64>,
    lifecycle_hooks: mush_agent::LifecycleHooks,
    cwd: std::path::PathBuf,
) -> Option<mush_agent::ContextTransform<'a>> {
    let ctx_tokens_for_transform = context_tokens_shared;
    #[expect(clippy::type_complexity)]
    let compaction_cache: std::sync::Arc<tokio::sync::Mutex<Option<(usize, Vec<Message>)>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let hooks = Arc::new(lifecycle_hooks);
    let cwd = Arc::new(cwd);

    Some(Box::new(move |messages| {
        let hint_match = enricher_arc
            .as_ref()
            .is_some_and(|enricher| event_handler::would_inject_hint(messages, enricher.as_ref()));
        let needs_mask = event_handler::needs_observation_mask(messages);
        let owned_messages = if do_auto_compact || hint_match || needs_mask {
            Some(messages.to_vec())
        } else {
            None
        };
        let enricher = enricher_arc.clone();
        let model = compact_model.clone();
        let options = compact_options.clone();
        let cache = compaction_cache.clone();
        let ctx_tokens = ctx_tokens_for_transform.clone();
        let hooks = hooks.clone();
        let cwd = cwd.clone();
        Box::pin(async move {
            let mut replayed_cached_compaction = false;
            let mut fresh_compaction = false;
            let mut maybe_msgs: Option<Vec<Message>> = None;

            if do_auto_compact {
                let messages = owned_messages
                    .as_ref()
                    .expect("owned messages required for context transform");
                let mut guard = cache.lock().await;
                let current_tokens =
                    TokenCount::new(ctx_tokens.load(std::sync::atomic::Ordering::Relaxed));

                let msgs = if let Some((orig_len, ref compacted)) = *guard {
                    if messages.len() >= orig_len {
                        replayed_cached_compaction = true;
                        let mut result = compacted.clone();
                        result.extend(messages[orig_len..].iter().cloned());
                        result
                    } else {
                        *guard = None;
                        messages.clone()
                    }
                } else {
                    messages.clone()
                };

                let pre_len = msgs.len();
                let compacted = event_handler::auto_compact(
                    msgs,
                    current_tokens,
                    context_window,
                    registry,
                    &model,
                    &options,
                    Some(&hooks),
                    Some(cwd.as_path()),
                )
                .await;
                if compacted.len() < pre_len {
                    *guard = Some((pre_len, compacted.clone()));
                    replayed_cached_compaction = false;
                    fresh_compaction = true;
                }
                maybe_msgs = Some(compacted);
            }

            if !fresh_compaction && !replayed_cached_compaction && !hint_match && !needs_mask {
                return mush_agent::ContextTransformResult::Unchanged;
            }

            let mut msgs = maybe_msgs.unwrap_or_else(|| {
                owned_messages.expect("owned messages required for context transform")
            });
            let mut changed = fresh_compaction || replayed_cached_compaction;

            if let Some(ref enricher) = enricher {
                changed |= event_handler::inject_hint(&mut msgs, enricher.as_ref());
            }
            changed |= event_handler::mask_observations(&mut msgs);

            if fresh_compaction {
                mush_agent::ContextTransformResult::Updated(msgs)
            } else if changed {
                mush_agent::ContextTransformResult::Silent(msgs)
            } else {
                mush_agent::ContextTransformResult::Unchanged
            }
        })
    }))
}

fn build_steering_callback<'a>(
    steering_queue: Arc<Mutex<Vec<Message>>>,
) -> Option<mush_agent::MessageCallback<'a>> {
    Some(Box::new(move || {
        let steering_queue = steering_queue.clone();
        Box::pin(async move {
            let mut queue = steering_queue.lock().await;
            queue.drain(..).collect()
        })
    }))
}

fn build_confirm_callback<'a>(
    pane_id: PaneId,
    confirm_tools: bool,
    is_multi_pane: bool,
    file_tracker: &FileTracker,
) -> (
    tokio::sync::mpsc::Receiver<ConfirmRequest>,
    Option<mush_agent::ConfirmCallback<'a>>,
) {
    let (confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel::<ConfirmRequest>(1);
    let confirm: Option<mush_agent::ConfirmCallback<'a>> = if confirm_tools || is_multi_pane {
        let file_tracker = file_tracker.clone();
        Some(Box::new(
            move |tool_call_id: &ToolCallId,
                  name: &str,
                  args: &serde_json::Value|
                  -> std::pin::Pin<
                Box<dyn std::future::Future<Output = mush_agent::ConfirmAction> + Send>,
            > {
                let file_tracker = file_tracker.clone();
                let tx = confirm_req_tx.clone();
                let summary = mush_agent::summarise_tool_args(name, args);
                let prompt = format!("{name} {summary}");
                let tool_call_id = tool_call_id.clone();
                let name = name.to_string();
                let args = args.clone();
                Box::pin(async move {
                    if matches!(name.as_str(), "write" | "edit")
                        && let Some(path) = args["path"].as_str()
                        && let Some(owner) = file_tracker.check_lock(pane_id, path)
                    {
                        return mush_agent::ConfirmAction::DenyWithReason(format!(
                            "file \"{}\" is locked by pane {}",
                            path,
                            owner.as_u32()
                        ));
                    }
                    if confirm_tools {
                        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                        if tx
                            .send(ConfirmRequest {
                                tool_call_id,
                                prompt,
                                reply: resp_tx,
                            })
                            .await
                            .is_err()
                        {
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
            },
        ) as mush_agent::ConfirmCallback<'a>)
    } else {
        None
    };

    (confirm_req_rx, confirm)
}

fn build_extra_tools(
    is_multi_pane: bool,
    pane_id: PaneId,
    message_bus: &crate::messaging::MessageBus,
    shared_state: &crate::shared_state::SharedState,
    delegation_queue: &crate::delegate::DelegationQueue,
) -> Vec<SharedTool> {
    let mut extra_tools = Vec::new();
    if is_multi_pane {
        extra_tools.push(Arc::new(crate::messaging::SendMessageTool {
            sender_id: pane_id,
            bus: message_bus.clone(),
        }) as SharedTool);
        extra_tools.push(Arc::new(crate::shared_state::ReadStateTool {
            state: shared_state.clone(),
        }) as SharedTool);
        extra_tools.push(Arc::new(crate::shared_state::WriteStateTool {
            state: shared_state.clone(),
        }) as SharedTool);
    }
    // delegate_task always available (spawns a new pane even from single-pane)
    extra_tools.push(Arc::new(crate::delegate::DelegateTaskTool {
        sender_id: pane_id,
        queue: delegation_queue.clone(),
    }) as SharedTool);
    extra_tools
}

fn build_system_prompt(
    pane_mgr: &PaneManager,
    pane_id: PaneId,
    base_system_prompt: &Option<String>,
) -> Option<String> {
    let mut system_prompt = if pane_mgr.is_multi_pane() {
        let sibling_info = super::panes::build_sibling_prompt(pane_id, pane_mgr);
        match base_system_prompt.as_ref() {
            Some(base) => Some(format!("{base}\n\n{sibling_info}")),
            None => Some(sibling_info),
        }
    } else {
        base_system_prompt.clone()
    };

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
                system_prompt =
                    Some(system_prompt.map_or(note.clone(), |prompt| format!("{prompt}{note}")));
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
                system_prompt =
                    Some(system_prompt.map_or(note.clone(), |prompt| format!("{prompt}{note}")));
            }
            None => {}
        }
    }

    system_prompt
}

/// build a snapshot of all pane conversations for session persistence
pub(crate) fn build_session_snapshot(pane_mgr: &PaneManager) -> super::SessionSnapshot {
    let primary = pane_mgr.focused();
    let additional: Vec<super::PaneSnapshot> = pane_mgr
        .panes()
        .iter()
        .filter(|p| p.id != primary.id)
        .map(|p| super::PaneSnapshot {
            pane_id: p.id,
            label: p.label.clone(),
            model_id: p.app.model_id.to_string(),
            conversation: p.conversation.clone(),
        })
        .collect();

    super::SessionSnapshot {
        primary: primary.conversation.clone(),
        model_id: primary.app.model_id.to_string(),
        panes: additional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mush_ai::types::{ModelId, ToolCallId};

    use crate::app::App;
    use crate::pane::Pane;

    fn app() -> App {
        App::new(ModelId::from("test-model"), TokenCount::new(4096))
    }

    #[test]
    fn take_pending_prompts_deduplicates_focused_pane() {
        let mut pane_mgr = PaneManager::new(Pane::new(PaneId::new(1), app()));
        pane_mgr.add_pane(Pane::new(PaneId::new(2), app()));

        let mut pending_prompt = Some("focused".to_string());
        pane_mgr.focused_mut().pending_prompt = Some("duplicate".to_string());
        pane_mgr.panes_mut()[1].pending_prompt = Some("other".to_string());

        let prompts = take_pending_prompts(&mut pane_mgr, &mut pending_prompt);

        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0], (PaneId::new(1), "focused".to_string()));
        assert_eq!(prompts[1], (PaneId::new(2), "other".to_string()));
        assert!(pending_prompt.is_none());
        assert!(pane_mgr.focused().pending_prompt.is_none());
        assert!(pane_mgr.panes()[1].pending_prompt.is_none());
    }

    #[tokio::test]
    async fn register_active_clears_abort_marker() {
        let mut stream_state = StreamState::new();
        let pane_id = PaneId::new(7);
        stream_state.abort(pane_id);
        assert!(stream_state.is_aborted(pane_id));

        let (_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
        );

        assert!(!stream_state.is_aborted(pane_id));
        assert!(stream_state.meta(pane_id).is_some());
        let _ = ToolCallId::from("unused");
    }

    #[tokio::test]
    async fn poll_confirmation_prompt_tracks_tool_call_id() {
        let pane_id = PaneId::new(1);
        let mut pane_mgr = PaneManager::new(Pane::new(pane_id, app()));
        let mut stream_state = StreamState::new();
        let (confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);

        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
        );

        let tool_call_id = ToolCallId::from("tc_1");
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        confirm_req_tx
            .send(ConfirmRequest {
                tool_call_id: tool_call_id.clone(),
                prompt: "read todo.md".into(),
                reply: reply_tx,
            })
            .await
            .unwrap();

        poll_confirmation_prompt(&mut pane_mgr, &mut stream_state).await;

        let app = &pane_mgr.focused().app;
        assert_eq!(app.mode, crate::app::AppMode::ToolConfirm);
        assert_eq!(app.confirm_prompt.as_deref(), Some("read todo.md"));
        assert_eq!(app.confirm_tool_call_id.as_ref(), Some(&tool_call_id));

        let pending = stream_state
            .meta(pane_id)
            .unwrap()
            .confirm_reply
            .lock()
            .await;
        assert_eq!(
            pending.as_ref().map(|pending| &pending.tool_call_id),
            Some(&tool_call_id)
        );
    }

    #[tokio::test]
    async fn answer_confirmation_clears_tool_call_id_and_sends_reply() {
        let pane_id = PaneId::new(1);
        let mut pane_mgr = PaneManager::new(Pane::new(pane_id, app()));
        let mut stream_state = StreamState::new();
        let (confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);

        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
        );

        let tool_call_id = ToolCallId::from("tc_2");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        confirm_req_tx
            .send(ConfirmRequest {
                tool_call_id: tool_call_id.clone(),
                prompt: "edit todo.md".into(),
                reply: reply_tx,
            })
            .await
            .unwrap();

        poll_confirmation_prompt(&mut pane_mgr, &mut stream_state).await;
        answer_confirmation(&mut pane_mgr, &mut stream_state, true).await;

        assert!(reply_rx.await.unwrap());
        let app = &pane_mgr.focused().app;
        assert_eq!(app.mode, crate::app::AppMode::Normal);
        assert!(app.confirm_prompt.is_none());
        assert!(app.confirm_tool_call_id.is_none());
        assert!(
            stream_state
                .meta(pane_id)
                .unwrap()
                .confirm_reply
                .lock()
                .await
                .is_none()
        );
    }

    #[test]
    fn poll_live_tool_output_updates_focused_tool() {
        let pane_id = PaneId::new(1);
        let mut pane_mgr = PaneManager::new(Pane::new(pane_id, app()));
        let tool_call_id = ToolCallId::from("tc_live");
        pane_mgr
            .focused_mut()
            .app
            .start_tool(&tool_call_id, "bash", "cargo test");

        let live = Some(std::sync::Arc::new(std::sync::Mutex::new(Some(
            "running".to_string(),
        ))));
        poll_live_tool_output(&mut pane_mgr, &live);

        assert_eq!(
            pane_mgr.focused().app.active_tools[0]
                .live_output
                .as_deref(),
            Some("running")
        );
    }
}
