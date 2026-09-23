use std::sync::Arc;

use mush_agent::AgentEvent;
use mush_ai::types::{Model, Provider, TokenCount, ToolOutcome};

use crate::session::Session;
use mush_ai::types::PaneId;

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

pub(super) fn poll_kernel_busy_prompt(session: &mut Session, stream_state: &StreamState) -> bool {
    let app = &mut session.app;
    if app.interaction.kernel_busy != crate::app_state::KernelBusy::Pending
        || app.interaction.mode != crate::app::AppMode::Insert
    {
        return false;
    }
    app.interaction.mode = crate::app::AppMode::ToolConfirm;
    app.interaction.kernel_busy = crate::app_state::KernelBusy::Asked;
    (stream_state.notify)(
        "python kernel is busy",
        "an interrupted cell is still running",
        Some(crate::notify::Sound::Attention),
    );
    true
}

pub(super) fn withdraw_kernel_busy(session: &mut Session) {
    let app = &mut session.app;
    if app.interaction.kernel_busy == crate::app_state::KernelBusy::Asked
        && app.interaction.mode == crate::app::AppMode::ToolConfirm
    {
        app.interaction.mode = crate::app::AppMode::Insert;
    }
    app.interaction.kernel_busy = crate::app_state::KernelBusy::None;
}

pub(super) async fn answer_kernel_busy(
    session: &mut Session,
    choice: mush_tools::ipython::BusyKernelChoice,
) {
    let _ = session
        .connection
        .send(mush_runtime::ClientCommand::AnswerKernelBusy(choice))
        .await;
    let app = &mut session.app;
    app.interaction.kernel_busy = crate::app_state::KernelBusy::None;
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

pub(super) async fn handle_turn_ended(
    session: &mut Session,
    stream_state: &mut StreamState,
    automatic_continuation: bool,
    cancelled: bool,
) {
    if !cancelled
        && !automatic_continuation
        && !session.connection.state().pending_prompt
        && !session.app.has_queued_messages()
    {
        (stream_state.notify)(
            "awaiting input",
            "task stopped and needs your input",
            Some(crate::notify::Sound::Complete),
        );
        stream_state.completion_signal();
    }
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
            .state()
            .turn_model
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

pub(super) async fn abort_focused_stream(session: &mut Session, _stream_state: &mut StreamState) {
    let events = session
        .connection
        .send(mush_runtime::ClientCommand::AbortAndSendQueued)
        .await
        .unwrap_or_default();
    let mut restarted = false;
    for event in events {
        match event {
            mush_runtime::ClientEvent::ToolsCancelled(ids) => show_cancelled_tools(session, &ids),
            mush_runtime::ClientEvent::TurnStarted => restarted = true,
            _ => {}
        }
    }

    let pane = &mut *session;
    pane.app.finish_streaming(None, None);
    pane.app.stream.active = false;
    pane.app.active_tools.clear();
    pane.app.end_turn();
    pane.app.status = Some(
        if pane.app.quit_hint_armed() {
            "aborted, press ctrl+c again to exit"
        } else {
            "aborted"
        }
        .into(),
    );
    if restarted {
        pane.app.promote_queued_messages();
        pane.app.begin_turn();
        pane.app.start_streaming();
        return;
    }
    let restored = pane.app.take_queued_messages();
    if !restored.is_empty() {
        let text = restored.join("\n");
        pane.app.input.text = text.clone();
        pane.app.input.cursor = text.len();
    }
}

fn show_cancelled_tools(pane: &mut Session, ids: &[mush_ai::types::ToolCallId]) {
    let running: Vec<_> = pane
        .app
        .active_tools
        .iter()
        .filter(|tool| ids.contains(&tool.tool_call_id))
        .map(|tool| (tool.tool_call_id.clone(), tool.name.clone()))
        .collect();
    for (tool_call_id, tool_name) in running {
        pane.app.end_tool(
            &tool_call_id,
            &tool_name,
            ToolOutcome::Error,
            Some(mush_runtime::session::CANCELLED_TOOL_MESSAGE),
            None,
        );
    }
}

pub(super) async fn submit_streaming_input(
    session: &mut Session,
    _stream_state: &StreamState,
    text: String,
    images: Vec<crate::input_buffer::PendingImage>,
) {
    if session.connection.state().turn_active {
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
        let _ = pane
            .connection
            .send(mush_runtime::ClientCommand::QueuePrompt { text, images })
            .await;
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

pub(crate) fn apply_session_events(
    session: &mut Session,
    tui_config: &mut super::TuiConfig,
    events: Vec<mush_runtime::SessionEvent>,
) {
    use mush_runtime::SessionEvent;
    for event in events {
        match event {
            SessionEvent::SessionIdChanged(id) => {
                session.app.clear_children();
                tui_config.session_id = id.clone();
                tui_config.options.session_id = Some(id);
            }
            SessionEvent::SystemPromptChanged(_) => {}
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
            SessionEvent::ThinkingLevel(level) => session.app.thinking_level = level,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mush_ai::models;
    use mush_ai::registry::ApiRegistry;
    use mush_ai::types::{
        Message, ModelId, StreamOptions, Timestamp, ToolCallId, UserContent, UserContentPart,
        UserMessage,
    };
    use std::collections::HashMap;

    use mush_agent::{AgentConfig, agent_loop};
    use mush_runtime::turn::{build_context_transform, estimated_tokens_u64};
    use tokio::sync::Mutex;

    use crate::event_handler;

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
    use crate::runner::looping::apply_command_events;
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
        let saver: mush_runtime::SessionSaver = Arc::new(move |snapshot| {
            *save_target.lock().unwrap() = Some(snapshot);
        });
        for turn in 0..2 {
            let messages = conversation.context();
            let transform = build_context_transform(
                mush_runtime::CompactionTarget::session_model(
                    model.clone(),
                    StreamOptions::default(),
                ),
                true,
                mush_session::compact::CompactionSettings {
                    keep_recent_tokens: 5_000,
                    ..mush_session::compact::CompactionSettings::default()
                },
                TokenCount::new(10_000),
                registry.clone(),
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                estimated_tokens_u64(&messages),
                mush_agent::LifecycleHooks::default(),
                std::path::PathBuf::from("/tmp"),
                None,
                None,
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
                    session.in_process().runtime.saver = Some(saver.clone());
                    let (_confirm_tx, confirm_req_rx) = tokio::sync::mpsc::channel(1);
                    let generation = session.in_process().runtime.turns.register_active(
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
                    session.in_process().runtime.conversation = conversation.clone();
                    session.in_process().ingest_agent_event(
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
    async fn a_bash_done_event_lands_in_the_transcript() {
        let mut session = Session::new(app());
        let mut config = test_tui_config();

        apply_command_events(
            &mut session,
            &mut config,
            vec![mush_runtime::ClientEvent::BashDone(bash_done("exit 5", 5))],
        )
        .await;

        assert_eq!(
            session.app.messages.last().map(|m| m.role.clone()),
            Some(crate::app::MessageRole::BashDone)
        );
    }

    #[tokio::test]
    async fn a_busy_kernel_event_becomes_a_question_and_the_answer_goes_to_the_worker() {
        use mush_tools::ipython::BusyKernelChoice;
        let mut session = Session::new(app());
        let mut config = test_tui_config();
        let stream_state = StreamState::new();

        apply_command_events(
            &mut session,
            &mut config,
            vec![mush_runtime::ClientEvent::KernelBusy],
        )
        .await;
        assert!(poll_kernel_busy_prompt(&mut session, &stream_state));
        assert_eq!(
            session.app.interaction.mode,
            crate::app::AppMode::ToolConfirm
        );
        assert!(!poll_kernel_busy_prompt(&mut session, &stream_state));

        answer_kernel_busy(&mut session, BusyKernelChoice::Kill).await;

        assert_eq!(session.app.interaction.mode, crate::app::AppMode::Insert);
        assert_eq!(
            session.app.interaction.kernel_busy,
            crate::app_state::KernelBusy::None
        );
    }

    #[tokio::test]
    async fn a_withdrawn_busy_kernel_question_closes_the_prompt() {
        let mut session = Session::new(app());
        let mut config = test_tui_config();
        let stream_state = StreamState::new();
        apply_command_events(
            &mut session,
            &mut config,
            vec![mush_runtime::ClientEvent::KernelBusy],
        )
        .await;
        assert!(poll_kernel_busy_prompt(&mut session, &stream_state));

        apply_command_events(
            &mut session,
            &mut config,
            vec![mush_runtime::ClientEvent::KernelBusyWithdrawn],
        )
        .await;

        assert_eq!(session.app.interaction.mode, crate::app::AppMode::Insert);
        assert_eq!(
            session.app.interaction.kernel_busy,
            crate::app_state::KernelBusy::None
        );
    }

    #[tokio::test]
    async fn a_notice_with_a_known_origin_attaches_to_its_cell() {
        let mut session = Session::new(app());
        let mut config = test_tui_config();
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

        apply_command_events(
            &mut session,
            &mut config,
            vec![mush_runtime::ClientEvent::BashDone(notice)],
        )
        .await;

        assert_eq!(session.app.messages.len(), before);
        let attached = &session.app.messages.last().unwrap().tool_calls[0].bash_done;
        assert_eq!(attached.as_ref().map(|d| d.exit_code), Some(5));
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

        session.in_process().runtime.turns.register_active(
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

        session.in_process().runtime.turns.register_active(
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

        session.in_process().runtime.turns.register_active(
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
        let gen1 = session.in_process().runtime.turns.register_active(
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

        assert!(session.in_process().runtime.turns.is_current(pane_id, gen1));
        assert!(session.in_process().runtime.turns.meta(pane_id).is_some());
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

        session.in_process().runtime.turns.register_active(
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
            .in_process()
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

        session.in_process().runtime.turns.register_active(
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
                .in_process()
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

        session.in_process().runtime.turns.register_active(
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

    async fn tool_output(session: &mut Session, entries: &[(&str, &str)]) {
        let mut config = test_tui_config();
        let events = entries
            .iter()
            .map(|(exec_id, line)| mush_runtime::ClientEvent::ToolOutput {
                exec_id: (*exec_id).to_string(),
                line: (*line).to_string(),
            })
            .collect();
        apply_command_events(session, &mut config, events).await;
    }

    #[tokio::test]
    async fn tool_output_updates_the_focused_tool() {
        let mut session = Session::new(app());
        let tool_call_id = ToolCallId::from("tc_live");
        session.app.start_tool(&tool_call_id, "bash", "cargo test");

        tool_output(&mut session, &[("tc_live", "running")]).await;

        assert_eq!(
            session.app.active_tools[0].live_output.as_deref(),
            Some("running")
        );
    }

    #[tokio::test]
    async fn tool_output_routes_each_exec_id_to_its_own_panel() {
        let mut session = Session::new(app());
        let a = ToolCallId::from("tc_a");
        let b = ToolCallId::from("tc_b");
        session.app.start_tool(&a, "bash", "jj st");
        session.app.start_tool(&b, "bash", "cargo test");

        tool_output(
            &mut session,
            &[("tc_a", "jj output"), ("tc_b", "test output")],
        )
        .await;

        let tools = &session.app.active_tools;
        assert_eq!(tools[0].live_output.as_deref(), Some("jj output"));
        assert_eq!(tools[1].live_output.as_deref(), Some("test output"));
    }

    fn test_tui_config() -> super::super::TuiConfig {
        let model = models::all_models_with_user().into_iter().next().unwrap();
        super::super::TuiConfig {
            model,
            options: mush_ai::types::StreamOptions::default(),
            initial_title: None,
            theme: crate::theme::Theme::default(),
            config_path: None,
            provider_api_keys: std::collections::HashMap::new(),
            last_models: super::super::LastModels::default(),
            save_last_models: None,
            feature_hints: true,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar_config: Default::default(),
            debug_cache: false,
            cache_timer: false,
            thinking_display: crate::app::ThinkingDisplay::Collapse,
            log_buffer: None,
            terminal_policy: crate::terminal_policy::TerminalPolicy::default(),
            cwd: std::path::PathBuf::from("/tmp"),
            agent_card: None,
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
            open_session_picker: false,
            branch_summary_skip_prompt: false,
            startup_notice: None,
            startup_commands: Vec::new(),
            reload_context: None,
            mcp: None,
            skills: Default::default(),
            autonomous: Default::default(),
            worker_opener: None,
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
        let gen1 = session.in_process().runtime.turns.register_active(
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

        session.in_process().runtime.turns.abort(pane_id);

        let (_tx2, rx2) = tokio::sync::mpsc::channel(1);
        let gen2 = session.in_process().runtime.turns.register_active(
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
            !session.in_process().runtime.turns.is_current(pane_id, gen1),
            "stale generation must be rejected"
        );

        assert!(
            session.in_process().runtime.turns.is_current(pane_id, gen2),
            "current generation must be accepted"
        );
    }

    #[tokio::test]
    async fn abort_without_reregister_rejects_all() {
        let mut session = Session::new(app());
        let pane_id = PaneId::new(1);

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let stream_gen = session.in_process().runtime.turns.register_active(
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

        session.in_process().runtime.turns.abort(pane_id);

        assert!(
            !session
                .in_process()
                .runtime
                .turns
                .is_current(pane_id, stream_gen)
        );
    }

    #[tokio::test]
    async fn abort_focused_stream_sends_queued_steers_as_one_new_turn() {
        let pane_id = PaneId::new(1);
        let mut session = Session::new(app());
        let mut stream_state = StreamState::new();

        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let steering_queue = session.in_process().runtime.steering_queue.clone();
        session.in_process().runtime.turns.register_active(
            pane_id,
            StreamMeta {
                steering_queue,
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
            session
                .in_process()
                .runtime
                .steering_queue
                .lock()
                .await
                .len(),
            2
        );
        assert_eq!(session.app.messages.iter().filter(|m| m.queued).count(), 2);

        abort_focused_stream(&mut session, &mut stream_state).await;

        assert!(
            session.app.input.text.is_empty(),
            "{:?}",
            session.app.input.text
        );
        let users: Vec<&str> = session
            .app
            .messages
            .iter()
            .filter(|m| matches!(m.role, crate::app::MessageRole::User))
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(users, ["first steer", "second steer"]);
        assert!(session.app.stream.active);
        let sent: Vec<String> = session
            .in_process()
            .runtime
            .conversation
            .context()
            .iter()
            .filter_map(|message| match message {
                mush_ai::types::Message::User(user) => Some(user.text()),
                _ => None,
            })
            .collect();
        assert_eq!(sent, ["first steer", "second steer"]);
        assert!(
            session
                .in_process()
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
        let steering_queue = session.in_process().runtime.steering_queue.clone();
        session.in_process().runtime.turns.register_active(
            pane_id,
            StreamMeta {
                steering_queue,
                confirm_req_rx: rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: models::all_models_with_user().into_iter().next().unwrap(),
                context_tokens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        );

        let tool_call_id = ToolCallId::from("tc_cancelled");
        session
            .in_process()
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

        let ctx = session.in_process().runtime.conversation.context();
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
        session.in_process().runtime.system_prompt =
            Some("you are running inside mush\nproject context: foo".into());

        let snapshot = session.in_process().runtime.snapshot();

        assert_eq!(
            snapshot.system_prompt.as_deref(),
            Some("you are running inside mush\nproject context: foo")
        );
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
        session.in_process().runtime.model_id = glm_id.to_string();

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

        let resolved = session.in_process().runtime.model(&default);

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
