use std::sync::Arc;

use mush_agent::AgentEvent;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    Message, Model, Provider, StreamOptions, Timestamp, TokenCount, ToolOutcome, ToolResultMessage,
};

use crate::session::{AgentConnection, PendingPrompt, Session};
use mush_ai::types::PaneId;

const USER_CANCELLED_TOOL_MESSAGE: &str = "user cancelled tool";

use super::TuiConfig;

#[cfg(test)]
use mush_runtime::{ConfirmRequest, StreamMeta};

#[cfg(not(test))]
fn default_notification_sink(summary: &str, body: &str, sound: Option<crate::notify::Sound>) {
    crate::notify::send_with_sound(summary, body, sound);
}

#[cfg(test)]
fn default_notification_sink(_: &str, _: &str, _: Option<crate::notify::Sound>) {}

pub(super) type NotificationSink =
    Arc<dyn Fn(&str, &str, Option<crate::notify::Sound>) + Send + Sync>;

pub(super) struct StreamState {
    pub(super) notify: NotificationSink,
    #[cfg(feature = "signal")]
    pub(super) signal: Option<mush_notify::Dispatcher>,
    #[cfg(feature = "signal")]
    completion_sequence: u64,
}

impl StreamState {
    pub(super) fn new() -> Self {
        Self {
            notify: Arc::new(default_notification_sink),
            #[cfg(feature = "signal")]
            signal: None,
            #[cfg(feature = "signal")]
            completion_sequence: 0,
        }
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

    pub(super) fn save_dirty_session(
        &mut self,
        session: &mut Session,
        _tui_config: &super::TuiConfig,
    ) {
        session.connection.runtime.save_if_dirty();
    }

    pub(super) fn save_session_now(
        &mut self,
        session: &mut Session,
        _tui_config: &super::TuiConfig,
    ) {
        session.connection.runtime.save_now_and_settle();
    }
}

pub(super) fn show_confirm_request(
    session: &mut Session,
    stream_state: &StreamState,
    tool_call_id: mush_ai::types::ToolCallId,
    prompt: String,
) {
    let app = &mut session.app;
    app.interaction.mode = crate::app::AppMode::ToolConfirm;
    app.interaction.confirm_tool_call_id = Some(tool_call_id);
    app.interaction.confirm_prompt = Some(prompt);
    (stream_state.notify)(
        "tool confirmation required",
        "awaiting your approval",
        Some(crate::notify::Sound::Attention),
    );
}

pub(super) fn poll_kernel_busy_prompt(
    session: &mut Session,
    stream_state: &StreamState,
    prompts: &Option<crate::runner::config::KernelPrompts>,
) -> bool {
    let app = &mut session.app;
    if let Some(reply) = &app.interaction.kernel_busy_reply
        && reply.is_closed()
    {
        app.interaction.kernel_busy_reply = None;
        if app.interaction.mode == crate::app::AppMode::ToolConfirm {
            app.interaction.mode = crate::app::AppMode::Insert;
        }
        return true;
    }
    if app.interaction.mode != crate::app::AppMode::Insert {
        return false;
    }
    let Some(prompts) = prompts else {
        return false;
    };
    let Some(request) = prompts
        .lock()
        .ok()
        .and_then(|mut guard| guard.drain(..).rfind(|request| !request.reply.is_closed()))
    else {
        return false;
    };
    app.interaction.mode = crate::app::AppMode::ToolConfirm;
    app.interaction.kernel_busy_reply = Some(request.reply);
    (stream_state.notify)(
        "python kernel is busy",
        "an interrupted cell is still running",
        Some(crate::notify::Sound::Attention),
    );
    true
}

pub(super) fn answer_kernel_busy(
    session: &mut Session,
    choice: mush_tools::ipython::BusyKernelChoice,
) {
    let app = &mut session.app;
    if let Some(reply) = app.interaction.kernel_busy_reply.take() {
        let _ = reply.send(choice);
    }
    app.interaction.mode = crate::app::AppMode::Insert;
    app.status = Some(
        match choice {
            mush_tools::ipython::BusyKernelChoice::Wait => "waiting for the python kernel",
            mush_tools::ipython::BusyKernelChoice::Kill => "restarting the python kernel",
            mush_tools::ipython::BusyKernelChoice::Cancel => "cell cancelled",
        }
        .into(),
    );
}

pub(super) async fn answer_confirmation(
    session: &mut Session,
    _stream_state: &mut StreamState,
    allowed: bool,
) {
    let _ = session
        .connection
        .send(mush_runtime::ClientCommand::ConfirmTool { allow: allowed })
        .await;
    let app = &mut session.app;
    let pending_tool_call_id = app.interaction.confirm_tool_call_id.take();
    app.interaction.mode = crate::app::AppMode::Insert;
    app.interaction.confirm_prompt = None;
    if !allowed {
        app.status = Some(crate::app_state::Status::from(match pending_tool_call_id {
            Some(tool_call_id) => format!("tool denied: {tool_call_id}"),
            None => "tool denied".into(),
        }));
    }
}

pub(super) async fn submit_kernel_notices(
    session: &mut Session,
    _stream_state: &StreamState,
    notices: &Option<crate::runner::config::KernelNotices>,
) -> bool {
    let Some(notices) = notices else {
        return false;
    };
    let pending: Vec<mush_tools::kernel_jobs::JobNotice> = match notices.lock() {
        Ok(mut guard) if !guard.is_empty() => guard.drain(..).collect(),
        _ => return false,
    };
    for notice in pending {
        submit_notice(session, notice).await;
    }
    true
}

async fn submit_notice(session: &mut Session, notice: mush_tools::kernel_jobs::JobNotice) {
    let text = notice.done.text();
    session
        .app
        .push_bash_done(&notice.done, notice.origin.as_deref());
    if session.connection.turn_active() {
        let _ = session
            .connection
            .send(mush_runtime::ClientCommand::Steer {
                text,
                images: Vec::new(),
            })
            .await;
    } else {
        session.app.active_tools.clear();
        session.app.start_streaming();
        session.connection.runtime.pending_prompt = Some(PendingPrompt::text(text));
    }
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

pub(super) async fn handle_turn_ended(
    session: &mut Session,
    stream_state: &mut StreamState,
    pane_id: PaneId,
    tui_config: &TuiConfig,
    registry: &ApiRegistry,
    automatic_continuation: bool,
    cancelled: bool,
) {
    if !cancelled
        && !automatic_continuation
        && session.connection.runtime.pending_prompt.is_none()
        && session
            .connection
            .runtime
            .pending_compaction_inputs
            .is_empty()
        && !session.app.has_queued_messages()
    {
        (stream_state.notify)(
            "awaiting input",
            "task stopped and needs your input",
            Some(crate::notify::Sound::Complete),
        );
        stream_state.completion_signal();
    }

    if tui_config.generate_title && session.connection.runtime.title_due() {
        let opts = StreamOptions {
            api_key: tui_config.options.api_key.clone(),
            ..Default::default()
        };
        if let Some(title) = session
            .connection
            .runtime
            .generate_title(registry, &tui_config.model, &opts)
            .await
        {
            session.app.session_title = Some(title);
        }
    }

    if tui_config.auto_fork_compact {
        auto_compact(session, pane_id, tui_config, registry).await;
    }

    stream_state.save_session_now(session, tui_config);
}

pub(super) async fn handle_agent_event_side_effects(
    session: &mut Session,
    pane_id: PaneId,
    event: &AgentEvent,
    tui_config: &TuiConfig,
) {
    if let AgentEvent::MessageEnd { message } = event
        && message.provider == Provider::OpenRouter
        && let Some(gen_id) = message.generation_id.clone()
    {
        let model = session
            .connection
            .runtime
            .turns
            .meta(pane_id)
            .map(|m| m.model.clone())
            .unwrap_or_else(|| tui_config.model.clone());
        reconcile_openrouter_generation(session, pane_id, gen_id, &model, tui_config).await;
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
    let messages = pane.connection.runtime.conversation.context();
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
                &mut pane.connection.runtime,
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
                &mut pane.connection.runtime,
                applied,
            );
        }
    }
}

pub(super) async fn abort_focused_stream(session: &mut Session, _stream_state: &mut StreamState) {
    if let Some(control) = &session.connection.runtime.control {
        control.finish(mush_agent::ipc::control::Outcome::Cancelled);
        session.connection.runtime.pending_prompt = None;
    }
    record_cancelled_tools(session);

    let _ = session
        .connection
        .send(mush_runtime::ClientCommand::Abort)
        .await;

    let pane = &mut *session;
    pane.app.finish_streaming(None, None);
    let restored = pane.app.take_queued_messages();
    pane.connection.runtime.steering_queue.lock().await.clear();
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
        pane.connection
            .runtime
            .conversation
            .append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id,
                tool_name: tool_name.into(),
                content: result.content,
                outcome: result.outcome,
                timestamp_ms: Timestamp::now(),
                details: None,
            }));
    }

    pane.app.stream.active = false;
    pane.app.active_tools.clear();
    pane.app.status = Some("aborted".into());
}

pub(super) async fn submit_streaming_input(
    session: &mut Session,
    _stream_state: &StreamState,
    text: String,
    images: Vec<crate::input_buffer::PendingImage>,
) {
    if session.connection.turn_active() {
        let image_bytes: Vec<Vec<u8>> = images.iter().map(|img| img.data.clone()).collect();
        if image_bytes.is_empty() {
            session.app.push_queued_message(text.clone());
        } else {
            session
                .app
                .push_queued_message_with_images(text.clone(), image_bytes);
        }
        let _ = session
            .connection
            .send(mush_runtime::ClientCommand::Steer { text, images })
            .await;
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
        pane.connection.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images));
    }
}

pub(super) async fn edit_last_queued_steering(session: &mut Session, _stream_state: &StreamState) {
    let Some(queued_text) = session.app.pop_last_queued_message() else {
        session.app.status = Some("no queued messages to edit".into());
        return;
    };
    let current_input = session.app.input.take_text();
    if !current_input.trim().is_empty() {
        session.app.push_queued_message(&current_input);
        let _ = session
            .connection
            .send(mush_runtime::ClientCommand::Steer {
                text: current_input,
                images: Vec::new(),
            })
            .await;
    }

    let app = &mut session.app;
    app.input.text.clone_from(&queued_text);
    app.input.cursor = app.input.text.len();
    app.input.ensure_cursor_visible();

    let _ = session
        .connection
        .send(mush_runtime::ClientCommand::RetractSteer { text: queued_text })
        .await;
}

pub(super) use mush_runtime::CompactionTarget;

pub(super) async fn compaction_target(
    session: &Session,
    _pane_id: PaneId,
    tui_config: &TuiConfig,
) -> CompactionTarget {
    session
        .connection
        .runtime
        .compaction_target(&tui_config.turn_config())
        .await
}

pub(crate) fn build_session_snapshot(
    session: &Session,
    _tui_config: &super::TuiConfig,
) -> super::SessionSnapshot {
    session.connection.runtime.snapshot()
}

pub(crate) fn apply_session_events(
    session: &mut Session,
    tui_config: &mut super::TuiConfig,
    events: Vec<mush_runtime::SessionEvent>,
) {
    use mush_runtime::SessionEvent;
    for event in events {
        match event {
            SessionEvent::SessionIdChanged(id) => {
                tui_config.session_id = id.clone();
                tui_config.options.session_id = Some(id);
            }
            SessionEvent::SystemPromptChanged(prompt) => tui_config.system_prompt = prompt,
            SessionEvent::Title(title) => session.app.session_title = title,
            SessionEvent::TranscriptReplaced {
                reason,
                messages,
                editor_text,
            } => {
                let app = &mut session.app;
                crate::conversation_display::rebuild_display(app, &messages);
                if reason.keeps_live_stats() {
                    let estimate = mush_session::compact::estimate_tokens(&messages);
                    app.stats.reset_live_state(TokenCount::new(estimate as u64));
                } else {
                    app.stats.reset();
                    crate::conversation_display::accumulate_session_stats(app, &messages);
                }
                if let Some(text) = editor_text {
                    app.input.text = text;
                    app.input.cursor = app.input.text.len();
                }
            }
            SessionEvent::Status(status) => session.app.status = Some(status.into()),
            SessionEvent::Failed(message) => session.app.push_system_message(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mush_ai::models;
    use mush_ai::types::{ModelId, ToolCallId, UserContent, UserContentPart, UserMessage};
    use std::collections::HashMap;

    use mush_agent::{AgentConfig, agent_loop};
    use mush_runtime::turn::{build_context_transform, estimated_tokens_u64};
    use tokio::sync::Mutex;

    use crate::event_handler;

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
        let app = crate::app::App::new(switched.id.clone(), switched.context_window);
        let mut session = crate::session::Session::new(app);
        session.connection.runtime.model_id = switched.id.to_string();

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
                let applied = mush_runtime::apply_agent_event(&mut conversation, &event);
                event_handler::handle_agent_event(
                    &mut event_handler::EventCtx {
                        app: &mut app,
                        image_protos: &mut image_protos,
                    },
                    &event,
                    applied,
                    &model,
                    false,
                    &None,
                );
                if matches!(event, AgentEvent::ContextCompacted { .. }) {
                    let mut session =
                        Session::new(App::new(model.id.clone(), model.context_window));
                    session.connection.runtime.saver = tui_config.save_session.clone();
                    let (_confirm_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
                    let generation = session.connection.runtime.turns.register_active(
                        PaneId::new(1),
                        StreamMeta {
                            steering_queue: Arc::new(Mutex::new(Vec::new())),
                            confirm_req_rx,
                            confirm_reply: Arc::new(Mutex::new(None)),
                            model: model.clone(),
                            context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                            cancel: tokio_util::sync::CancellationToken::new(),
                        },
                    );
                    session.connection.runtime.conversation = conversation.clone();
                    session.connection.ingest_agent_event(
                        PaneId::new(1),
                        generation,
                        event.clone(),
                    );
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

    fn bash_done(command: &str, exit_code: i32) -> mush_tools::kernel_jobs::JobNotice {
        mush_tools::kernel_jobs::JobNotice {
            done: mush_tools::kernel_jobs::BashDone {
                pid: Some(77),
                exit_code,
                command: command.into(),
            },
            origin: None,
        }
    }

    #[tokio::test]
    async fn a_kernel_notice_becomes_a_prompt_when_idle_and_steering_when_busy() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let stream_state = StreamState::new();
        let notices: crate::runner::config::KernelNotices =
            Arc::new(std::sync::Mutex::new(vec![bash_done("exit 5", 5)]));

        assert!(submit_kernel_notices(&mut session, &stream_state, &Some(notices.clone())).await);
        assert_eq!(
            session
                .connection
                .runtime
                .pending_prompt
                .as_ref()
                .map(|p| p.text.as_str()),
            Some("[bash-done pid:77 exit:5]\n\nCommand: \"exit 5\"")
        );
        assert_eq!(
            session.app.messages.last().map(|m| m.role.clone()),
            Some(crate::app::MessageRole::BashDone)
        );
        assert!(!submit_kernel_notices(&mut session, &stream_state, &Some(notices.clone())).await);

        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));
        session.connection.runtime.turns.register_active(
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
        notices.lock().unwrap().push(bash_done("second", 0));

        assert!(submit_kernel_notices(&mut session, &stream_state, &Some(notices)).await);
        assert_eq!(steering_queue.lock().await.len(), 1);
        assert!(!session.app.messages.iter().any(|m| m.queued));
    }

    #[tokio::test]
    async fn a_busy_kernel_request_becomes_a_prompt_and_the_answer_reaches_the_tool() {
        use mush_tools::ipython::BusyKernelChoice;
        let mut session = Session::new(app());
        let stream_state = StreamState::new();
        let (reply, answer) = tokio::sync::oneshot::channel();
        let prompts: crate::runner::config::KernelPrompts = Arc::new(std::sync::Mutex::new(vec![
            crate::runner::config::KernelBusyRequest { reply },
        ]));

        assert!(poll_kernel_busy_prompt(
            &mut session,
            &stream_state,
            &Some(prompts.clone())
        ));
        assert_eq!(
            session.app.interaction.mode,
            crate::app::AppMode::ToolConfirm
        );
        assert!(!poll_kernel_busy_prompt(
            &mut session,
            &stream_state,
            &Some(prompts.clone())
        ));

        answer_kernel_busy(&mut session, BusyKernelChoice::Kill);

        assert_eq!(answer.await, Ok(BusyKernelChoice::Kill));
        assert_eq!(session.app.interaction.mode, crate::app::AppMode::Insert);
    }

    #[tokio::test]
    async fn an_aborted_cell_takes_its_busy_prompt_away() {
        let mut session = Session::new(app());
        let stream_state = StreamState::new();
        let (reply, answer) =
            tokio::sync::oneshot::channel::<mush_tools::ipython::BusyKernelChoice>();
        let prompts: crate::runner::config::KernelPrompts = Arc::new(std::sync::Mutex::new(vec![
            crate::runner::config::KernelBusyRequest { reply },
        ]));
        assert!(poll_kernel_busy_prompt(
            &mut session,
            &stream_state,
            &Some(prompts.clone())
        ));

        drop(answer);

        assert!(poll_kernel_busy_prompt(
            &mut session,
            &stream_state,
            &Some(prompts)
        ));
        assert_eq!(session.app.interaction.mode, crate::app::AppMode::Insert);
        assert!(session.app.interaction.kernel_busy_reply.is_none());
    }

    #[tokio::test]
    async fn a_notice_with_a_known_origin_attaches_to_its_cell() {
        let mut session = Session::new(app());
        let stream_state = StreamState::new();
        let mut cell = crate::app::DisplayMessage::new(crate::app::MessageRole::Assistant, "");
        cell.tool_calls.push(crate::app::DisplayToolCall {
            tool_call_id: "call_9".into(),
            name: "ipython".into(),
            summary: "h = bash('exit 5')".into(),
            status: crate::app::ToolCallStatus::Done,
            output: None,
            image_data: None,
            batch: 1,
            bash_done: None,
            cell: None,
        });
        session.app.messages.push(cell);
        let before = session.app.messages.len();
        let mut notice = bash_done("exit 5", 5);
        notice.origin = Some("call_9".into());
        let notices: crate::runner::config::KernelNotices =
            Arc::new(std::sync::Mutex::new(vec![notice]));

        assert!(submit_kernel_notices(&mut session, &stream_state, &Some(notices)).await);

        assert_eq!(session.app.messages.len(), before);
        let attached = &session.app.messages.last().unwrap().tool_calls[0].bash_done;
        assert_eq!(attached.as_ref().map(|d| d.exit_code), Some(5));
        assert!(session.connection.runtime.pending_prompt.is_some());
    }

    #[tokio::test]
    async fn submit_streaming_input_includes_pending_images_in_steering_queue() {
        use crate::input_buffer::PendingImage;
        use mush_ai::types::ImageMimeType;

        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let stream_state = StreamState::new();
        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));

        session.connection.runtime.turns.register_active(
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
        let stream_state = StreamState::new();
        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));

        session.connection.runtime.turns.register_active(
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
        let stream_state = StreamState::new();
        let (_confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = Arc::new(Mutex::new(Vec::new()));

        session.connection.runtime.turns.register_active(
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

    #[tokio::test]
    async fn register_active_supersedes_previous_generation() {
        let mut session = Session::new(app());
        let pane_id = PaneId::new(7);

        let (_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
        let gen1 = session.connection.runtime.turns.register_active(
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

        assert!(session.connection.runtime.turns.is_current(pane_id, gen1));
        assert!(session.connection.runtime.turns.meta(pane_id).is_some());
        let _ = ToolCallId::from("unused");
    }

    async fn poll_confirmation_prompt(
        session: &mut Session,
        stream_state: &mut StreamState,
    ) -> bool {
        match session.connection.try_next_event() {
            Some(mush_runtime::ClientEvent::ConfirmRequest {
                tool_call_id,
                prompt,
            }) => {
                show_confirm_request(session, stream_state, tool_call_id, prompt);
                true
            }
            _ => false,
        }
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

        session.connection.runtime.turns.register_active(
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

        let pending = session
            .connection
            .runtime
            .turns
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

        session.connection.runtime.turns.register_active(
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
            session
                .connection
                .runtime
                .turns
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

        session.connection.runtime.turns.register_active(
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
            kernel_prompts: None,
            kernel_state: None,
            rlm_depth: 0,
            rlm_max_depth: 2,
            forbidden_chars: Default::default(),
        }
    }

    #[test]
    fn build_user_content_persists_pending_prompt_text() {
        let content = mush_runtime::turn::build_user_content("hello from pending", &[]);

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
        let mut session = Session::new(app());
        let pane_id = PaneId::new(1);

        let (_tx1, rx1) = tokio::sync::mpsc::channel(1);
        let gen1 = session.connection.runtime.turns.register_active(
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

        session.connection.runtime.turns.abort(pane_id);

        let (_tx2, rx2) = tokio::sync::mpsc::channel(1);
        let gen2 = session.connection.runtime.turns.register_active(
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
            !session.connection.runtime.turns.is_current(pane_id, gen1),
            "stale generation must be rejected"
        );

        assert!(
            session.connection.runtime.turns.is_current(pane_id, gen2),
            "current generation must be accepted"
        );
    }

    #[tokio::test]
    async fn abort_without_reregister_rejects_all() {
        let mut session = Session::new(app());
        let pane_id = PaneId::new(1);

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let stream_gen = session.connection.runtime.turns.register_active(
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

        session.connection.runtime.turns.abort(pane_id);

        assert!(
            !session
                .connection
                .runtime
                .turns
                .is_current(pane_id, stream_gen)
        );
    }

    #[tokio::test]
    async fn abort_focused_stream_restores_queued_messages_without_duplicates() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        session.connection.runtime.turns.register_active(
            pane_id,
            StreamMeta {
                steering_queue: session.connection.runtime.steering_queue.clone(),
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

        assert_eq!(
            session.connection.runtime.steering_queue.lock().await.len(),
            2
        );
        assert_eq!(session.app.messages.iter().filter(|m| m.queued).count(), 2);

        abort_focused_stream(&mut session, &mut stream_state).await;

        let restored_input = &session.app.input.text;
        assert_eq!(
            restored_input, "first steer\nsecond steer",
            "abort must restore each queued message exactly once, got: {restored_input:?}"
        );
        assert!(
            session
                .connection
                .runtime
                .steering_queue
                .lock()
                .await
                .is_empty()
        );
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
        session.connection.runtime.turns.register_active(
            pane_id,
            StreamMeta {
                steering_queue: session.connection.runtime.steering_queue.clone(),
                confirm_req_rx: rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        let tool_call_id = ToolCallId::from("tc_cancelled");
        session
            .connection
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

        let ctx = session.connection.runtime.conversation.context();
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
        let mut session = Session::new(app());
        let config = test_tui_config();
        session.connection.runtime.system_prompt =
            Some("you are running inside mush\nproject context: foo".into());

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
        let config = test_tui_config();
        let mut session = Session::new(app());
        session.connection.runtime.saver = Some(std::sync::Arc::new(move |_| {
            count_clone.fetch_add(1, Ordering::Relaxed);
        }));
        let mut stream_state = StreamState::new();

        session.connection.runtime.save_debounced();
        assert_eq!(save_count.load(Ordering::Relaxed), 1);

        session.connection.runtime.save_debounced();
        assert_eq!(save_count.load(Ordering::Relaxed), 1);

        stream_state.save_session_now(&mut session, &config);
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
        session.connection.runtime.model_id = glm_id.to_string();

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

        let resolved = session.connection.runtime.model(&default);

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
