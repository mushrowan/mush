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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone)]
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
}

pub struct SessionRuntime {
    pub session_id: SessionId,
    pub title: Option<String>,
    pub model_id: String,
    pub system_prompt: Option<String>,
    pub conversation: ConversationState,
    pub pending_prompt: Option<PendingPrompt>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    pub pending_compaction_inputs: Vec<(String, Vec<PendingImage>)>,
    pub title_generated: bool,
    pub control: Option<mush_agent::ipc::control::Control>,
    pub control_socket: Option<PathBuf>,
    pub turns: TurnEvents,
    pub saver: Option<crate::snapshot::SessionSaver>,
    pub kernel_state: Option<Arc<dyn mush_kernel::KernelState>>,
    last_save: std::time::Instant,
    dirty: bool,
}

pub const SESSION_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

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
            pending_compaction_inputs: Vec::new(),
            title_generated: false,
            control: None,
            control_socket: None,
            turns: TurnEvents::default(),
            saver: None,
            kernel_state: None,
            last_save: std::time::Instant::now() - SESSION_SAVE_DEBOUNCE,
            dirty: false,
        }
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
        crate::apply_agent_event(&mut self.conversation, event)
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

    pub async fn generate_title(
        &mut self,
        registry: &ApiRegistry,
        model: &mush_ai::types::Model,
        options: &mush_ai::types::StreamOptions,
    ) -> Option<String> {
        if !self.title_due() {
            return None;
        }
        let messages = self.conversation.context_prefix(4);
        let generated = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mush_session::title::generate_title(messages, registry, model, options),
        )
        .await
        .ok()
        .flatten();
        self.title_generated = true;
        if let Some(title) = generated {
            self.title = Some(title.clone());
            return Some(title);
        }
        None
    }

    #[must_use]
    pub fn control_endpoint(&self) -> Option<mush_agent::exec_context::ControlEndpoint> {
        Some(mush_agent::exec_context::ControlEndpoint {
            socket: self.control_socket.clone()?,
            target: self.control.as_ref()?.target(),
        })
    }

    pub fn drain_compaction_inputs(&mut self) -> Vec<(String, Vec<PendingImage>)> {
        std::mem::take(&mut self.pending_compaction_inputs)
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
