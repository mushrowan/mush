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
use crate::turn::{TurnConfig, TurnDeps, TurnRequest, build_user_content, resolve_auth_for_model};

pub const PRIMARY_PANE: PaneId = PaneId::new(1);
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(33);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientCommand {
    Prompt {
        text: String,
        images: Vec<PendingImage>,
    },
    QueuePrompt {
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
    SetRlmMaxDepth(u32),
    SetSystemPrompt(Option<String>),
    Session(SessionCommand),
    Compact {
        focus: CompactionFocus,
        auto: bool,
    },
    CompactionRetry,
    CompactionFallback,
    Shutdown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
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
    ToolsCancelled(Vec<ToolCallId>),
    MessageInjected(mush_ai::types::EventMessage),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConnectionState {
    pub session_id: mush_ai::types::SessionId,
    pub title: Option<String>,
    pub model_id: String,
    pub thinking_level: ThinkingLevel,
    pub context_tokens: TokenCount,
    pub context_len: usize,
    pub tree_len: usize,
    pub rlm_depth: u32,
    pub rlm_max_depth: u32,
    pub turn_active: bool,
    pub turn_model: Option<mush_ai::types::Model>,
    pub active_turns: usize,
    pub compacting: bool,
    pub pending_prompt: bool,
    pub controlled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ConnectionError {
    #[error("compaction already in progress")]
    Compacting,
    #[error("conversation too short to compact")]
    TooShortToCompact,
    #[error("no failed compaction to recover")]
    NothingToRecover,
    #[error("connection closed")]
    Closed,
    #[error("transport: {0}")]
    Transport(String),
}

#[async_trait::async_trait]
pub trait AgentConnection: Send {
    async fn send(&mut self, command: ClientCommand) -> Result<Vec<ClientEvent>, ConnectionError>;
    async fn next_event(&mut self) -> Option<ClientEvent>;
    fn try_next_event(&mut self) -> Option<ClientEvent>;
    fn state(&self) -> ConnectionState;
    async fn transcript(&mut self) -> Vec<Message>;
    async fn history(&mut self) -> Vec<Message>;
    async fn tree(&mut self) -> mush_session::tree::SessionTree;
}

pub struct InProcessConnection {
    pub runtime: SessionRuntime,
    tools: ToolRegistry,
    registry: ApiRegistry,
    config: TurnConfig,
    injections: mush_agent::AgentInjections,
    queued: VecDeque<ClientEvent>,
    compaction: Option<tokio::task::JoinHandle<CompactionTaskResult>>,
    compaction_auto: bool,
    failed_compaction: Option<FailedCompaction>,
    title: Option<tokio::task::JoinHandle<Option<String>>>,
    closed: bool,
}

struct FailedCompaction {
    failure: mush_session::compact::CompactionFailureReport,
    tokens_before: usize,
    model_id: String,
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
            compaction_auto: false,
            failed_compaction: None,
            title: None,
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
                generate_title: false,
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

    pub fn attach_control(
        &mut self,
        control: mush_agent::ipc::control::Control,
        socket: std::path::PathBuf,
    ) {
        self.runtime.control = Some(control);
        self.runtime.control_socket = Some(socket);
    }

    pub async fn start_pending(&mut self) -> bool {
        self.poll_control().await;
        let started = self.start_pending_prompt().await;
        self.runtime.save_if_dirty();
        started
    }

    pub async fn idle(&self) -> bool {
        !self.turn_active()
            && self.compaction.is_none()
            && self.runtime.pending_prompt.is_none()
            && self.runtime.pending_compaction_inputs.is_empty()
            && self.runtime.steering_queue.lock().await.is_empty()
            && self.runtime.conversation.leaf_id().is_none_or(|leaf| {
                self.runtime
                    .conversation
                    .tree()
                    .unanswered_tool_calls(leaf)
                    .is_empty()
            })
    }

    fn sync_control_target(&self) {
        if let Some(control) = &self.runtime.control
            && control.target().session != self.runtime.session_id.as_str()
        {
            control.activate(self.runtime.session_id.to_string());
        }
    }

    pub async fn poll_control(&mut self) {
        let Some(control) = self.runtime.control.clone() else {
            return;
        };
        self.sync_control_target();
        if !self.idle().await {
            return;
        }
        control.finish(mush_agent::ipc::control::Outcome::Failed);
        let Some(delivery) = control.take_next(&control.target()) else {
            return;
        };
        let message = match delivery.message() {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(%error, "dropping a control delivery that isn't a message");
                return;
            }
        };
        let Message::Event(event) = message else {
            return;
        };
        self.runtime.accept_event(event.clone());
        self.queued.push_back(ClientEvent::MessageInjected(event));
    }

    fn poll_background(&mut self) -> Option<ClientEvent> {
        if let Some(event) = self.queued.pop_front() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_confirm() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_title() {
            return Some(event);
        }
        self.try_poll_compaction()
    }

    fn try_poll_title(&mut self) -> Option<ClientEvent> {
        use futures::FutureExt;
        let task = self.title.as_mut()?;
        if !task.is_finished() {
            return None;
        }
        let title = task.now_or_never()?;
        self.title = None;
        let title = title.ok().flatten()?;
        self.runtime.title = Some(title.clone());
        self.runtime.save_debounced();
        Some(ClientEvent::Session(SessionEvent::Title(Some(title))))
    }

    fn spawn_title(&mut self) {
        if !self.config.generate_title || !self.runtime.title_due() || self.title.is_some() {
            return;
        }
        self.runtime.title_generated = true;
        let messages = self.runtime.conversation.context_prefix(4);
        let registry = self.registry.clone();
        let model = self.config.default_model.clone();
        let keys = self.config.provider_api_keys.clone();
        self.title = Some(tokio::spawn(async move {
            let (api_key, account_id) = resolve_auth_for_model(&model, &keys).await;
            let options = mush_ai::types::StreamOptions {
                api_key,
                account_id,
                ..Default::default()
            };
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                mush_session::title::generate_title(messages, &registry, &model, &options),
            )
            .await
            .ok()
            .flatten()
        }));
    }

    async fn abort(&mut self) -> Vec<ClientEvent> {
        if let Some(control) = &self.runtime.control {
            control.finish(mush_agent::ipc::control::Outcome::Cancelled);
            self.runtime.pending_prompt = None;
        }
        let cancelled = self.runtime.record_cancelled_tools();
        self.runtime.turns.abort(PRIMARY_PANE);
        self.runtime.steering_queue.lock().await.clear();
        self.runtime.save_debounced();
        if cancelled.is_empty() {
            Vec::new()
        } else {
            vec![ClientEvent::ToolsCancelled(cancelled)]
        }
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
        let result = result.ok()?;
        if self.compaction_auto
            && let CompactionTaskResult::Failed {
                failure,
                tokens_before,
                model_id,
            } = result
        {
            let hooks = self.injections.lifecycle_hooks.clone();
            let cwd = self.injections.cwd.clone();
            self.compaction = Some(tokio::spawn(async move {
                CompactionTaskResult::Applied(
                    crate::compaction::fallback_applied_compaction(
                        failure,
                        tokens_before,
                        model_id,
                        Some(&hooks),
                        cwd.as_deref(),
                    )
                    .await,
                )
            }));
            return None;
        }
        Some(self.finish_compaction(result))
    }

    fn finish_compaction(&mut self, result: CompactionTaskResult) -> ClientEvent {
        match result {
            CompactionTaskResult::Applied(applied) => {
                self.runtime.apply_compaction(
                    applied.summary.clone(),
                    applied.summarised_count,
                    applied.messages.clone(),
                );
                self.runtime.save_now_and_settle();
                ClientEvent::Compacted(applied)
            }
            CompactionTaskResult::Failed {
                failure,
                tokens_before,
                model_id,
            } => {
                self.failed_compaction = Some(FailedCompaction {
                    failure: failure.clone(),
                    tokens_before,
                    model_id: model_id.clone(),
                });
                ClientEvent::CompactionFailed {
                    failure,
                    tokens_before,
                    model_id,
                }
            }
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
        let mut injections = self.injections.clone();
        injections.control_endpoint = self.runtime.control_endpoint();
        let deps = TurnDeps {
            config: self.config.clone(),
            injections,
            tools: &self.tools,
            registry: &self.registry,
        };
        let request = TurnRequest {
            pane_id: PRIMARY_PANE,
            prompt,
        };
        self.runtime.start_prepared_turn(request, prep, &deps);
        self.runtime.save_now_and_settle();
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

    async fn start_compaction(
        &mut self,
        focus: CompactionFocus,
        auto: bool,
    ) -> Result<(), ConnectionError> {
        let messages = self.runtime.conversation.context();
        if messages.len() <= crate::compaction::MIN_MESSAGES_FOR_COMPACTION {
            return Err(ConnectionError::TooShortToCompact);
        }
        self.spawn_compaction(messages, focus).await?;
        self.compaction_auto = auto;
        Ok(())
    }

    async fn retry_compaction(&mut self) -> Result<(), ConnectionError> {
        let failed = self
            .failed_compaction
            .take()
            .ok_or(ConnectionError::NothingToRecover)?;
        let messages = failed.failure.original_messages().to_vec();
        let focus = failed.failure.focus().clone();
        self.spawn_compaction(messages, focus).await
    }

    async fn fallback_compaction(&mut self) -> Result<ClientEvent, ConnectionError> {
        let failed = self
            .failed_compaction
            .take()
            .ok_or(ConnectionError::NothingToRecover)?;
        let applied = crate::compaction::fallback_applied_compaction(
            failed.failure,
            failed.tokens_before,
            failed.model_id,
            Some(&self.injections.lifecycle_hooks),
            self.injections.cwd.as_deref(),
        )
        .await;
        Ok(self.finish_compaction(CompactionTaskResult::Applied(applied)))
    }

    async fn spawn_compaction(
        &mut self,
        messages: Vec<Message>,
        focus: CompactionFocus,
    ) -> Result<(), ConnectionError> {
        if self.compaction.is_some() {
            return Err(ConnectionError::Compacting);
        }
        self.compaction_auto = false;
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
        if let Some(control) = &self.runtime.control {
            match event {
                AgentEvent::Error { .. } | AgentEvent::MaxTurnsReached { .. } => {
                    control.running_failed(true);
                }
                AgentEvent::AgentEnd => {
                    control.finish(mush_agent::ipc::control::Outcome::Completed);
                }
                _ => {}
            }
        }
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
                self.spawn_title();
                self.queued.push_back(ClientEvent::TurnEnded { cancelled });
            }
            _ => {}
        }
    }
}

#[async_trait::async_trait]
impl AgentConnection for InProcessConnection {
    async fn send(&mut self, command: ClientCommand) -> Result<Vec<ClientEvent>, ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        let before = self.queued.len();
        match command {
            ClientCommand::Prompt { text, images } => {
                if self.turn_active() {
                    self.steer(text, images).await;
                } else {
                    self.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images));
                    self.start_pending_prompt().await;
                }
            }
            ClientCommand::QueuePrompt { text, images } => {
                self.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images));
            }
            ClientCommand::Steer { text, images } => self.steer(text, images).await,
            ClientCommand::RetractSteer { text } => {
                if !self.retract_steer(&text).await {
                    tracing::warn!(%text, "no steering queue entry matched the retracted message");
                }
            }
            ClientCommand::Abort => {
                let events = self.abort().await;
                self.queued.extend(events);
            }
            ClientCommand::ConfirmTool { allow } => self.confirm(allow).await,
            ClientCommand::SetModel(id) => self.runtime.model_id = id,
            ClientCommand::SetThinking(level) => self.runtime.thinking_level = level,
            ClientCommand::SetRlmMaxDepth(depth) => self.runtime.set_rlm_max_depth(depth),
            ClientCommand::SetSystemPrompt(prompt) => self.runtime.system_prompt = prompt,
            ClientCommand::Session(command) => {
                let events = self.runtime.handle(command).await;
                self.sync_control_target();
                self.queued
                    .extend(events.into_iter().map(ClientEvent::Session));
            }
            ClientCommand::Compact { focus, auto } => self.start_compaction(focus, auto).await?,
            ClientCommand::CompactionRetry => self.retry_compaction().await?,
            ClientCommand::CompactionFallback => {
                let event = self.fallback_compaction().await?;
                self.queued.push_back(event);
            }
            ClientCommand::Shutdown => {
                self.abort().await;
                let id = self.runtime.session_id.clone();
                self.runtime.snapshot_kernel(&id).await;
                self.runtime.save_now_and_settle();
                self.closed = true;
            }
        }
        Ok(self.queued.drain(before..).collect())
    }

    async fn next_event(&mut self) -> Option<ClientEvent> {
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Some(event);
            }
            if self.closed {
                return None;
            }
            if let Some(event) = self.poll_background() {
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

    fn try_next_event(&mut self) -> Option<ClientEvent> {
        self.poll_background()
    }

    fn state(&self) -> ConnectionState {
        ConnectionState {
            session_id: self.runtime.session_id.clone(),
            title: self.runtime.title.clone(),
            model_id: self.runtime.model_id.clone(),
            thinking_level: self.runtime.thinking_level.clone(),
            context_tokens: self.runtime.context_tokens,
            context_len: self.runtime.conversation.context_len(),
            tree_len: self.runtime.conversation.tree().len(),
            rlm_depth: self.runtime.rlm_depth,
            rlm_max_depth: self.runtime.rlm_max_depth,
            turn_active: self.turn_active(),
            turn_model: self
                .runtime
                .turns
                .meta(PRIMARY_PANE)
                .map(|meta| meta.model.clone()),
            active_turns: self.runtime.turns.active_turns(),
            compacting: self.compaction.is_some(),
            pending_prompt: self.runtime.pending_prompt.is_some(),
            controlled: self.runtime.control.is_some(),
        }
    }

    async fn transcript(&mut self) -> Vec<Message> {
        self.runtime.conversation.context()
    }

    async fn history(&mut self) -> Vec<Message> {
        self.runtime.conversation.display_history()
    }

    async fn tree(&mut self) -> mush_session::tree::SessionTree {
        self.runtime.conversation.tree().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Api, AssistantMessage, StopReason};
    use mush_session::ConversationState;
    use std::sync::{Arc, Mutex};

    fn connection() -> (InProcessConnection, Arc<Mutex<Vec<Vec<Message>>>>) {
        crate::testing::echo_connection()
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

        let started = connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_turn(&mut connection).await;

        assert!(
            matches!(started[..], [ClientEvent::TurnStarted]),
            "{started:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::MessageEnd { .. },
                applied: Applied::Appended,
                ..
            }
        )));
        let transcript = connection.transcript().await;
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

        let events = connection
            .send(ClientCommand::Session(SessionCommand::New))
            .await
            .unwrap();

        assert!(matches!(
            events.first(),
            Some(ClientEvent::Session(SessionEvent::SessionIdChanged(_)))
        ));
        assert_ne!(connection.state().session_id, before);
        assert!(connection.transcript().await.is_empty());
        assert_eq!(connection.state().context_tokens, TokenCount::ZERO);
    }

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(mush_ai::types::UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[tokio::test]
    async fn a_compaction_runs_in_the_background_applies_and_saves() {
        let (mut connection, _) = connection();
        let messages: Vec<Message> = (0..30)
            .map(|i| user(&format!("turn {i}: {}", "x".repeat(400))))
            .collect();
        connection.runtime.conversation = ConversationState::from_messages(messages);
        let saved = Arc::new(Mutex::new(0usize));
        let counter = saved.clone();
        connection.runtime.saver = Some(Arc::new(move |_| {
            *counter.lock().unwrap() += 1;
        }));

        let started = connection
            .send(ClientCommand::Compact {
                focus: CompactionFocus::default(),
                auto: false,
            })
            .await
            .unwrap();
        assert!(matches!(started[..], [ClientEvent::Compacting]));
        assert!(connection.state().compacting);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(event) = connection.next_event().await {
                    break event;
                }
            }
        })
        .await
        .unwrap();

        assert!(matches!(event, ClientEvent::Compacted(_)), "{event:?}");
        assert!(!connection.state().compacting);
        assert!(connection.transcript().await.len() < 30);
        assert_eq!(*saved.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn abort_answers_the_open_tool_calls_and_clears_the_steering_queue() {
        use mush_ai::types::{AssistantContentPart, ToolCall, ToolCallId, ToolName};
        let (mut connection, _) = connection();
        let call = AssistantMessage::builder()
            .content(vec![AssistantContentPart::ToolCall(ToolCall {
                id: ToolCallId::from("call_1"),
                name: ToolName::from("ipython"),
                arguments: serde_json::json!({ "code": "1" }),
            })])
            .model("m".into())
            .provider(mush_ai::types::Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .stop_reason(StopReason::ToolUse)
            .timestamp_ms(Timestamp::zero())
            .build();
        connection.runtime.conversation =
            ConversationState::from_messages(vec![user("go"), Message::Assistant(call)]);
        connection
            .runtime
            .steering_queue
            .lock()
            .await
            .push(user("later"));

        let events = connection.send(ClientCommand::Abort).await.unwrap();

        assert!(matches!(
            &events[..],
            [ClientEvent::ToolsCancelled(ids)] if ids == &[ToolCallId::from("call_1")]
        ));
        let transcript = connection.transcript().await;
        assert!(matches!(
            transcript.last(),
            Some(Message::ToolResult(result))
                if result.tool_call_id.as_str() == "call_1" && result.outcome.is_error()
        ));
        assert!(connection.runtime.steering_queue.lock().await.is_empty());
    }

    #[tokio::test]
    async fn a_finished_turn_names_the_session_when_asked() {
        let (mut connection, _) = connection();
        connection.config.generate_title = true;
        connection
            .send(ClientCommand::Prompt {
                text: "please look at the parser".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;

        let title = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(ClientEvent::Session(SessionEvent::Title(title))) =
                    connection.next_event().await
                {
                    break title;
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(title.as_deref(), Some("hi back"));
        assert_eq!(connection.state().title.as_deref(), Some("hi back"));
    }

    #[tokio::test]
    async fn compacting_a_short_conversation_is_refused() {
        let (mut connection, _) = connection();

        let outcome = connection
            .send(ClientCommand::Compact {
                focus: CompactionFocus::default(),
                auto: false,
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
