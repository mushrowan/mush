use std::collections::HashMap;
use std::sync::Arc;

use mush_agent::tool::{SharedTool, ToolRegistry};
use mush_agent::{AgentConfig, AgentEvent, agent_loop};
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    ImageContent, Message, Model, Provider, StreamOptions, TextContent, ThinkingLevel, Timestamp,
    TokenCount, ToolCallId, ToolOutcome, ToolResultMessage, UserContent, UserContentPart,
    UserMessage,
};
use tokio::sync::Mutex;

use crate::event_handler;
use crate::runner::runtime::save_pane_pending_prompt;
use crate::session::{PendingPrompt, Session};
use mush_ai::types::PaneId;

const USER_CANCELLED_TOOL_MESSAGE: &str = "user cancelled tool";

use super::TuiConfig;

pub(super) use mush_runtime::StreamGeneration;

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

pub(super) struct StreamMeta {
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    pub confirm_req_rx: tokio::sync::mpsc::Receiver<ConfirmRequest>,
    pub confirm_reply: Arc<Mutex<Option<PendingConfirmation>>>,
    pub model: Model,
    pub context_tokens: Arc<std::sync::atomic::AtomicU64>,
    pub cancel: tokio_util::sync::CancellationToken,
}

const SESSION_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(not(test))]
fn default_notification_sink(summary: &str, body: &str, sound: Option<crate::notify::Sound>) {
    crate::notify::send_with_sound(summary, body, sound);
}

#[cfg(test)]
fn default_notification_sink(_: &str, _: &str, _: Option<crate::notify::Sound>) {}

pub(super) type NotificationSink =
    Arc<dyn Fn(&str, &str, Option<crate::notify::Sound>) + Send + Sync>;

pub(super) struct StreamState {
    metas: HashMap<PaneId, StreamMeta>,
    generations: HashMap<PaneId, StreamGeneration>,
    next_gen: u64,
    last_session_save: std::time::Instant,
    session_dirty: bool,
    pub(super) notify: NotificationSink,
    #[cfg(feature = "signal")]
    pub(super) signal: Option<mush_notify::Dispatcher>,
    #[cfg(feature = "signal")]
    completion_sequence: u64,
}

pub(super) struct StreamConfig {
    pub default_model: Model,
    pub system_prompt: Option<String>,
    pub options: StreamOptions,
    pub max_turns: usize,
    pub provider_api_keys: HashMap<String, mush_ai::ApiKey>,
    pub confirm_tools: bool,
    pub auto_compact: bool,
    pub compaction_model: Option<(Model, StreamOptions)>,
    pub forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars,
    pub tool_allowlist: Option<Vec<String>>,
}

pub(super) struct StreamDeps<'a> {
    pub config: StreamConfig,
    pub injections: mush_agent::AgentInjections,
    pub tools: &'a ToolRegistry,
    pub registry: &'a ApiRegistry,
}

impl StreamState {
    pub(super) fn new() -> Self {
        Self {
            metas: HashMap::new(),
            generations: HashMap::new(),
            next_gen: 0,
            last_session_save: std::time::Instant::now() - SESSION_SAVE_DEBOUNCE,
            session_dirty: false,
            notify: Arc::new(default_notification_sink),
            #[cfg(feature = "signal")]
            signal: None,
            #[cfg(feature = "signal")]
            completion_sequence: 0,
        }
    }

    fn advance_generation(&mut self, pane_id: PaneId) -> StreamGeneration {
        self.cancel_signal();
        let stream_gen = StreamGeneration(self.next_gen);
        self.next_gen += 1;
        self.generations.insert(pane_id, stream_gen);
        stream_gen
    }

    pub(super) fn cancel_signal(&mut self) {
        #[cfg(feature = "signal")]
        if let Some(signal) = &mut self.signal {
            signal.cancel();
        }
    }

    fn completion_signal(&mut self) {
        #[cfg(feature = "signal")]
        if let Some(sequence) = self.completion_sequence.checked_add(1) {
            self.completion_sequence = sequence;
            if let Some(signal) = &mut self.signal {
                signal.completion(mush_notify::Completion(sequence));
            }
        }
    }

    pub(super) async fn poll_signal(&mut self) {
        #[cfg(feature = "signal")]
        if let Some(signal) = &mut self.signal
            && let Some(outcome) = signal.finished().await
        {
            tracing::debug!(?outcome, "signal notification decision");
        }
    }

    pub(super) fn register_active(
        &mut self,
        pane_id: PaneId,
        meta: StreamMeta,
    ) -> StreamGeneration {
        let stream_gen = self.advance_generation(pane_id);
        self.metas.insert(pane_id, meta);
        stream_gen
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
        if let Some(meta) = self.metas.get(&pane_id) {
            meta.cancel.cancel();
        }
        self.advance_generation(pane_id);
        self.metas.remove(&pane_id);
    }

    pub(super) fn is_current(&self, pane_id: PaneId, stream_gen: StreamGeneration) -> bool {
        self.generations.get(&pane_id) == Some(&stream_gen)
    }

    pub(super) fn save_session_debounced(
        &mut self,
        session: &Session,
        tui_config: &super::TuiConfig,
    ) {
        self.session_dirty = true;
        self.save_dirty_session(session, tui_config);
    }

    pub(super) fn save_dirty_session(&mut self, session: &Session, tui_config: &super::TuiConfig) {
        if !self.session_dirty || self.last_session_save.elapsed() < SESSION_SAVE_DEBOUNCE {
            return;
        }
        if let Some(ref saver) = tui_config.save_session {
            saver(build_session_snapshot(session, tui_config));
            self.last_session_save = std::time::Instant::now();
            self.session_dirty = false;
        }
    }

    pub(super) fn save_session_now(&mut self, session: &Session, tui_config: &super::TuiConfig) {
        if let Some(ref saver) = tui_config.save_session {
            saver(build_session_snapshot(session, tui_config));
            self.last_session_save = std::time::Instant::now();
            self.session_dirty = false;
        }
    }
}

pub(super) fn take_pending_prompts(
    session: &mut Session,
    pending_prompt: &mut Option<String>,
) -> Vec<(PaneId, PendingPrompt)> {
    let mut prompts = Vec::new();
    if let Some(prompt) = pending_prompt.take() {
        prompts.push((PaneId::new(1), PendingPrompt::text(prompt)));
    }
    for pane in std::slice::from_mut(session) {
        if let Some(prompt) = pane.runtime.pending_prompt.take()
            && !prompts.iter().any(|(id, _)| *id == PaneId::new(1))
        {
            prompts.push((PaneId::new(1), prompt));
        }
    }
    prompts
}

pub(super) async fn poll_confirmation_prompt(
    session: &mut Session,
    stream_state: &mut StreamState,
) -> bool {
    let focused_id = PaneId::new(1);
    if let Some(meta) = stream_state.meta_mut(focused_id)
        && !meta.cancel.is_cancelled()
        && meta.confirm_reply.lock().await.is_none()
        && let Ok(confirm) = meta.confirm_req_rx.try_recv()
        && !confirm.reply.is_closed()
    {
        let app = &mut session.app;
        app.interaction.mode = crate::app::AppMode::ToolConfirm;
        app.interaction.confirm_tool_call_id = Some(confirm.tool_call_id.clone());
        app.interaction.confirm_prompt = Some(confirm.prompt);
        *meta.confirm_reply.lock().await = Some(PendingConfirmation {
            tool_call_id: confirm.tool_call_id,
            reply: confirm.reply,
        });
        (stream_state.notify)(
            "tool confirmation required",
            "awaiting your approval",
            Some(crate::notify::Sound::Attention),
        );
        return true;
    }
    false
}

pub(super) async fn answer_confirmation(
    session: &mut Session,
    stream_state: &mut StreamState,
    allowed: bool,
) {
    let focused_id = PaneId::new(1);
    let pending_tool_call_id = if let Some(meta) = stream_state.meta_mut(focused_id)
        && let Some(pending) = meta.confirm_reply.lock().await.take()
    {
        let tool_call_id = pending.tool_call_id.clone();
        let _ = pending.reply.send(allowed);
        Some(tool_call_id)
    } else {
        None
    };

    let app = &mut session.app;
    app.interaction.mode = crate::app::AppMode::Insert;
    app.interaction.confirm_prompt = None;
    app.interaction.confirm_tool_call_id = None;
    if !allowed {
        app.status = Some(crate::app_state::Status::from(match pending_tool_call_id {
            Some(tool_call_id) => format!("tool denied: {tool_call_id}"),
            None => "tool denied".into(),
        }));
    }
}

pub(super) async fn submit_kernel_notices(
    session: &mut Session,
    stream_state: &StreamState,
    notices: &Option<crate::runner::config::KernelNotices>,
) -> bool {
    let Some(notices) = notices else {
        return false;
    };
    let pending: Vec<String> = match notices.lock() {
        Ok(mut guard) if !guard.is_empty() => guard.drain(..).collect(),
        _ => return false,
    };
    for text in pending {
        submit_streaming_input(session, stream_state, text, Vec::new()).await;
    }
    true
}

pub(super) fn poll_live_tool_output(
    session: &mut Session,
    live: &Option<crate::runner::config::LiveToolOutput>,
) -> bool {
    let Some(live) = live else {
        return false;
    };
    let updates: Vec<(String, String)> = match live.lock() {
        Ok(mut guard) if !guard.is_empty() => guard.drain().collect(),
        _ => return false,
    };
    let app = &mut session.app;
    let mut changed = false;
    for (exec_id, line) in updates {
        changed |= app.push_tool_output_by_exec_id(&exec_id, &line);
    }
    changed
}

async fn handle_agent_end(
    session: &mut Session,
    stream_state: &mut StreamState,
    pane_id: PaneId,
    tui_config: &TuiConfig,
    registry: &ApiRegistry,
    automatic_continuation: bool,
) {
    let stopped = stream_state
        .remove(pane_id)
        .is_some_and(|meta| !meta.cancel.is_cancelled());
    if stopped
        && !automatic_continuation
        && session.runtime.pending_prompt.is_none()
        && session.runtime.pending_compaction_inputs.is_empty()
        && !session.app.has_queued_messages()
    {
        (stream_state.notify)(
            "awaiting input",
            "task stopped and needs your input",
            Some(crate::notify::Sound::Complete),
        );
        stream_state.completion_signal();
    }

    let title_request = (!session.runtime.title_generated
        && tui_config.generate_title
        && session.app.session_title.is_none()
        && session.runtime.conversation.context_len() >= 2)
        .then(|| session.runtime.conversation.context_prefix(4));

    if let Some(msgs) = title_request {
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
            session.app.session_title = Some(title);
        }
        {
            let pane = &mut *session;
            pane.runtime.title_generated = true;
        }
    }

    if tui_config.auto_fork_compact {
        auto_compact(session, pane_id, tui_config, registry).await;
    }

    stream_state.save_session_now(session, tui_config);
}

pub(super) async fn handle_agent_event_side_effects(
    session: &mut Session,
    stream_state: &mut StreamState,
    pane_id: PaneId,
    event: &AgentEvent,
    tui_config: &TuiConfig,
    registry: &ApiRegistry,
    automatic_continuation: bool,
) {
    if let AgentEvent::MessageEnd { message } = event
        && let Some(meta) = stream_state.meta(pane_id)
    {
        meta.context_tokens.store(
            message.usage.total_input_tokens().get(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    if let AgentEvent::MessageEnd { message } = event
        && message.provider == Provider::OpenRouter
        && let Some(gen_id) = message.generation_id.clone()
    {
        let model = stream_state
            .meta(pane_id)
            .map(|m| m.model.clone())
            .unwrap_or_else(|| tui_config.model.clone());
        reconcile_openrouter_generation(session, pane_id, gen_id, &model, tui_config).await;
    }

    if matches!(
        event,
        AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolExecEnd { .. }
            | AgentEvent::ContextCompacted { .. }
    ) {
        stream_state.save_session_debounced(session, tui_config);
    }

    if matches!(event, AgentEvent::AgentEnd) {
        handle_agent_end(
            session,
            stream_state,
            pane_id,
            tui_config,
            registry,
            automatic_continuation,
        )
        .await;
    }
}

async fn reconcile_openrouter_generation(
    session: &mut Session,
    _pane_id: PaneId,
    gen_id: String,
    model: &Model,
    tui_config: &TuiConfig,
) {
    let (key, _) =
        crate::event_handler::resolve_auth_for_model(model, &tui_config.provider_api_keys).await;
    let Some(key) = key else {
        tracing::debug!("openrouter /generation lookup skipped: no api key");
        return;
    };

    let cost =
        match mush_ai::openrouter_generation::fetch_generation_cost(key.expose(), gen_id).await {
            Ok(c) => c,
            Err(err) => {
                tracing::debug!(error = %err, "openrouter /generation lookup failed");
                return;
            }
        };

    let Some(provider_name) = cost.provider_name else {
        return;
    };

    {
        let pane = &mut *session;
        pane.app.cache.apply_serving_provider(
            provider_name,
            model,
            &tui_config.options,
            tui_config.cache_timer,
        );
    }
}

async fn auto_compact(
    session: &mut Session,
    pane_id: PaneId,
    tui_config: &TuiConfig,
    registry: &ApiRegistry,
) {
    let pane = &*session;

    let context_tokens = pane.app.stats.context_tokens;
    let messages = pane.runtime.conversation.context();
    let target = compaction_target(session, pane_id, tui_config).await;

    if !mush_session::compact::needs_compaction_for_messages(
        context_tokens,
        target.window,
        &messages,
    ) {
        return;
    }

    let pane = &mut *session;

    let task = crate::slash::start_compaction(
        messages,
        target.model,
        target.options,
        registry.clone(),
        Some(tui_config.lifecycle_hooks.clone()),
        Some(tui_config.cwd.clone()),
        mush_session::compact::CompactionFocus::default(),
    );
    let Ok(result) = task.await else {
        pane.app.push_system_message("auto-compaction task failed");
        return;
    };

    let pane = session;
    match result {
        crate::slash::CompactionTaskResult::Applied(result) => {
            crate::slash::apply_applied_compaction(
                &mut pane.app,
                &mut pane.runtime.conversation,
                result,
            );
        }
        crate::slash::CompactionTaskResult::Failed {
            failure,
            tokens_before,
            model_id,
        } => {
            let applied = crate::slash::fallback_applied_compaction(
                failure,
                tokens_before,
                model_id,
                Some(&tui_config.lifecycle_hooks),
                Some(&tui_config.cwd),
            )
            .await;
            crate::slash::apply_applied_compaction(
                &mut pane.app,
                &mut pane.runtime.conversation,
                applied,
            );
        }
    }
}

pub(super) async fn abort_focused_stream(session: &mut Session, stream_state: &mut StreamState) {
    if let Some(control) = &session.runtime.control {
        control.finish(mush_agent::ipc::control::Outcome::Cancelled);
        session.runtime.pending_prompt = None;
    }
    let focused_id = PaneId::new(1);
    record_cancelled_tools(session);

    stream_state.abort(focused_id);

    let pane = &mut *session;
    pane.app.finish_streaming(None, None);
    let restored = pane.app.take_queued_messages();
    pane.runtime.steering_queue.lock().await.clear();
    if !restored.is_empty() {
        let text = restored.join("\n");
        pane.app.input.text = text.clone();
        pane.app.input.cursor = text.len();
    }
}

fn record_cancelled_tools(pane: &mut crate::session::Session) {
    let running_tools: Vec<_> = pane
        .app
        .active_tools
        .iter()
        .filter(|tool| tool.status == crate::display_types::ToolCallStatus::Running)
        .map(|tool| (tool.tool_call_id.clone(), tool.name.clone()))
        .collect();

    for (tool_call_id, tool_name) in running_tools {
        let result = mush_agent::tool::ToolResult::error(USER_CANCELLED_TOOL_MESSAGE);
        pane.app.end_tool(
            &tool_call_id,
            &tool_name,
            ToolOutcome::Error,
            Some(USER_CANCELLED_TOOL_MESSAGE),
            None,
        );
        pane.runtime
            .conversation
            .append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id,
                tool_name: tool_name.into(),
                content: result.content,
                outcome: result.outcome,
                timestamp_ms: Timestamp::now(),
            }));
    }

    pane.app.stream.active = false;
    pane.app.active_tools.clear();
    pane.app.status = Some("aborted".into());
}

pub(super) async fn submit_streaming_input(
    session: &mut Session,
    stream_state: &StreamState,
    text: String,
    images: Vec<crate::input_buffer::PendingImage>,
) {
    let focused_id = PaneId::new(1);
    if let Some(meta) = stream_state.meta(focused_id) {
        let content = build_user_content(&text, &images);
        meta.steering_queue.lock().await.push(Message::User(
            UserMessage::builder()
                .content(content)
                .timestamp_ms(Timestamp::now())
                .prompt_id(mush_ai::types::PromptId::new())
                .build(),
        ));
        let image_bytes: Vec<Vec<u8>> = images.into_iter().map(|img| img.data).collect();
        if image_bytes.is_empty() {
            session.app.push_queued_message(text);
        } else {
            session
                .app
                .push_queued_message_with_images(text, image_bytes);
        }
    } else {
        let pane = &mut *session;
        let image_bytes: Vec<Vec<u8>> = images.iter().map(|img| img.data.clone()).collect();
        if image_bytes.is_empty() {
            pane.app.push_user_message(text.clone());
        } else {
            pane.app
                .push_user_message_with_images(text.clone(), image_bytes);
        }
        pane.app.active_tools.clear();
        pane.app.start_streaming();
        pane.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images));
    }
}

pub(crate) fn build_user_content(
    text: &str,
    images: &[crate::input_buffer::PendingImage],
) -> UserContent {
    if images.is_empty() {
        return UserContent::Text(text.to_owned());
    }
    let mut parts: Vec<UserContentPart> = vec![UserContentPart::Text(TextContent {
        text: text.to_owned(),
    })];
    for image in images {
        use base64::Engine;
        parts.push(UserContentPart::Image(ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(&image.data),
            mime_type: image.mime_type,
        }));
    }
    UserContent::Parts(parts)
}

pub(super) async fn edit_last_queued_steering(session: &mut Session, stream_state: &StreamState) {
    let focused_id = PaneId::new(1);
    let app = &mut session.app;

    if let Some(queued_text) = app.pop_last_queued_message() {
        let current_input = app.input.take_text();
        if !current_input.trim().is_empty() {
            app.push_queued_message(&current_input);
            if let Some(meta) = stream_state.meta(focused_id) {
                meta.steering_queue.lock().await.push(Message::User(
                    UserMessage::builder()
                        .content(UserContent::Text(current_input))
                        .timestamp_ms(Timestamp::now())
                        .prompt_id(mush_ai::types::PromptId::new())
                        .build(),
                ));
            }
        }

        app.input.text.clone_from(&queued_text);
        app.input.cursor = app.input.text.len();
        app.input.ensure_cursor_visible();

        if let Some(meta) = stream_state.meta(focused_id) {
            let mut steering_queue = meta.steering_queue.lock().await;
            let target = queued_text.trim();
            let pos = steering_queue.iter().rposition(|message| {
                if let Message::User(user_message) = message {
                    user_message.content.text() == queued_text
                } else {
                    false
                }
            });
            let pos = pos.or_else(|| {
                steering_queue.iter().rposition(|message| {
                    if let Message::User(user_message) = message {
                        user_message.content.text().trim() == target
                    } else {
                        false
                    }
                })
            });
            if let Some(pos) = pos {
                steering_queue.remove(pos);
            } else {
                tracing::warn!(
                    queued_text = %queued_text,
                    "could not find matching steering queue entry to remove \
                     on lift; the next stream may see duplicate content"
                );
            }
        }
    } else {
        app.status = Some("no queued messages to edit".into());
    }
}

pub(super) async fn start_pending_streams<'a>(
    stream_state: &mut StreamState,
    session: &mut Session,
    pending_prompt: &mut Option<String>,
    deps: StreamDeps<'a>,
) {
    if session.runtime.turn_active() {
        return;
    }
    for (pane_id, prompt) in take_pending_prompts(session, pending_prompt) {
        start_stream_for_prompt(stream_state, session, pane_id, prompt, &deps).await;
    }
}

async fn start_stream_for_prompt<'a>(
    stream_state: &mut StreamState,
    session: &mut Session,
    pane_id: PaneId,
    prompt: PendingPrompt,
    deps: &StreamDeps<'a>,
) {
    let tools = deps.tools;
    let registry = deps.registry;
    let pane_ref = &*session;
    {};
    let steering_queue = pane_ref.runtime.steering_queue.clone();
    let model = pane_model(session, &deps.config.default_model);
    let thinking_level = pane_thinking_level(session);
    let conversation_snapshot = {
        let pane = &mut *session;
        pane.runtime.pending_prompt = Some(prompt);
        save_pane_pending_prompt(pane);
        let conversation = pane.runtime.conversation.context();
        pane.runtime.pending_prompt = None;
        conversation
    };

    let compaction_window = model.compaction_window();
    let (compact_model, compact_options) = deps
        .config
        .compaction_model
        .clone()
        .unwrap_or_else(|| (model.clone(), deps.config.options.clone()));
    let compact_options = crate::event_handler::resolve_auth_into_options(
        &compact_model,
        compact_options,
        &deps.config.provider_api_keys,
    )
    .await;
    let do_auto_compact = deps.config.auto_compact;
    let initial_ctx = session.app.stats.context_tokens.get();
    let initial_estimated_tokens = estimated_tokens_u64(&conversation_snapshot);
    let context_tokens_shared = Arc::new(std::sync::atomic::AtomicU64::new(initial_ctx));
    let transform = build_context_transform(
        compact_model,
        compact_options,
        do_auto_compact,
        compaction_window,
        registry.clone(),
        context_tokens_shared.clone(),
        initial_estimated_tokens,
        deps.injections.lifecycle_hooks.clone(),
        deps.injections.cwd.clone().unwrap_or_default(),
    );

    let steering = build_steering_callback(steering_queue.clone());
    let follow_up = build_steering_callback(steering_queue.clone());
    let (confirm_req_rx, confirm) = build_confirm_callback(deps.config.confirm_tools);

    let mut call_options = deps.config.options.clone();
    let (api_key, account_id) =
        event_handler::resolve_auth_for_model(&model, &deps.config.provider_api_keys).await;
    call_options.api_key = api_key;
    call_options.account_id = account_id;
    call_options.thinking = Some(thinking_level);

    let extra_tools: Vec<SharedTool> = Vec::new();
    let system_prompt = build_system_prompt(&deps.config.system_prompt);

    let mut tool_registry = tools.clone();
    tool_registry.extend_shared(extra_tools);

    let effective_cwd = deps
        .injections
        .cwd
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    mush_tools::align_for_model(
        &mut tool_registry,
        &model,
        std::sync::Arc::from(effective_cwd),
        deps.config.forbidden_chars.clone(),
    );
    if let Some(allowed) = &deps.config.tool_allowlist {
        tool_registry.retain_named(allowed);
    }

    let cancel = tokio_util::sync::CancellationToken::new();

    let stream_gen = stream_state.register_active(
        pane_id,
        StreamMeta {
            steering_queue,
            confirm_req_rx,
            confirm_reply: Arc::new(Mutex::new(None)),
            model: model.clone(),
            context_tokens: context_tokens_shared,
            cancel: cancel.clone(),
        },
    );

    let max_turns = deps.config.max_turns;
    let injections = deps.injections.clone();
    session
        .runtime
        .turns
        .spawn(registry.clone(), move |registry| {
            let config = AgentConfig {
                model,
                system_prompt,
                tools: tool_registry,
                registry,
                options: call_options,
                max_turns,
                hooks: Box::new(mush_agent::ClosureHooks {
                    steering,
                    follow_up,
                    transform,
                    confirm,
                }),
                injections,
                cancel: Some(cancel),
            };
            Box::pin(futures::StreamExt::map(
                agent_loop(config, conversation_snapshot),
                move |event| (pane_id, stream_gen, event),
            ))
        });
}

fn pane_model(session: &Session, default_model: &Model) -> Model {
    resolve_runtime_model(session.app.model_id.as_str(), default_model)
}

pub(super) struct CompactionTarget {
    pub model: Model,
    pub options: StreamOptions,
    pub window: mush_ai::types::TokenCount,
}

pub(super) async fn compaction_target(
    session: &Session,
    _pane_id: PaneId,
    tui_config: &TuiConfig,
) -> CompactionTarget {
    let pane_model = pane_model(session, &tui_config.model);
    let (model, options) = tui_config
        .compaction_options()
        .unwrap_or_else(|| (pane_model.clone(), tui_config.stream_options()));
    let options = crate::event_handler::resolve_auth_into_options(
        &model,
        options,
        &tui_config.provider_api_keys,
    )
    .await;
    CompactionTarget {
        model,
        options,
        window: pane_model.compaction_window(),
    }
}

pub(super) fn resolve_runtime_model(id: &str, default: &Model) -> Model {
    mush_ai::discovery::resolve_model_by_id(id).unwrap_or_else(|| default.clone())
}

fn pane_thinking_level(session: &Session) -> ThinkingLevel {
    session.app.thinking_level.clone()
}

#[allow(clippy::too_many_arguments)]
fn build_context_transform(
    compact_model: Model,
    compact_options: StreamOptions,
    do_auto_compact: bool,
    compaction_window: TokenCount,
    registry: ApiRegistry,
    context_tokens_shared: Arc<std::sync::atomic::AtomicU64>,
    initial_estimated_tokens: u64,
    lifecycle_hooks: mush_agent::LifecycleHooks,
    cwd: std::path::PathBuf,
) -> Option<mush_agent::TransformFn> {
    if !do_auto_compact {
        return None;
    }
    let ctx_tokens_for_transform = context_tokens_shared;
    let baseline_estimated_tokens =
        Arc::new(std::sync::atomic::AtomicU64::new(initial_estimated_tokens));
    let hooks = Arc::new(lifecycle_hooks);
    let cwd = Arc::new(cwd);
    let registry = Arc::new(registry);

    Some(Box::new(move |messages| {
        let owned_messages = messages.to_vec();
        let model = compact_model.clone();
        let options = compact_options.clone();
        let ctx_tokens = ctx_tokens_for_transform.clone();
        let baseline_estimated_tokens = baseline_estimated_tokens.clone();
        let hooks = hooks.clone();
        let cwd = cwd.clone();
        let registry = registry.clone();
        Box::pin(async move {
            let current_tokens =
                TokenCount::new(ctx_tokens.load(std::sync::atomic::Ordering::Relaxed));

            let current_estimated_tokens = TokenCount::new(estimated_tokens_u64(&owned_messages));
            let baseline_tokens = TokenCount::new(
                baseline_estimated_tokens.load(std::sync::atomic::Ordering::Relaxed),
            );
            let effective_compaction_tokens =
                mush_session::compact::compaction_tokens_for_estimated_payload(
                    current_tokens,
                    baseline_tokens,
                    current_estimated_tokens,
                );
            if !mush_session::compact::needs_compaction_at(
                effective_compaction_tokens,
                compaction_window,
                owned_messages.len(),
            ) {
                baseline_estimated_tokens.store(
                    current_estimated_tokens.get(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                return mush_agent::ContextTransformResult::Unchanged;
            }
            let result = event_handler::auto_compact(
                owned_messages,
                effective_compaction_tokens,
                compaction_window,
                &registry,
                &model,
                &options,
                Some(&hooks),
                Some(cwd.as_path()),
            )
            .await;
            let tokens_after = estimated_tokens_u64(&result.messages);
            baseline_estimated_tokens.store(tokens_after, std::sync::atomic::Ordering::Relaxed);
            if result.summarised_count > 0 {
                ctx_tokens.store(tokens_after, std::sync::atomic::Ordering::Relaxed);
                mush_agent::ContextTransformResult::Compacted(mush_agent::ContextCompaction {
                    messages: result.messages,
                    summary: result.summary,
                    summarised_count: result.summarised_count,
                })
            } else if result.masked_count > 0 {
                mush_agent::ContextTransformResult::Silent(result.messages)
            } else {
                mush_agent::ContextTransformResult::Unchanged
            }
        })
    }))
}

fn estimated_tokens_u64(messages: &[Message]) -> u64 {
    mush_session::compact::estimate_tokens(messages)
        .try_into()
        .unwrap_or(u64::MAX)
}

fn build_steering_callback(
    steering_queue: Arc<Mutex<Vec<Message>>>,
) -> Option<mush_agent::SteeringFn> {
    Some(Box::new(move || {
        let steering_queue = steering_queue.clone();
        Box::pin(async move {
            let mut queue = steering_queue.lock().await;
            queue.drain(..).collect()
        })
    }))
}

fn build_confirm_callback(
    confirm_tools: bool,
) -> (
    tokio::sync::mpsc::Receiver<ConfirmRequest>,
    Option<mush_agent::ConfirmFn>,
) {
    let (confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel::<ConfirmRequest>(1);
    let confirm: Option<mush_agent::ConfirmFn> = if confirm_tools {
        Some(Box::new(
            move |tool_call_id: &ToolCallId,
                  name: &str,
                  args: &serde_json::Value|
                  -> std::pin::Pin<
                Box<dyn std::future::Future<Output = mush_agent::ConfirmAction> + Send>,
            > {
                let tx = confirm_req_tx.clone();
                let summary = mush_agent::summarise_tool_args(name, args);
                let prompt = format!("{name} {summary}");
                let tool_call_id = tool_call_id.clone();
                let name = name.to_string();
                let args = args.clone();
                Box::pin(async move {
                    let _ = (&name, &args);
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
        )
            as Box<
                dyn Fn(
                        &mush_ai::types::ToolCallId,
                        &str,
                        &serde_json::Value,
                    )
                        -> mush_agent::BoxFuture<'static, mush_agent::ConfirmAction>
                    + Send
                    + Sync,
            >)
    } else {
        None
    };

    (confirm_req_rx, confirm)
}

fn build_system_prompt(base_system_prompt: &Option<String>) -> Option<String> {
    base_system_prompt.clone()
}

pub(crate) fn build_session_snapshot(
    session: &Session,
    tui_config: &super::TuiConfig,
) -> super::SessionSnapshot {
    super::SessionSnapshot {
        session_id: tui_config.session_id.clone(),
        title: session.app.session_title.clone(),
        primary: session.runtime.conversation.clone(),
        model_id: session.app.model_id.to_string(),
        panes: Vec::new(),
        system_prompt: tui_config.system_prompt.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mush_ai::models;
    use mush_ai::types::{ModelId, ToolCallId};

    #[tokio::test]
    async fn compaction_target_follows_the_pane_model_not_the_startup_one() {
        let catalogue = models::all_models_with_user();
        let startup = catalogue.first().expect("catalogue is never empty").clone();
        let switched = catalogue
            .iter()
            .find(|m| m.id != startup.id && m.compaction_window() != startup.compaction_window())
            .expect("catalogue should hold two models with different windows")
            .clone();

        let mut config = crate::runner::TuiConfig::for_test();
        config.model = startup.clone();

        let pane_id = PaneId::new(1);
        let mut app = crate::app::App::new(switched.id.clone(), switched.context_window);
        app.model_id = switched.id.clone();
        let session = crate::session::Session::new(app);

        let target = compaction_target(&session, pane_id, &config).await;

        assert_eq!(
            target.model.id, switched.id,
            "the pane's own model should write the summary"
        );
        assert_eq!(
            target.window,
            switched.compaction_window(),
            "the pane's own window is the budget to compact against"
        );
    }

    #[tokio::test]
    async fn compaction_target_prefers_the_configured_summariser_model() {
        let catalogue = models::all_models_with_user();
        let pane_model = catalogue.first().expect("catalogue is never empty").clone();
        let summariser = catalogue
            .iter()
            .find(|m| m.id != pane_model.id)
            .expect("catalogue should hold two models")
            .clone();

        let mut config = crate::runner::TuiConfig::for_test();
        config.model = pane_model.clone();
        config.compaction_model = Some((summariser.clone(), StreamOptions::default()));

        let pane_id = PaneId::new(1);
        let app = crate::app::App::new(pane_model.id.clone(), pane_model.context_window);
        let session = crate::session::Session::new(app);

        let target = compaction_target(&session, pane_id, &config).await;

        assert_eq!(target.model.id, summariser.id, "/compaction-model wins");
        assert_eq!(
            target.window,
            pane_model.compaction_window(),
            "the window still comes from the pane's model, not the summariser"
        );
    }

    #[test]
    fn stream_config_groups_owned_fields() {
        let config = StreamConfig {
            default_model: models::all_models_with_user().into_iter().next().unwrap(),
            system_prompt: None,
            options: StreamOptions::default(),
            max_turns: 32,
            provider_api_keys: HashMap::new(),
            confirm_tools: false,
            auto_compact: false,
            compaction_model: None,
            forbidden_chars: Default::default(),
            tool_allowlist: None,
        };
        assert_eq!(config.max_turns, 32);
        assert!(!config.confirm_tools);
    }

    #[test]
    fn stream_deps_uses_agent_injections() {
        let injections = mush_agent::AgentInjections {
            cwd: Some(std::path::PathBuf::from("/tmp")),
            ..Default::default()
        };
        assert_eq!(
            injections.cwd.as_deref(),
            Some(std::path::Path::new("/tmp"))
        );
    }

    use crate::app::App;
    use crate::session::Session;

    fn app() -> App {
        App::new(ModelId::from("test-model"), TokenCount::new(4096))
    }

    fn user_msg(text: impl Into<String>) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    struct RecordingProvider {
        requests: Arc<std::sync::Mutex<Vec<Vec<Message>>>>,
    }

    #[async_trait::async_trait]
    impl mush_ai::registry::ApiProvider for RecordingProvider {
        fn api(&self) -> mush_ai::types::Api {
            mush_ai::types::Api::AnthropicMessages
        }

        async fn stream(
            &self,
            model: &Model,
            context: &mush_ai::registry::LlmContext,
            _options: &StreamOptions,
        ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
            self.requests.lock().unwrap().push(context.messages.clone());
            let message = mush_ai::types::AssistantMessage::builder()
                .content(vec![mush_ai::types::AssistantContentPart::Text(
                    mush_ai::types::TextContent {
                        text: "waiting for val push".into(),
                    },
                )])
                .model(model.id.clone())
                .provider(model.provider.clone())
                .api(model.api)
                .stop_reason(mush_ai::types::StopReason::Stop)
                .timestamp_ms(Timestamp::zero())
                .build();
            Ok(Box::pin(futures::stream::iter([
                mush_ai::stream::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "waiting for val push".into(),
                },
                mush_ai::stream::StreamEvent::Done {
                    reason: mush_ai::types::StopReason::Stop,
                    message,
                },
            ])))
        }
    }

    #[tokio::test]
    async fn automatic_compaction_survives_next_prompt_and_resume() {
        use futures::StreamExt;
        use mush_session::ConversationState;

        let model = models::all_models()
            .into_iter()
            .find(|model| model.api == mush_ai::types::Api::AnthropicMessages)
            .unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(RecordingProvider {
            requests: requests.clone(),
        }));
        let messages: Vec<_> = (0..30)
            .map(|i| user_msg(format!("turn {i}: {}", "x".repeat(2_000))))
            .collect();
        let mut conversation = ConversationState::from_messages(messages);
        let mut app = app();
        let mut image_protos = HashMap::new();
        let saved = Arc::new(std::sync::Mutex::new(None));
        let save_target = saved.clone();
        let mut tui_config = test_tui_config();
        tui_config.save_session = Some(Arc::new(move |snapshot| {
            *save_target.lock().unwrap() = Some(snapshot);
        }));
        let mut stream_state = StreamState::new();

        for turn in 0..2 {
            let messages = conversation.context();
            let transform = build_context_transform(
                model.clone(),
                StreamOptions::default(),
                true,
                TokenCount::new(10_000),
                registry.clone(),
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                estimated_tokens_u64(&messages),
                mush_agent::LifecycleHooks::default(),
                std::path::PathBuf::from("/tmp"),
            );
            let config = AgentConfig {
                model: model.clone(),
                system_prompt: None,
                tools: mush_agent::ToolRegistry::new(),
                registry: &registry,
                options: StreamOptions::default(),
                max_turns: 2,
                hooks: Box::new(mush_agent::ClosureHooks {
                    transform,
                    ..Default::default()
                }),
                injections: Default::default(),
                cancel: None,
            };
            let mut stream = agent_loop(config, messages);
            while let Some(event) = stream.next().await {
                event_handler::handle_agent_event(
                    &mut event_handler::EventCtx {
                        app: &mut app,
                        conversation: &mut conversation,
                        image_protos: &mut image_protos,
                    },
                    &event,
                    &model,
                    false,
                    &None,
                );
                if matches!(event, AgentEvent::ContextCompacted { .. }) {
                    let mut session =
                        Session::new(App::new(model.id.clone(), model.context_window));
                    session.runtime.conversation = conversation.clone();
                    handle_agent_event_side_effects(
                        &mut session,
                        &mut stream_state,
                        PaneId::new(1),
                        &event,
                        &tui_config,
                        &registry,
                        false,
                    )
                    .await;
                    assert_eq!(
                        saved
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .primary
                            .context_len(),
                        11
                    );
                }
            }

            assert_eq!(conversation.context_len(), 12 + turn * 2);
            let saved = serde_json::to_string(&conversation).unwrap();
            conversation = serde_json::from_str(&saved).unwrap();
            assert_eq!(conversation.context_len(), 12 + turn * 2);
            assert!(conversation.display_history().iter().any(
                |message| matches!(message, Message::User(user) if user.text().starts_with("turn 0:"))
            ));
            if turn == 0 {
                conversation.append_message(user_msg("so im allowed to push?"));
            }
        }

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].len(), 11);
        assert_eq!(requests[2].len(), 13);
        assert!(matches!(
            &requests[2][0],
            Message::User(user) if user.text().contains("<summary>\nwaiting for val push\n</summary>")
        ));
    }

    #[tokio::test]
    async fn context_transform_compacts_after_unsent_payload_growth() {
        let model = models::all_models_with_user().into_iter().next().unwrap();
        let context_tokens = Arc::new(std::sync::atomic::AtomicU64::new(6_000));
        let transform = build_context_transform(
            model,
            StreamOptions::default(),
            true,
            TokenCount::new(10_000),
            ApiRegistry::new(),
            context_tokens,
            0,
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
        )
        .expect("auto-compaction enabled should build a transform");

        let baseline: Vec<Message> = (0..20).map(|i| user_msg(format!("small {i}"))).collect();
        assert!(matches!(
            transform(&baseline).await,
            mush_agent::ContextTransformResult::Unchanged
        ));

        let mut grown = baseline;
        grown.push(user_msg("x".repeat(16_000)));

        match transform(&grown).await {
            mush_agent::ContextTransformResult::Compacted(compaction) => {
                assert!(
                    compaction.messages.len() < grown.len(),
                    "unsent payload growth should force compaction before the next provider call"
                );
            }
            other => panic!("expected compaction update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_transform_does_not_double_count_initial_payload_estimate() {
        let model = models::all_models_with_user().into_iter().next().unwrap();
        let context_tokens = Arc::new(std::sync::atomic::AtomicU64::new(9_000));
        let messages: Vec<Message> = (0..20)
            .map(|i| user_msg(format!("small {i}: {}", "x".repeat(120))))
            .collect();
        let transform = build_context_transform(
            model,
            StreamOptions::default(),
            true,
            TokenCount::new(10_000),
            ApiRegistry::new(),
            context_tokens,
            estimated_tokens_u64(&messages),
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
        )
        .expect("auto-compaction enabled should build a transform");

        match transform(&messages).await {
            mush_agent::ContextTransformResult::Unchanged => {}
            other => {
                panic!("expected unchanged context without new estimated growth, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn a_kernel_notice_becomes_a_prompt_when_idle_and_steering_when_busy() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();
        let notices: crate::runner::config::KernelNotices = Arc::new(std::sync::Mutex::new(vec![
            "Background command finished: `exit 5`".to_string(),
        ]));

        assert!(submit_kernel_notices(&mut session, &stream_state, &Some(notices.clone())).await);
        assert_eq!(
            session
                .runtime
                .pending_prompt
                .as_ref()
                .map(|p| p.text.as_str()),
            Some("Background command finished: `exit 5`")
        );
        assert!(!submit_kernel_notices(&mut session, &stream_state, &Some(notices.clone())).await);

        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));
        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: steering_queue.clone(),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );
        notices.lock().unwrap().push("second".into());

        assert!(submit_kernel_notices(&mut session, &stream_state, &Some(notices)).await);
        assert_eq!(steering_queue.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn submit_streaming_input_includes_pending_images_in_steering_queue() {
        use crate::input_buffer::PendingImage;
        use mush_ai::types::ImageMimeType;

        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();
        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));

        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: steering_queue.clone(),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        submit_streaming_input(
            &mut session,
            &stream_state,
            "look at this".into(),
            vec![PendingImage {
                data: vec![1, 2, 3, 4],
                mime_type: ImageMimeType::Png,
                dimensions: Some((10, 10)),
            }],
        )
        .await;

        let queue = steering_queue.lock().await;
        assert_eq!(queue.len(), 1, "steering queue should have 1 entry");
        let Message::User(user_msg) = &queue[0] else {
            panic!("expected user message in steering queue");
        };
        match &user_msg.content {
            UserContent::Parts(parts) => {
                let has_image = parts.iter().any(|p| matches!(p, UserContentPart::Image(_)));
                assert!(
                    has_image,
                    "steering user message should carry the pending image as a content part"
                );
            }
            UserContent::Text(_) => {
                panic!("steering user message dropped the pending image and used Text-only content")
            }
        }

        assert!(
            session.app.input.images.is_empty(),
            "pending images should be drained after submission"
        );
    }

    #[tokio::test]
    async fn submit_streaming_input_uses_text_only_when_no_images() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();
        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));

        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: steering_queue.clone(),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        submit_streaming_input(&mut session, &stream_state, "just words".into(), Vec::new()).await;

        let queue = steering_queue.lock().await;
        let Message::User(user_msg) = &queue[0] else {
            panic!("expected user message in steering queue");
        };
        assert!(
            matches!(&user_msg.content, UserContent::Text(t) if t == "just words"),
            "text-only submit should keep UserContent::Text"
        );
    }

    #[tokio::test]
    async fn edit_last_queued_steering_replaces_queue_entry_not_duplicates() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();
        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));

        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: steering_queue.clone(),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        submit_streaming_input(
            &mut session,
            &stream_state,
            "original message ".into(),
            Vec::new(),
        )
        .await;
        assert_eq!(
            steering_queue.lock().await.len(),
            1,
            "first submit puts one entry in queue"
        );

        edit_last_queued_steering(&mut session, &stream_state).await;
        let queue_len_after_lift = steering_queue.lock().await.len();
        assert_eq!(
            queue_len_after_lift, 0,
            "lift should remove the original entry from the queue \
             (had {queue_len_after_lift} left over)"
        );
        assert_eq!(
            session.app.input.text, "original message ",
            "lifted text should land in the input buffer"
        );

        session.app.input.text = "edited message".into();
        session.app.input.cursor = "edited message".len();
        let edited = session.app.input.take_text();
        submit_streaming_input(&mut session, &stream_state, edited, Vec::new()).await;

        let queue = steering_queue.lock().await;
        assert_eq!(
            queue.len(),
            1,
            "after lift+resubmit the queue must hold exactly the edited text, \
             not the original too (got {} entries)",
            queue.len()
        );
        let Message::User(user_msg) = &queue[0] else {
            panic!("expected user message in steering queue");
        };
        assert!(
            matches!(&user_msg.content, UserContent::Text(t) if t == "edited message"),
            "queue should contain the edited text, got {:?}",
            user_msg.content
        );
    }

    #[test]
    fn take_pending_prompts_prefers_the_typed_prompt() {
        let mut session = Session::new(app());
        let mut pending_prompt = Some("focused".to_string());
        session.runtime.pending_prompt = Some(PendingPrompt::text("duplicate"));

        let prompts = take_pending_prompts(&mut session, &mut pending_prompt);

        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].1.text, "focused");
        assert!(pending_prompt.is_none());
        assert!(session.runtime.pending_prompt.is_none());
    }

    #[tokio::test]
    async fn register_active_supersedes_previous_generation() {
        let mut stream_state = StreamState::new();
        let pane_id = PaneId::new(7);

        let (_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let gen1 = stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        assert!(stream_state.is_current(pane_id, gen1));
        assert!(stream_state.meta(pane_id).is_some());
        let _ = ToolCallId::from("unused");
    }

    #[tokio::test]
    async fn poll_confirmation_prompt_tracks_tool_call_id() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();
        let alerts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = alerts.clone();
        stream_state.notify = Arc::new(move |summary, body, sound| {
            captured
                .lock()
                .unwrap()
                .push((summary.to_owned(), body.to_owned(), sound));
        });
        let (confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);

        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
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

        assert!(poll_confirmation_prompt(&mut session, &mut stream_state).await);
        assert!(!poll_confirmation_prompt(&mut session, &mut stream_state).await);
        assert_eq!(
            *alerts.lock().unwrap(),
            vec![(
                "tool confirmation required".into(),
                "awaiting your approval".into(),
                Some(crate::notify::Sound::Attention)
            )]
        );

        let app = &session.app;
        assert_eq!(app.interaction.mode, crate::app::AppMode::ToolConfirm);
        assert_eq!(
            app.interaction.confirm_prompt.as_deref(),
            Some("read todo.md")
        );
        assert_eq!(
            app.interaction.confirm_tool_call_id.as_ref(),
            Some(&tool_call_id)
        );

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
        let mut session = Session::new(app());
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
                cancel: tokio_util::sync::CancellationToken::new(),
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

        assert!(poll_confirmation_prompt(&mut session, &mut stream_state).await);
        answer_confirmation(&mut session, &mut stream_state, true).await;

        assert!(reply_rx.await.unwrap());
        let app = &session.app;
        assert_eq!(app.interaction.mode, crate::app::AppMode::Insert);
        assert!(app.interaction.confirm_prompt.is_none());
        assert!(app.interaction.confirm_tool_call_id.is_none());
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

    #[tokio::test]
    async fn answer_confirmation_deny_sends_false_and_sets_status() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
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
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        let tool_call_id = ToolCallId::from("tc_esc");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        confirm_req_tx
            .send(ConfirmRequest {
                tool_call_id: tool_call_id.clone(),
                prompt: "delete everything".into(),
                reply: reply_tx,
            })
            .await
            .unwrap();

        assert!(poll_confirmation_prompt(&mut session, &mut stream_state).await);
        answer_confirmation(&mut session, &mut stream_state, false).await;

        assert!(!reply_rx.await.unwrap());
        let app = &session.app;
        assert_eq!(app.interaction.mode, crate::app::AppMode::Insert);
        assert!(app.interaction.confirm_prompt.is_none());
        assert!(app.interaction.confirm_tool_call_id.is_none());
        assert_eq!(
            app.status.as_ref().map(|s| s.text.as_str()),
            Some(format!("tool denied: {tool_call_id}").as_str())
        );
    }

    fn live_map(entries: &[(&str, &str)]) -> Option<crate::runner::config::LiveToolOutput> {
        let map = entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Some(std::sync::Arc::new(std::sync::Mutex::new(map)))
    }

    #[test]
    fn poll_live_tool_output_updates_focused_tool() {
        let _pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let tool_call_id = ToolCallId::from("tc_live");
        session.app.start_tool(&tool_call_id, "bash", "cargo test");

        let live = live_map(&[("tc_live", "running")]);
        assert!(poll_live_tool_output(&mut session, &live));

        assert_eq!(
            session.app.active_tools[0].live_output.as_deref(),
            Some("running")
        );
    }

    #[test]
    fn poll_live_tool_output_routes_each_exec_id_to_its_own_panel() {
        let mut session = Session::new(app());
        let a = ToolCallId::from("tc_a");
        let b = ToolCallId::from("tc_b");
        session.app.start_tool(&a, "bash", "jj st");
        session.app.start_tool(&b, "bash", "cargo test");

        let live = live_map(&[("tc_a", "jj output"), ("tc_b", "test output")]);
        assert!(poll_live_tool_output(&mut session, &live));

        let tools = &session.app.active_tools;
        assert_eq!(tools[0].live_output.as_deref(), Some("jj output"));
        assert_eq!(tools[1].live_output.as_deref(), Some("test output"));
    }

    #[test]
    fn poll_live_tool_output_reports_unchanged_output() {
        let _pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let tool_call_id = ToolCallId::from("tc_live");
        session.app.start_tool(&tool_call_id, "bash", "cargo test");
        session.app.push_tool_output(&tool_call_id, "running");

        let live = live_map(&[("tc_live", "running")]);

        assert!(!poll_live_tool_output(&mut session, &live));
    }

    fn test_tui_config() -> super::super::TuiConfig {
        let model = models::all_models_with_user().into_iter().next().unwrap();
        super::super::TuiConfig {
            model,
            system_prompt: None,
            options: mush_ai::types::StreamOptions::default(),
            max_turns: 32,
            initial_conversation: Default::default(),
            initial_title: None,
            initial_panes: vec![],
            theme: crate::theme::Theme::default(),
            config_path: None,
            provider_api_keys: std::collections::HashMap::new(),
            thinking_prefs: super::super::ThinkingPrefs::default(),
            save_thinking_prefs: None,
            last_models: super::super::LastModels::default(),
            save_last_models: None,
            save_session: None,
            generate_title: false,
            confirm_tools: false,
            auto_compact: false,
            auto_fork_compact: false,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar_config: Default::default(),
            debug_cache: false,
            cache_timer: false,
            thinking_display: crate::app::ThinkingDisplay::Collapse,
            tool_output_live: None,
            log_buffer: None,
            terminal_policy: crate::terminal_policy::TerminalPolicy::default(),
            lifecycle_hooks: mush_agent::LifecycleHooks::default(),
            cwd: std::path::PathBuf::from("/tmp"),
            dynamic_system_context: None,
            file_rules: None,
            lsp_diagnostics: None,
            agent_card: None,
            control: false,
            model_tiers: std::collections::HashMap::new(),
            compaction_model: None,
            resolve_model: None,
            http_client: None,
            session_id: mush_ai::types::SessionId::new(),
            settings: Default::default(),
            favourite_models: Vec::new(),
            favourites_locked: false,
            save_favourite_models: None,
            recent_models: Vec::new(),
            save_recent_models: None,
            keymap: crate::keybinds::KeyMap::default(),
            reload_context: None,
            mcp: None,
            skills: Default::default(),
            tool_allowlist: None,
            kernel_notices: None,
            kernel_state: None,
            forbidden_chars: Default::default(),
        }
    }

    #[test]
    fn save_dirty_session_flushes_after_debounce() {
        let mut session = Session::new(app());
        session.runtime.conversation.append_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("persist me".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));
        let mut config = test_tui_config();
        let saves = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let saves_for_callback = saves.clone();
        config.save_session = Some(Arc::new(move |_| {
            saves_for_callback.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        let mut stream_state = StreamState::new();
        stream_state.save_session_debounced(&session, &config);
        stream_state.save_session_debounced(&session, &config);
        stream_state.last_session_save = std::time::Instant::now() - SESSION_SAVE_DEBOUNCE;
        stream_state.save_dirty_session(&session, &config);

        assert_eq!(saves.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn build_user_content_persists_pending_prompt_text() {
        let content = build_user_content("hello from pending", &[]);

        assert_eq!(content.text(), "hello from pending");
    }

    #[test]
    fn stream_options_uses_canonical_session_id_not_stale_options_field() {
        let canonical = mush_ai::types::SessionId::from("loaded-session");
        let stale = mush_ai::types::SessionId::from("fresh-process");

        let mut config = test_tui_config();
        config.session_id = canonical.clone();
        config.options.session_id = Some(stale);

        let options = config.stream_options();

        assert_eq!(
            options.session_id.as_ref(),
            Some(&canonical),
            "stream_options must override session_id with the canonical top-level value"
        );
    }

    #[tokio::test]
    async fn generation_rejects_stale_stream_events() {
        let mut stream_state = StreamState::new();
        let pane_id = PaneId::new(1);

        let (_tx1, rx1) = tokio::sync::mpsc::channel(1);
        let gen1 = stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx: rx1,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        stream_state.abort(pane_id);

        let (_tx2, rx2) = tokio::sync::mpsc::channel(1);
        let gen2 = stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx: rx2,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        assert_ne!(gen1, gen2, "generations must differ");

        assert!(
            !stream_state.is_current(pane_id, gen1),
            "stale generation must be rejected"
        );

        assert!(
            stream_state.is_current(pane_id, gen2),
            "current generation must be accepted"
        );
    }

    #[tokio::test]
    async fn abort_without_reregister_rejects_all() {
        let mut stream_state = StreamState::new();
        let pane_id = PaneId::new(1);

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let stream_gen = stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: Arc::new(Mutex::new(Vec::new())),
                confirm_req_rx: rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        stream_state.abort(pane_id);

        assert!(!stream_state.is_current(pane_id, stream_gen));
    }

    #[tokio::test]
    async fn abort_focused_stream_restores_queued_messages_without_duplicates() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: session.runtime.steering_queue.clone(),
                confirm_req_rx: rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );
        session.app.start_streaming();

        submit_streaming_input(
            &mut session,
            &stream_state,
            "first steer".into(),
            Vec::new(),
        )
        .await;
        submit_streaming_input(
            &mut session,
            &stream_state,
            "second steer".into(),
            Vec::new(),
        )
        .await;

        assert_eq!(session.runtime.steering_queue.lock().await.len(), 2);
        assert_eq!(session.app.messages.iter().filter(|m| m.queued).count(), 2);

        abort_focused_stream(&mut session, &mut stream_state).await;

        let restored_input = &session.app.input.text;
        assert_eq!(
            restored_input, "first steer\nsecond steer",
            "abort must restore each queued message exactly once, got: {restored_input:?}"
        );
        assert!(session.runtime.steering_queue.lock().await.is_empty());
        assert!(!session.app.messages.iter().any(|m| m.queued));
    }

    #[tokio::test]
    async fn abort_focused_stream_records_running_tools_as_cancelled_results() {
        use mush_ai::types::{
            Api, AssistantContentPart, AssistantMessage, Message, Provider, TextContent, ToolCall,
            ToolCallId, ToolName, ToolOutcome, ToolResultContentPart,
        };

        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        stream_state.register_active(
            pane_id,
            StreamMeta {
                steering_queue: session.runtime.steering_queue.clone(),
                confirm_req_rx: rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        let tool_call_id = ToolCallId::from("tc_cancelled");
        session
            .runtime
            .conversation
            .append_message(Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![
                        AssistantContentPart::Text(TextContent {
                            text: "i will read that".into(),
                        }),
                        AssistantContentPart::ToolCall(ToolCall {
                            id: tool_call_id.clone(),
                            name: ToolName::from("read"),
                            arguments: serde_json::json!({"path": "src/lib.rs"}),
                        }),
                    ])
                    .model(ModelId::from("test-model"))
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ));
        session.app.start_streaming();
        session.app.finish_streaming(None, None);
        session.app.start_tool(&tool_call_id, "read", "src/lib.rs");

        abort_focused_stream(&mut session, &mut stream_state).await;

        let ctx = session.runtime.conversation.context();
        let Some(Message::ToolResult(result)) = ctx.last() else {
            panic!("abort should append a tool result");
        };

        assert_eq!(result.tool_call_id, tool_call_id);
        assert_eq!(result.tool_name.as_str(), "read");
        assert_eq!(result.outcome, ToolOutcome::Error);
        assert!(
            result
                .content
                .iter()
                .any(|part| matches!(part, ToolResultContentPart::Text(text) if text.text == "user cancelled tool")),
            "cancelled tool result should explain that the user cancelled it"
        );
    }

    #[test]
    fn build_session_snapshot_carries_system_prompt() {
        let session = Session::new(app());
        let mut config = test_tui_config();
        config.system_prompt = Some("you are running inside mush\nproject context: foo".into());

        let snapshot = build_session_snapshot(&session, &config);

        assert_eq!(
            snapshot.system_prompt.as_deref(),
            Some("you are running inside mush\nproject context: foo")
        );
    }

    #[test]
    fn save_session_debounced_respects_interval() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let save_count = std::sync::Arc::new(AtomicU32::new(0));
        let count_clone = save_count.clone();
        let mut config = test_tui_config();
        config.save_session = Some(std::sync::Arc::new(move |_| {
            count_clone.fetch_add(1, Ordering::Relaxed);
        }));

        let session = Session::new(app());
        let mut stream_state = StreamState::new();

        stream_state.save_session_debounced(&session, &config);
        assert_eq!(save_count.load(Ordering::Relaxed), 1);

        stream_state.save_session_debounced(&session, &config);
        assert_eq!(save_count.load(Ordering::Relaxed), 1);

        stream_state.save_session_now(&session, &config);
        assert_eq!(save_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn pane_model_resolves_openrouter_discovered_id() {
        use mush_ai::discovery::{DiscoveryCache, DiscoveryReport, cache_path};
        use mush_ai::types::{Api, BaseUrl, InputModality, ModelCost, Provider};
        use std::time::SystemTime;

        let tmp = tempfile::tempdir().unwrap();
        let saved = std::env::var("MUSH_DATA_DIR").ok();
        unsafe { std::env::set_var("MUSH_DATA_DIR", tmp.path()) };

        let glm_id = "z-ai/glm-5.1";
        let glm_model = Model {
            id: ModelId::from(glm_id),
            name: "GLM 5.1".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::OpenRouter,
            base_url: BaseUrl::from("https://openrouter.ai/api/v1"),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(128_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(8_192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        };
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::OpenRouter,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![glm_model.clone().into()],
        });
        cache.save(&cache_path()).unwrap();

        let mut session = Session::new(app());
        session.app.model_id = ModelId::from(glm_id);

        let default = Model {
            id: ModelId::from("claude-opus-4-7"),
            name: "Claude (default fallback)".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: BaseUrl::from("https://api.anthropic.com"),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(8_192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        };

        let resolved = pane_model(&session, &default);

        unsafe {
            match saved {
                Some(v) => std::env::set_var("MUSH_DATA_DIR", v),
                None => std::env::remove_var("MUSH_DATA_DIR"),
            }
        }

        assert_eq!(resolved.id.as_str(), glm_id);
        assert_eq!(resolved.provider, Provider::OpenRouter);
    }
}
