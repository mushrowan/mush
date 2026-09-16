use std::collections::VecDeque;

use mush_agent::AgentEvent;
use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    Message, PaneId, ThinkingLevel, Timestamp, TokenCount, ToolCallId, UserMessage,
};
use mush_session::compact::CompactionFocus;

use crate::Applied;
use crate::commands::{SessionCommand, SessionEvent};
use crate::compaction::{AppliedCompaction, CompactionTaskResult};
use crate::session::{
    PendingConfirmation, PendingImage, PendingPrompt, SessionRuntime, StreamGeneration,
};
use crate::turn::{TurnConfig, TurnDeps, TurnRequest, build_user_content};

pub const PRIMARY_PANE: PaneId = PaneId::new(1);
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(33);

#[derive(Debug, Clone)]
pub enum ClientCommand {
    Prompt {
        text: String,
        images: Vec<PendingImage>,
    },
    Steer {
        text: String,
        images: Vec<PendingImage>,
    },
    RetractSteer {
        text: String,
    },
    Abort,
    ConfirmTool {
        allow: bool,
    },
    SetModel(String),
    SetThinking(ThinkingLevel),
    Session(SessionCommand),
    Compact {
        focus: CompactionFocus,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum ClientEvent {
    Agent {
        generation: StreamGeneration,
        event: AgentEvent,
        applied: Applied,
    },
    Session(SessionEvent),
    TurnStarted,
    ConfirmRequest {
        tool_call_id: ToolCallId,
        prompt: String,
    },
    Compacting,
    Compacted(AppliedCompaction),
    CompactionFailed {
        failure: mush_session::compact::CompactionFailureReport,
        tokens_before: usize,
        model_id: String,
    },
    TurnEnded {
        cancelled: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionState {
    pub session_id: mush_ai::types::SessionId,
    pub title: Option<String>,
    pub model_id: String,
    pub thinking_level: ThinkingLevel,
    pub context_tokens: TokenCount,
    pub context_len: usize,
    pub turn_active: bool,
    pub compacting: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("compaction already in progress")]
    Compacting,
    #[error("conversation too short to compact")]
    TooShortToCompact,
    #[error("connection closed")]
    Closed,
}

#[async_trait::async_trait]
pub trait AgentConnection: Send {
    async fn send(&mut self, command: ClientCommand) -> Result<(), ConnectionError>;
    async fn next_event(&mut self) -> Option<ClientEvent>;
    fn state(&self) -> ConnectionState;
    fn transcript(&self) -> Vec<Message>;
}

pub struct InProcessConnection {
    pub runtime: SessionRuntime,
    tools: ToolRegistry,
    registry: ApiRegistry,
    config: TurnConfig,
    injections: mush_agent::AgentInjections,
    queued: VecDeque<ClientEvent>,
    compaction: Option<tokio::task::JoinHandle<CompactionTaskResult>>,
    closed: bool,
}

impl InProcessConnection {
    #[must_use]
    pub fn new(
        runtime: SessionRuntime,
        tools: ToolRegistry,
        registry: ApiRegistry,
        config: TurnConfig,
        injections: mush_agent::AgentInjections,
    ) -> Self {
        Self {
            runtime,
            tools,
            registry,
            config,
            injections,
            queued: VecDeque::new(),
            compaction: None,
            closed: false,
        }
    }

    #[must_use]
    pub fn detached(runtime: SessionRuntime) -> Self {
        let default_model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap_or_else(|| unreachable!("the model catalogue is never empty"));
        Self::new(
            runtime,
            ToolRegistry::new(),
            ApiRegistry::new(),
            TurnConfig {
                default_model,
                options: mush_ai::types::StreamOptions::default(),
                max_turns: 1,
                provider_api_keys: std::collections::HashMap::new(),
                confirm_tools: false,
                auto_compact: false,
                compaction_model: None,
                forbidden_chars: Default::default(),
                tool_allowlist: None,
            },
            mush_agent::AgentInjections::default(),
        )
    }

    #[must_use]
    pub fn turn_active(&self) -> bool {
        self.runtime.turns.meta(PRIMARY_PANE).is_some() || self.runtime.turn_active()
    }

    pub fn set_deps(&mut self, tools: ToolRegistry, registry: ApiRegistry) {
        self.tools = tools;
        self.registry = registry;
    }

    pub fn set_config(&mut self, config: TurnConfig) {
        self.config = config;
    }

    pub fn set_injections(&mut self, injections: mush_agent::AgentInjections) {
        self.injections = injections;
    }

    pub async fn start_pending(&mut self) -> bool {
        self.start_pending_prompt().await
    }

    pub fn try_next_event(&mut self) -> Option<ClientEvent> {
        if let Some(event) = self.queued.pop_front() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_confirm() {
            return Some(event);
        }
        self.try_poll_compaction()
    }

    fn try_poll_confirm(&mut self) -> Option<ClientEvent> {
        let meta = self.runtime.turns.meta_mut(PRIMARY_PANE)?;
        if meta.cancel.is_cancelled() {
            return None;
        }
        let mut reply = meta.confirm_reply.try_lock().ok()?;
        if reply.is_some() {
            return None;
        }
        let request = meta.confirm_req_rx.try_recv().ok()?;
        if request.reply.is_closed() {
            return None;
        }
        let event = ClientEvent::ConfirmRequest {
            tool_call_id: request.tool_call_id.clone(),
            prompt: request.prompt,
        };
        *reply = Some(PendingConfirmation {
            tool_call_id: request.tool_call_id,
            reply: request.reply,
        });
        Some(event)
    }

    fn try_poll_compaction(&mut self) -> Option<ClientEvent> {
        use futures::FutureExt;
        let task = self.compaction.as_mut()?;
        if !task.is_finished() {
            return None;
        }
        let result = task.now_or_never()?;
        self.compaction = None;
        Some(self.finish_compaction(result.ok()?))
    }

    fn finish_compaction(&mut self, result: CompactionTaskResult) -> ClientEvent {
        match result {
            CompactionTaskResult::Applied(applied) => {
                self.runtime.apply_compaction(
                    applied.summary.clone(),
                    applied.summarised_count,
                    applied.messages.clone(),
                );
                self.runtime.save_debounced();
                ClientEvent::Compacted(applied)
            }
            CompactionTaskResult::Failed {
                failure,
                tokens_before,
                model_id,
            } => ClientEvent::CompactionFailed {
                failure,
                tokens_before,
                model_id,
            },
        }
    }

    async fn start_pending_prompt(&mut self) -> bool {
        if self.runtime.turn_active()
            || self.compaction.is_some()
            || self.runtime.pending_prompt.is_none()
        {
            return false;
        }
        let prep = self.runtime.prepare_turn(&self.config).await;
        let Some(prompt) = self.runtime.pending_prompt.take() else {
            return false;
        };
        let deps = TurnDeps {
            config: self.config.clone(),
            injections: self.injections.clone(),
            tools: &self.tools,
            registry: &self.registry,
        };
        let request = TurnRequest {
            pane_id: PRIMARY_PANE,
            prompt,
        };
        self.runtime.start_prepared_turn(request, prep, &deps);
        self.queued.push_back(ClientEvent::TurnStarted);
        true
    }

    async fn steer(&mut self, text: String, images: Vec<PendingImage>) {
        let Some(meta) = self.runtime.turns.meta(PRIMARY_PANE) else {
            self.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images));
            return;
        };
        meta.steering_queue.lock().await.push(Message::User(
            UserMessage::builder()
                .content(build_user_content(&text, &images))
                .timestamp_ms(Timestamp::now())
                .prompt_id(mush_ai::types::PromptId::new())
                .build(),
        ));
    }

    async fn retract_steer(&mut self, text: &str) -> bool {
        let Some(meta) = self.runtime.turns.meta(PRIMARY_PANE) else {
            return false;
        };
        let mut queue = meta.steering_queue.lock().await;
        let matches = |exact: bool| {
            queue.iter().rposition(|message| match message {
                Message::User(user) if exact => user.content.text() == text,
                Message::User(user) => user.content.text().trim() == text.trim(),
                _ => false,
            })
        };
        let Some(position) = matches(true).or_else(|| matches(false)) else {
            return false;
        };
        queue.remove(position);
        true
    }

    async fn confirm(&mut self, allow: bool) {
        if let Some(meta) = self.runtime.turns.meta_mut(PRIMARY_PANE)
            && let Some(pending) = meta.confirm_reply.lock().await.take()
        {
            let _ = pending.reply.send(allow);
        }
    }

    async fn start_compaction(&mut self, focus: CompactionFocus) -> Result<(), ConnectionError> {
        if self.compaction.is_some() {
            return Err(ConnectionError::Compacting);
        }
        let messages = self.runtime.conversation.context();
        if messages.len() <= crate::compaction::MIN_MESSAGES_FOR_COMPACTION {
            return Err(ConnectionError::TooShortToCompact);
        }
        let target = self.runtime.compaction_target(&self.config).await;
        let task = crate::compaction::start_compaction(
            messages,
            target.model,
            target.options,
            self.registry.clone(),
            Some(self.injections.lifecycle_hooks.clone()),
            self.injections.cwd.clone(),
            focus,
        );
        self.compaction = Some(task);
        self.queued.push_back(ClientEvent::Compacting);
        Ok(())
    }

    pub fn ingest_agent_event(
        &mut self,
        pane: PaneId,
        generation: StreamGeneration,
        event: AgentEvent,
    ) -> Option<ClientEvent> {
        if !self.runtime.turns.is_current(pane, generation) {
            return None;
        }
        let applied = self.runtime.apply_event(&event);
        self.on_agent_event(&event);
        Some(ClientEvent::Agent {
            generation,
            event,
            applied,
        })
    }

    fn on_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageEnd { message } => {
                if let Some(meta) = self.runtime.turns.meta(PRIMARY_PANE) {
                    meta.context_tokens.store(
                        message.usage.total_input_tokens().get(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                self.runtime.save_debounced();
            }
            AgentEvent::ToolExecEnd { .. } | AgentEvent::ContextCompacted { .. } => {
                self.runtime.save_debounced();
            }
            AgentEvent::AgentEnd => {
                let cancelled = self
                    .runtime
                    .turns
                    .remove(PRIMARY_PANE)
                    .is_none_or(|meta| meta.cancel.is_cancelled());
                self.runtime.save_now_and_settle();
                self.queued.push_back(ClientEvent::TurnEnded { cancelled });
            }
            _ => {}
        }
    }
}

#[async_trait::async_trait]
impl AgentConnection for InProcessConnection {
    async fn send(&mut self, command: ClientCommand) -> Result<(), ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        match command {
            ClientCommand::Prompt { text, images } => {
                if self.turn_active() {
                    self.steer(text, images).await;
                } else {
                    self.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images));
                    self.start_pending_prompt().await;
                }
            }
            ClientCommand::Steer { text, images } => self.steer(text, images).await,
            ClientCommand::RetractSteer { text } => {
                if !self.retract_steer(&text).await {
                    tracing::warn!(%text, "no steering queue entry matched the retracted message");
                }
            }
            ClientCommand::Abort => self.runtime.turns.abort(PRIMARY_PANE),
            ClientCommand::ConfirmTool { allow } => self.confirm(allow).await,
            ClientCommand::SetModel(id) => self.runtime.model_id = id,
            ClientCommand::SetThinking(level) => self.runtime.thinking_level = level,
            ClientCommand::Session(command) => {
                let events = self.runtime.handle(command).await;
                self.queued
                    .extend(events.into_iter().map(ClientEvent::Session));
            }
            ClientCommand::Compact { focus } => self.start_compaction(focus).await?,
            ClientCommand::Shutdown => {
                self.runtime.turns.abort(PRIMARY_PANE);
                let id = self.runtime.session_id.clone();
                self.runtime.snapshot_kernel(&id).await;
                self.runtime.save_now_and_settle();
                self.closed = true;
            }
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Option<ClientEvent> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Some(event);
            }
            if self.closed {
                return None;
            }
            if let Some(event) = self.try_next_event() {
                return Some(event);
            }
            if !self.runtime.turn_active() {
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }
            let turn_event = tokio::select! {
                event = self.runtime.turns.next() => event,
                () = tokio::time::sleep(IDLE_POLL) => continue,
            };
            let Some((pane, generation, event)) = turn_event else {
                continue;
            };
            if let Some(event) = self.ingest_agent_event(pane, generation, event) {
                return Some(event);
            }
        }
    }

    fn state(&self) -> ConnectionState {
        ConnectionState {
            session_id: self.runtime.session_id.clone(),
            title: self.runtime.title.clone(),
            model_id: self.runtime.model_id.clone(),
            thinking_level: self.runtime.thinking_level.clone(),
            context_tokens: self.runtime.context_tokens,
            context_len: self.runtime.conversation.context_len(),
            turn_active: self.turn_active(),
            compacting: self.compaction.is_some(),
        }
    }

    fn transcript(&self) -> Vec<Message> {
        self.runtime.conversation.context()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{
        Api, AssistantContentPart, AssistantMessage, Model, StopReason, StreamOptions, TextContent,
    };
    use mush_session::ConversationState;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct Echo {
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    #[async_trait::async_trait]
    impl mush_ai::registry::ApiProvider for Echo {
        fn api(&self) -> Api {
            Api::AnthropicMessages
        }

        async fn stream(
            &self,
            model: &Model,
            context: &mush_ai::registry::LlmContext,
            _options: &StreamOptions,
        ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
            self.requests.lock().unwrap().push(context.messages.clone());
            let message = AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: "hi back".into(),
                })])
                .model(model.id.clone())
                .provider(model.provider.clone())
                .api(model.api)
                .stop_reason(StopReason::Stop)
                .timestamp_ms(Timestamp::zero())
                .build();
            Ok(Box::pin(futures::stream::iter([
                mush_ai::stream::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "hi back".into(),
                },
                mush_ai::stream::StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                },
            ])))
        }
    }

    fn connection() -> (InProcessConnection, Arc<Mutex<Vec<Vec<Message>>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(Echo {
            requests: requests.clone(),
        }));
        let model = mush_ai::models::all_models()
            .into_iter()
            .find(|model| model.api == Api::AnthropicMessages)
            .unwrap();
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.model_id = model.id.to_string();
        let config = TurnConfig {
            default_model: model,
            options: StreamOptions::default(),
            max_turns: 4,
            provider_api_keys: HashMap::new(),
            confirm_tools: false,
            auto_compact: false,
            compaction_model: None,
            forbidden_chars: Default::default(),
            tool_allowlist: None,
        };
        let connection = InProcessConnection::new(
            runtime,
            ToolRegistry::new(),
            registry,
            config,
            mush_agent::AgentInjections::default(),
        );
        (connection, requests)
    }

    async fn drain_turn(connection: &mut InProcessConnection) -> Vec<ClientEvent> {
        let mut events = Vec::new();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(5), connection.next_event())
                    .await
                    .expect("an event before the timeout")
                    .expect("the connection stays open");
            let done = matches!(event, ClientEvent::TurnEnded { .. });
            events.push(event);
            if done {
                return events;
            }
        }
    }

    #[tokio::test]
    async fn a_prompt_runs_a_turn_and_the_transcript_holds_both_sides() {
        let (mut connection, requests) = connection();

        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_turn(&mut connection).await;

        assert!(matches!(events[0], ClientEvent::TurnStarted), "{events:?}");
        assert!(events.iter().any(|event| matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::MessageEnd { .. },
                applied: Applied::Appended,
                ..
            }
        )));
        let transcript = connection.transcript();
        assert_eq!(transcript.len(), 2);
        assert!(matches!(&transcript[0], Message::User(user) if user.text() == "hello"));
        assert!(matches!(&transcript[1], Message::Assistant(_)));
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(!connection.state().turn_active);
        assert_eq!(connection.state().context_len, 2);
    }

    #[tokio::test]
    async fn a_new_session_replaces_the_transcript_through_events() {
        let (mut connection, _) = connection();
        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        let before = connection.state().session_id.clone();

        connection
            .send(ClientCommand::Session(SessionCommand::New))
            .await
            .unwrap();
        let first = connection.next_event().await.unwrap();

        assert!(matches!(
            first,
            ClientEvent::Session(SessionEvent::SessionIdChanged(_))
        ));
        assert_ne!(connection.state().session_id, before);
        assert!(connection.transcript().is_empty());
        assert_eq!(connection.state().context_tokens, TokenCount::ZERO);
    }

    #[tokio::test]
    async fn compacting_a_short_conversation_is_refused() {
        let (mut connection, _) = connection();

        let outcome = connection
            .send(ClientCommand::Compact {
                focus: CompactionFocus::default(),
            })
            .await;

        assert!(matches!(outcome, Err(ConnectionError::TooShortToCompact)));
        assert!(!connection.state().compacting);
    }

    #[tokio::test]
    async fn shutdown_closes_the_event_stream() {
        let (mut connection, _) = connection();

        connection.send(ClientCommand::Shutdown).await.unwrap();

        assert!(connection.next_event().await.is_none());
        assert!(matches!(
            connection.send(ClientCommand::Abort).await,
            Err(ConnectionError::Closed)
        ));
    }
}
