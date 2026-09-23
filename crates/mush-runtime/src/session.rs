use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use mush_agent::AgentEvent;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{ImageMimeType, Message, Model, PaneId, SessionId, ToolCallId};
use mush_session::ConversationState;
use tokio::sync::{Mutex, mpsc, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StreamGeneration(pub u64);

pub type TurnEvent = (PaneId, StreamGeneration, AgentEvent);

pub type TurnStream<'a> = Pin<Box<dyn futures::Stream<Item = TurnEvent> + Send + 'a>>;

const TURN_EVENT_BUFFER: usize = 1024;

pub type ConfirmReply = tokio::sync::oneshot::Sender<bool>;

pub struct ConfirmRequest {
    pub tool_call_id: ToolCallId,
    pub prompt: String,
    pub reply: ConfirmReply,
}

pub struct PendingConfirmation {
    pub tool_call_id: ToolCallId,
    pub reply: ConfirmReply,
}

pub struct StreamMeta {
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    pub confirm_req_rx: mpsc::Receiver<ConfirmRequest>,
    pub confirm_reply: Arc<Mutex<Option<PendingConfirmation>>>,
    pub model: Model,
    pub context_tokens: Arc<std::sync::atomic::AtomicU64>,
    pub cancel: tokio_util::sync::CancellationToken,
}

pub struct TurnEvents {
    tx: mpsc::Sender<TurnEvent>,
    rx: mpsc::Receiver<TurnEvent>,
    active: Arc<AtomicUsize>,
    active_tx: watch::Sender<usize>,
    active_rx: watch::Receiver<usize>,
    metas: std::collections::HashMap<PaneId, StreamMeta>,
    generations: std::collections::HashMap<PaneId, StreamGeneration>,
    next_generation: u64,
}

impl Default for TurnEvents {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(TURN_EVENT_BUFFER);
        let (active_tx, active_rx) = watch::channel(0);
        Self {
            tx,
            rx,
            active: Arc::new(AtomicUsize::new(0)),
            active_tx,
            active_rx,
            metas: std::collections::HashMap::new(),
            generations: std::collections::HashMap::new(),
            next_generation: 0,
        }
    }
}

impl TurnEvents {
    fn advance_generation(&mut self, pane_id: PaneId) -> StreamGeneration {
        let generation = StreamGeneration(self.next_generation);
        self.next_generation += 1;
        self.generations.insert(pane_id, generation);
        generation
    }

    pub fn register_active(&mut self, pane_id: PaneId, meta: StreamMeta) -> StreamGeneration {
        let generation = self.advance_generation(pane_id);
        self.metas.insert(pane_id, meta);
        generation
    }

    #[must_use]
    pub fn meta(&self, pane_id: PaneId) -> Option<&StreamMeta> {
        self.metas.get(&pane_id)
    }

    pub fn meta_mut(&mut self, pane_id: PaneId) -> Option<&mut StreamMeta> {
        self.metas.get_mut(&pane_id)
    }

    pub fn remove(&mut self, pane_id: PaneId) -> Option<StreamMeta> {
        self.metas.remove(&pane_id)
    }

    pub fn abort(&mut self, pane_id: PaneId) {
        if let Some(meta) = self.metas.get(&pane_id) {
            meta.cancel.cancel();
        }
        self.advance_generation(pane_id);
        self.metas.remove(&pane_id);
    }

    #[must_use]
    pub fn is_current(&self, pane_id: PaneId, generation: StreamGeneration) -> bool {
        self.generations.get(&pane_id) == Some(&generation)
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0 || !self.rx.is_empty()
    }

    #[must_use]
    pub fn active_turns(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn try_next(&mut self) -> Option<TurnEvent> {
        self.rx.try_recv().ok()
    }

    pub async fn next(&mut self) -> Option<TurnEvent> {
        if !self.is_active() {
            return None;
        }
        let mut idle = self.active_rx.clone();
        tokio::select! {
            event = self.rx.recv() => event,
            _ = idle.wait_for(|active| *active == 0) => self.rx.try_recv().ok(),
        }
    }

    pub fn spawn<B>(&self, registry: ApiRegistry, build: B)
    where
        B: for<'r> FnOnce(&'r ApiRegistry) -> TurnStream<'r> + Send + 'static,
    {
        let tx = self.tx.clone();
        let active = self.active.clone();
        let active_tx = self.active_tx.clone();
        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = active_tx.send(count);
        tokio::spawn(async move {
            let mut stream = build(&registry);
            while let Some(event) = stream.next().await {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            let count = active.fetch_sub(1, Ordering::SeqCst) - 1;
            let _ = active_tx.send(count);
        });
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingImage {
    pub data: Vec<u8>,
    pub mime_type: ImageMimeType,
    pub dimensions: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct PendingPrompt {
    pub text: String,
    pub images: Vec<PendingImage>,
    persisted: bool,
}

impl PendingPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            persisted: false,
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<PendingImage>) -> Self {
        Self {
            text: text.into(),
            images,
            persisted: false,
        }
    }

    pub fn is_persisted(&self) -> bool {
        self.persisted
    }

    pub fn mark_persisted(&mut self) {
        self.persisted = true;
    }

    pub fn merge(&mut self, text: &str, images: Vec<PendingImage>) {
        if self.persisted {
            *self = Self::with_images(text, images);
            return;
        }
        if !self.text.is_empty() && !text.is_empty() {
            self.text.push_str("\n\n");
        }
        self.text.push_str(text);
        self.images.extend(images);
    }
}

pub struct SessionRuntime {
    pub session_id: SessionId,
    pub title: Option<String>,
    pub model_id: String,
    pub system_prompt: Option<String>,
    pub conversation: ConversationState,
    pub pending_prompt: Option<PendingPrompt>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    pub title_generated: bool,
    pub control: Option<mush_agent::ipc::control::Control>,
    pub control_socket: Option<PathBuf>,
    pub turns: TurnEvents,
    pub saver: Option<crate::snapshot::SessionSaver>,
    pub kernel_state: Option<Arc<dyn mush_kernel::KernelState>>,
    pub rlm_depth: u32,
    pub rlm_max_depth: u32,
    pub thinking_level: mush_ai::types::ThinkingLevel,
    pub parent: Option<SessionId>,
    pub context_tokens: mush_ai::types::TokenCount,
    pub lease: Option<mush_session::SessionLease>,
    pub models: crate::models::ModelSelection,
    pub subagent_model: Option<String>,
    pub paused: Option<mush_session::Pause>,
    pub notes: Option<Arc<crate::notes::Notes>>,
    pub(crate) kernel_restore: Option<KernelRestore>,
    pub(crate) harness_digest_pending: bool,
    last_save: std::time::Instant,
    dirty: bool,
}

type KernelRestore = tokio::task::JoinHandle<Option<Result<mush_kernel::RestoreReport, String>>>;

pub const SESSION_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);
pub const CANCELLED_TOOL_MESSAGE: &str = "user cancelled tool";
pub const DEFAULT_RLM_MAX_DEPTH: u32 = 2;

impl SessionRuntime {
    #[must_use]
    pub fn new(conversation: ConversationState) -> Self {
        Self {
            session_id: SessionId::new(),
            title: None,
            model_id: String::new(),
            system_prompt: None,
            conversation,
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            title_generated: false,
            control: None,
            control_socket: None,
            turns: TurnEvents::default(),
            saver: None,
            kernel_state: None,
            rlm_depth: 0,
            rlm_max_depth: DEFAULT_RLM_MAX_DEPTH,
            thinking_level: mush_ai::types::ThinkingLevel::Off,
            parent: None,
            context_tokens: mush_ai::types::TokenCount::ZERO,
            lease: None,
            models: crate::models::ModelSelection::default(),
            subagent_model: None,
            paused: None,
            notes: None,
            kernel_restore: None,
            harness_digest_pending: true,
            last_save: std::time::Instant::now() - SESSION_SAVE_DEBOUNCE,
            dirty: false,
        }
    }

    pub fn begin_kernel_restore(&mut self) {
        self.kernel_restore = None;
        let Some(kernel) = self.kernel_state.clone() else {
            return;
        };
        let path = mush_session::SessionStore::kernel_state_path(&self.session_id);
        if !path.is_file() {
            return;
        }
        self.kernel_restore = Some(tokio::spawn(
            async move { kernel.restore_from(&path).await },
        ));
    }

    #[must_use]
    pub fn kernel_restore_pending(&self) -> bool {
        self.kernel_restore.is_some()
    }

    pub fn settle_kernel_restore(&mut self) -> Option<String> {
        use futures::FutureExt;
        if self.turn_active() {
            return None;
        }
        let task = self.kernel_restore.as_mut()?;
        if !task.is_finished() {
            return None;
        }
        let outcome = match task.now_or_never()? {
            Ok(outcome) => outcome,
            Err(error) => Some(Err(error.to_string())),
        };
        self.kernel_restore = None;
        let status = match &outcome {
            Some(Err(_)) => "python state could not be restored",
            Some(Ok(report)) if report.restored.is_empty() && !report.failed.is_empty() => {
                "python state could not be restored"
            }
            Some(Ok(_)) | None => "python state restored",
        };
        if let Some(message) = outcome.as_ref().and_then(crate::kernel_notice::restored) {
            self.conversation.append_message(message);
        }
        Some(status.to_string())
    }

    #[must_use]
    pub fn has_inbox_event(&self, event_id: &str) -> bool {
        mush_session::inbox::contains(&self.conversation, event_id)
    }

    pub fn accept_inbox_event(
        &mut self,
        event: &mush_session::inbox::Event,
    ) -> std::io::Result<mush_ai::types::EventMessage> {
        let message = mush_session::inbox::message(event)?;
        self.accept_event(message.clone());
        Ok(message)
    }

    pub fn accept_event(&mut self, event: mush_ai::types::EventMessage) {
        let mut prompt = PendingPrompt::text(event.text());
        prompt.mark_persisted();
        self.conversation.append_message(Message::Event(event));
        self.pending_prompt = Some(prompt);
    }

    pub fn apply_event(&mut self, event: &mush_agent::AgentEvent) -> crate::Applied {
        let applied = crate::apply_agent_event(&mut self.conversation, event);
        match event {
            mush_agent::AgentEvent::MessageEnd { message } => {
                self.context_tokens = message.usage.total_input_tokens();
            }
            mush_agent::AgentEvent::ContextCompacted { compaction, .. } => {
                let estimate = mush_session::compact::estimate_tokens(&compaction.messages);
                self.context_tokens = mush_ai::types::TokenCount::new(estimate as u64);
                self.harness_digest_pending = true;
            }
            _ => {}
        }
        applied
    }

    pub fn save_debounced(&mut self) {
        self.dirty = true;
        self.save_if_dirty();
    }

    pub fn save_if_dirty(&mut self) {
        if !self.dirty || self.last_save.elapsed() < SESSION_SAVE_DEBOUNCE {
            return;
        }
        self.save_now();
        self.last_save = std::time::Instant::now();
        self.dirty = false;
    }

    pub fn saved_just_now(&mut self) {
        self.last_save = std::time::Instant::now();
        self.dirty = false;
    }

    #[must_use]
    pub fn turn_active(&self) -> bool {
        self.turns.is_active()
    }

    #[must_use]
    pub fn title_due(&self) -> bool {
        !self.title_generated && self.title.is_none() && self.conversation.context_len() >= 2
    }

    pub fn record_cancelled_tools(&mut self) -> Vec<ToolCallId> {
        use mush_ai::types::{
            AssistantContentPart, TextContent, ToolResultContentPart, ToolResultMessage,
        };
        let context = self.conversation.context();
        let mut answered: Vec<ToolCallId> = Vec::new();
        let mut open = Vec::new();
        for message in context.iter().rev() {
            match message {
                Message::ToolResult(result) => answered.push(result.tool_call_id.clone()),
                Message::Assistant(assistant) => {
                    for part in &assistant.content {
                        if let AssistantContentPart::ToolCall(call) = part
                            && !answered.contains(&call.id)
                        {
                            open.push((call.id.clone(), call.name.clone()));
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
        let mut cancelled = Vec::new();
        for (tool_call_id, tool_name) in open {
            self.conversation
                .append_message(Message::ToolResult(ToolResultMessage {
                    tool_call_id: tool_call_id.clone(),
                    tool_name,
                    content: vec![ToolResultContentPart::Text(TextContent {
                        text: CANCELLED_TOOL_MESSAGE.into(),
                    })],
                    outcome: mush_ai::types::ToolOutcome::Error,
                    timestamp_ms: mush_ai::types::Timestamp::now(),
                    details: None,
                }));
            cancelled.push(tool_call_id);
        }
        cancelled
    }

    #[must_use]
    pub fn control_endpoint(&self) -> Option<mush_agent::exec_context::ControlEndpoint> {
        Some(mush_agent::exec_context::ControlEndpoint {
            socket: self.control_socket.clone()?,
            target: self.control.as_ref()?.target(),
        })
    }
}

impl Default for SessionRuntime {
    fn default() -> Self {
        Self::new(ConversationState::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_event_persists_the_event_and_queues_a_persisted_prompt() {
        let mut runtime = SessionRuntime::new(ConversationState::new());
        let event = mush_ai::types::EventMessage {
            content: "worker finished".into(),
            timestamp_ms: mush_ai::types::Timestamp::zero(),
            kind: mush_ai::types::EventKind::WorkerCompletion,
            receipts: Vec::new(),
        };

        runtime.accept_event(event);

        assert!(matches!(
            runtime.conversation.context().last(),
            Some(Message::Event(e)) if e.content == "worker finished"
        ));
        let prompt = runtime.pending_prompt.as_ref().expect("a queued prompt");
        assert!(prompt.is_persisted());
    }

    fn events(n: usize) -> TurnStream<'static> {
        Box::pin(futures::stream::iter((0..n).map(|_| {
            (PaneId::new(1), StreamGeneration(0), AgentEvent::AgentStart)
        })))
    }

    #[tokio::test]
    async fn events_flow_through_and_the_turn_settles() {
        let mut turns = TurnEvents::default();
        assert!(!turns.is_active());
        assert!(turns.next().await.is_none());

        turns.spawn(ApiRegistry::new(), |_| events(3));

        let mut seen = 0;
        while let Some(_event) = turns.next().await {
            seen += 1;
        }
        assert_eq!(seen, 3);
        assert!(!turns.is_active());
    }

    #[test]
    fn a_dirty_session_is_saved_once_the_debounce_window_passes() {
        let mut runtime = SessionRuntime::default();
        let saves = Arc::new(AtomicUsize::new(0));
        runtime.saver = Some({
            let saves = saves.clone();
            Arc::new(move |_| {
                saves.fetch_add(1, Ordering::SeqCst);
            })
        });

        runtime.save_debounced();
        runtime.save_debounced();
        runtime.last_save = std::time::Instant::now() - SESSION_SAVE_DEBOUNCE;
        runtime.save_if_dirty();

        assert_eq!(saves.load(Ordering::SeqCst), 2);
        runtime.save_if_dirty();
        assert_eq!(saves.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_turn_that_is_still_running_counts_as_active() {
        let mut turns = TurnEvents::default();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        turns.spawn(ApiRegistry::new(), move |_| {
            Box::pin(futures::stream::once(async move {
                let _ = release_rx.await;
                (PaneId::new(1), StreamGeneration(0), AgentEvent::AgentEnd)
            }))
        });
        tokio::task::yield_now().await;

        assert!(turns.is_active());
        assert!(turns.try_next().is_none());
        let _ = release_tx.send(());
        assert!(matches!(
            turns.next().await,
            Some((_, _, AgentEvent::AgentEnd))
        ));
        assert!(turns.next().await.is_none());
    }
}
