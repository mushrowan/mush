use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use mush_agent::AgentEvent;
use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    Cost, EventKind, EventMessage, EventOrigin, EventReceipt, ThinkingLevel, Timestamp, Usage,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::connection::{AgentConnection, ClientCommand, ClientEvent, InProcessConnection};
use crate::goal::Goal;
use crate::worker::WorkerConfig;

pub const MAX_CHILDREN: usize = 32;
pub const PROGRESS_NOTE_CHARS: usize = 512;
pub const PROGRESS_NOTE_INTERVAL: Duration = Duration::from_secs(10);
pub const MESSAGE_MAX_CHARS: usize = 16 * 1024;
pub const MAX_PENDING_MESSAGES: usize = 20;
pub const TASK_LABEL: &str = "[task from parent]";
const TICK: Duration = Duration::from_millis(50);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(5);

pub type ChildId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub id: Option<ChildId>,
    pub name: String,
    pub depth: u32,
}

impl Identity {
    #[must_use]
    pub fn root(name: impl Into<String>) -> Self {
        Self {
            id: None,
            name: name.into(),
            depth: 0,
        }
    }
}

pub struct ChildSpec {
    pub id: ChildId,
    pub name: String,
    pub prompt: String,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub depth: u32,
    pub parent: Identity,
}

pub struct ChildParts {
    pub config: WorkerConfig,
    pub tools: ToolRegistry,
    pub registry: ApiRegistry,
}

pub type ChildFactory = Arc<
    dyn Fn(ChildSpec, Arc<Family>) -> BoxFuture<'static, Result<ChildParts, String>> + Send + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    Starting,
    Running,
    Idle,
    Failed,
    Cancelled,
    Interrupted,
    Deleted,
}

impl ChildStatus {
    #[must_use]
    pub fn settled(self) -> bool {
        !matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildSummary {
    pub id: ChildId,
    pub name: String,
    pub parent: Option<ChildId>,
    pub depth: u32,
    pub session_id: String,
    pub model_id: String,
    pub status: ChildStatus,
    pub spawned_at: Timestamp,
    pub updated_at: Timestamp,
    pub answer: Option<String>,
    pub error: Option<String>,
    pub progress_note: Option<String>,
    pub usage: Usage,
    pub cost: Cost,
    pub turns: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildHandle {
    pub rlm_child_id: ChildId,
    pub name: String,
    pub session_id: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Receipt {
    Delivered,
    Queued,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    Auto,
    Steer,
    FollowUp,
}

pub struct Delivery {
    pub event: EventMessage,
    pub mode: DeliveryMode,
    receipt: Option<oneshot::Sender<Receipt>>,
}

impl Delivery {
    pub fn ack(&mut self, receipt: Receipt) {
        if let Some(sender) = self.receipt.take() {
            let _ = sender.send(receipt);
        }
    }
}

pub type AgentInbox = mpsc::UnboundedReceiver<Delivery>;

struct Node {
    summary: ChildSummary,
    inbox: Option<mpsc::UnboundedSender<Delivery>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    last_note_at: Option<Instant>,
}

struct Inner {
    root: Identity,
    root_session_id: String,
    root_inbox: Option<mpsc::UnboundedSender<Delivery>>,
    root_note: Option<String>,
    children: BTreeMap<ChildId, Node>,
}

pub struct Family {
    inner: Mutex<Inner>,
    factory: Option<ChildFactory>,
    registry_path: Mutex<Option<PathBuf>>,
    max_depth: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    children: Vec<ChildSummary>,
}

impl Family {
    #[must_use]
    pub fn new(
        root: Identity,
        root_session_id: String,
        max_depth: u32,
        factory: Option<ChildFactory>,
        registry_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let mut children = BTreeMap::new();
        if let Some(path) = &registry_path
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(registry) = serde_json::from_slice::<Registry>(&bytes)
        {
            for mut summary in registry.children {
                if !summary.status.settled() {
                    summary.status = ChildStatus::Interrupted;
                }
                children.insert(
                    summary.id.clone(),
                    Node {
                        summary,
                        inbox: None,
                        shutdown: None,
                        task: None,
                        last_note_at: None,
                    },
                );
            }
        }
        Arc::new(Self {
            inner: Mutex::new(Inner {
                root,
                root_session_id,
                root_inbox: None,
                root_note: None,
                children,
            }),
            factory,
            registry_path: Mutex::new(registry_path),
            max_depth,
        })
    }

    #[must_use]
    pub fn max_depth(&self) -> u32 {
        self.max_depth
    }

    pub fn root_inbox(&self) -> AgentInbox {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().root_inbox = Some(tx);
        rx
    }

    pub fn rebind(&self, session_id: String, registry_path: Option<PathBuf>) {
        {
            let mut inner = self.lock();
            inner.root_session_id = session_id;
        }
        *self
            .registry_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = registry_path;
        self.persist();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn persist(&self) {
        let path = self
            .registry_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(path) = path else {
            return;
        };
        let registry = Registry {
            children: self
                .lock()
                .children
                .values()
                .map(|node| node.summary.clone())
                .collect(),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&registry) {
            let _ = mush_ai::private_io::write_private(&path, bytes);
        }
    }

    #[must_use]
    pub fn children_of(&self, caller: &Identity) -> Vec<ChildSummary> {
        self.lock()
            .children
            .values()
            .filter(|node| node.summary.parent == caller.id)
            .map(|node| node.summary.clone())
            .collect()
    }

    #[must_use]
    pub fn all(&self) -> Vec<ChildSummary> {
        self.lock()
            .children
            .values()
            .map(|node| node.summary.clone())
            .collect()
    }

    fn resolve(&self, caller: &Identity, target: &str) -> Result<ChildId, String> {
        let inner = self.lock();
        let mut matches = inner.children.values().filter(|node| {
            node.summary.parent == caller.id
                && (node.summary.id == target || node.summary.name == target)
        });
        let found = matches
            .next()
            .ok_or_else(|| format!("no child of yours is called {target}"))?;
        if matches.next().is_some() {
            return Err(format!("{target} names more than one child, use its id"));
        }
        Ok(found.summary.id.clone())
    }

    pub async fn spawn(
        &self,
        caller: &Identity,
        prompt: String,
        name: String,
        model: Option<String>,
        thinking: Option<ThinkingLevel>,
        this: &Arc<Self>,
    ) -> Result<ChildHandle, String> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("name is required".into());
        }
        if prompt.trim().is_empty() {
            return Err("prompt is required".into());
        }
        if caller.depth + 1 > self.max_depth {
            return Err(format!(
                "recursion depth {} is the limit, this agent cannot spawn children",
                self.max_depth
            ));
        }
        let factory = self
            .factory
            .clone()
            .ok_or_else(|| "subagents are not available in this session".to_string())?;
        let id = format!("sub-{}", crate::schedule::short_id());
        {
            let inner = self.lock();
            let live = inner
                .children
                .values()
                .filter(|node| !node.summary.status.settled())
                .count();
            if live >= MAX_CHILDREN {
                return Err(format!("{MAX_CHILDREN} children are already running"));
            }
            if inner.children.values().any(|node| {
                node.summary.parent == caller.id
                    && node.summary.name == name
                    && node.summary.status != ChildStatus::Deleted
            }) {
                return Err(format!("you already have a child called {name}"));
            }
        }
        let spec = ChildSpec {
            id: id.clone(),
            name: name.clone(),
            prompt: prompt.clone(),
            model,
            thinking,
            depth: caller.depth + 1,
            parent: caller.clone(),
        };
        let parts = (factory)(spec, Arc::clone(this)).await?;
        let session_id = parts.config.session_id.to_string();
        let model_id = parts.config.turn.default_model.id.to_string();
        let now = Timestamp::now();
        let summary = ChildSummary {
            id: id.clone(),
            name: name.clone(),
            parent: caller.id.clone(),
            depth: caller.depth + 1,
            session_id: session_id.clone(),
            model_id: model_id.clone(),
            status: ChildStatus::Starting,
            spawned_at: now,
            updated_at: now,
            answer: None,
            error: None,
            progress_note: None,
            usage: Usage::default(),
            cost: Cost::default(),
            turns: 0,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut connection = parts.config.connect(&parts.tools, &parts.registry);
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        connection.attach_agent_inbox(inbox_rx);
        let task = tokio::spawn(drive_child(
            Arc::clone(this),
            id.clone(),
            connection,
            shutdown_rx,
            format!("{TASK_LABEL}\n\n{prompt}"),
        ));
        self.lock().children.insert(
            id.clone(),
            Node {
                summary,
                inbox: Some(inbox_tx),
                shutdown: Some(shutdown_tx),
                task: Some(task),
                last_note_at: None,
            },
        );
        self.persist();
        Ok(ChildHandle {
            rlm_child_id: id,
            name,
            session_id,
            model: model_id,
        })
    }

    fn update(&self, id: &str, apply: impl FnOnce(&mut ChildSummary)) {
        {
            let mut inner = self.lock();
            let Some(node) = inner.children.get_mut(id) else {
                return;
            };
            apply(&mut node.summary);
            node.summary.updated_at = Timestamp::now();
        }
        self.persist();
    }

    fn record(&self, id: &str, event: &ClientEvent, model: Option<&mush_ai::types::Model>) {
        match event {
            ClientEvent::TurnStarted => self.update(id, |child| {
                child.status = ChildStatus::Running;
                child.turns += 1;
            }),
            ClientEvent::Agent {
                event: AgentEvent::MessageEnd { message },
                ..
            } => {
                let text = message.text();
                let usage = message.usage;
                let cost = model.map(|model| mush_ai::models::calculate_cost(model, &usage));
                self.update(id, |child| {
                    if !text.trim().is_empty() {
                        child.answer = Some(text);
                    }
                    child.usage.input_tokens += usage.input_tokens;
                    child.usage.output_tokens += usage.output_tokens;
                    child.usage.cache_read_tokens += usage.cache_read_tokens;
                    child.usage.cache_write_tokens += usage.cache_write_tokens;
                    if let Some(cost) = cost {
                        child.cost.input += cost.input;
                        child.cost.output += cost.output;
                        child.cost.cache_read += cost.cache_read;
                        child.cost.cache_write += cost.cache_write;
                    }
                });
            }
            ClientEvent::Agent {
                event: AgentEvent::Error { error },
                ..
            } => self.update(id, |child| {
                child.status = ChildStatus::Failed;
                child.error = Some(error.clone());
            }),
            ClientEvent::TurnEnded { cancelled } => self.update(id, |child| {
                if child.status == ChildStatus::Running {
                    child.status = if *cancelled {
                        ChildStatus::Cancelled
                    } else {
                        ChildStatus::Idle
                    };
                }
            }),
            _ => {}
        }
    }

    pub async fn collect(
        &self,
        caller: &Identity,
        targets: Vec<String>,
        timeout: Duration,
    ) -> Result<Vec<ChildSummary>, String> {
        let ids: Vec<ChildId> = if targets.is_empty() {
            self.children_of(caller)
                .into_iter()
                .map(|child| child.id)
                .collect()
        } else {
            targets
                .iter()
                .map(|target| self.resolve(caller, target))
                .collect::<Result<_, _>>()?
        };
        let deadline = Instant::now() + timeout;
        loop {
            let summaries: Vec<ChildSummary> = {
                let inner = self.lock();
                ids.iter()
                    .filter_map(|id| inner.children.get(id))
                    .map(|node| node.summary.clone())
                    .collect()
            };
            let pending = summaries.iter().any(|child| !child.status.settled());
            if !pending || Instant::now() >= deadline {
                return Ok(summaries);
            }
            tokio::time::sleep(TICK).await;
        }
    }

    pub async fn delete(&self, caller: &Identity, target: &str) -> Result<ChildSummary, String> {
        let id = self.resolve(caller, target)?;
        let shutdown = self
            .lock()
            .children
            .get_mut(&id)
            .and_then(|node| node.shutdown.take());
        if let Some(shutdown) = shutdown {
            let _ = shutdown.send(());
        }
        self.update(&id, |child| {
            if !child.status.settled() {
                child.status = ChildStatus::Cancelled;
            }
            child.status = ChildStatus::Deleted;
        });
        let task = self
            .lock()
            .children
            .get_mut(&id)
            .and_then(|node| node.task.take());
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
        self.lock()
            .children
            .get(&id)
            .map(|node| node.summary.clone())
            .ok_or_else(|| format!("{id} vanished"))
    }

    pub fn take_root_note(&self) -> Option<String> {
        self.lock().root_note.take()
    }

    pub fn progress_note(&self, caller: &Identity, text: &str) -> Result<(), String> {
        let text: String = text.trim().chars().take(PROGRESS_NOTE_CHARS).collect();
        if text.is_empty() {
            return Err("a progress note needs some text".into());
        }
        let Some(id) = caller.id.as_deref() else {
            self.lock().root_note = Some(text);
            return Ok(());
        };
        {
            let mut inner = self.lock();
            let node = inner
                .children
                .get_mut(id)
                .ok_or_else(|| format!("{id} is not in the family"))?;
            if node
                .last_note_at
                .is_some_and(|at| at.elapsed() < PROGRESS_NOTE_INTERVAL)
            {
                return Err(format!(
                    "progress notes are limited to one every {} seconds",
                    PROGRESS_NOTE_INTERVAL.as_secs()
                ));
            }
            node.last_note_at = Some(Instant::now());
            node.summary.progress_note = Some(text);
            node.summary.updated_at = Timestamp::now();
        }
        self.persist();
        Ok(())
    }

    fn sender_label(&self, caller: &Identity, relationship: &str) -> String {
        format!("{relationship}:{}", caller.name)
    }

    pub async fn send_message(
        &self,
        caller: &Identity,
        receiver_role: &str,
        receiver_name: Option<&str>,
        text: &str,
        mode: DeliveryMode,
    ) -> Result<Vec<(String, Receipt)>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("message is required".into());
        }
        if text.chars().count() > MESSAGE_MAX_CHARS {
            return Err(format!("message is over {MESSAGE_MAX_CHARS} characters"));
        }
        let targets: Vec<(Option<ChildId>, String, String)> = match receiver_role {
            "parent" => {
                let id = caller
                    .id
                    .as_deref()
                    .ok_or_else(|| "the root agent has no parent".to_string())?;
                if receiver_name.is_some() {
                    return Err("receiver_name is not used for parent messages".into());
                }
                let inner = self.lock();
                let node = inner
                    .children
                    .get(id)
                    .ok_or_else(|| format!("{id} is not in the family"))?;
                let parent = node.summary.parent.clone();
                let parent_name = match &parent {
                    Some(parent) => inner
                        .children
                        .get(parent)
                        .map(|node| node.summary.name.clone())
                        .unwrap_or_default(),
                    None => inner.root.name.clone(),
                };
                vec![(parent, parent_name, self.sender_label(caller, "child"))]
            }
            "child" => {
                let name = receiver_name
                    .ok_or_else(|| "receiver_name is required for child messages".to_string())?;
                let id = self.resolve(caller, name)?;
                let child_name = self
                    .lock()
                    .children
                    .get(&id)
                    .map(|node| node.summary.name.clone())
                    .unwrap_or_default();
                vec![(Some(id), child_name, self.sender_label(caller, "parent"))]
            }
            "sibling" => {
                let name = receiver_name
                    .ok_or_else(|| "receiver_name is required for sibling messages".to_string())?;
                let my_id = caller
                    .id
                    .as_deref()
                    .ok_or_else(|| "the root agent has no siblings".to_string())?;
                let inner = self.lock();
                let my_parent = inner
                    .children
                    .get(my_id)
                    .map(|node| node.summary.parent.clone())
                    .ok_or_else(|| format!("{my_id} is not in the family"))?;
                let sibling = inner
                    .children
                    .values()
                    .find(|node| {
                        node.summary.parent == my_parent
                            && node.summary.id != my_id
                            && (node.summary.id == name || node.summary.name == name)
                            && node.summary.status != ChildStatus::Deleted
                    })
                    .ok_or_else(|| format!("no sibling of yours is called {name}"))?;
                vec![(
                    Some(sibling.summary.id.clone()),
                    sibling.summary.name.clone(),
                    self.sender_label(caller, "sibling"),
                )]
            }
            "all" => {
                let inner = self.lock();
                let mut targets = Vec::new();
                if let Some(my_id) = caller.id.as_deref() {
                    let my_parent = inner
                        .children
                        .get(my_id)
                        .map(|node| node.summary.parent.clone())
                        .ok_or_else(|| format!("{my_id} is not in the family"))?;
                    let parent_name = match &my_parent {
                        Some(parent) => inner
                            .children
                            .get(parent)
                            .map(|node| node.summary.name.clone())
                            .unwrap_or_default(),
                        None => inner.root.name.clone(),
                    };
                    targets.push((
                        my_parent.clone(),
                        parent_name,
                        self.sender_label(caller, "child"),
                    ));
                    for node in inner.children.values() {
                        if node.summary.parent == my_parent
                            && node.summary.id != my_id
                            && !node.summary.status.settled()
                        {
                            targets.push((
                                Some(node.summary.id.clone()),
                                node.summary.name.clone(),
                                self.sender_label(caller, "sibling"),
                            ));
                        }
                    }
                }
                for node in inner.children.values() {
                    if node.summary.parent == caller.id && !node.summary.status.settled() {
                        targets.push((
                            Some(node.summary.id.clone()),
                            node.summary.name.clone(),
                            self.sender_label(caller, "parent"),
                        ));
                    }
                }
                targets
            }
            other => {
                return Err(format!(
                    "receiver_role must be parent, child, sibling or all, not {other}"
                ));
            }
        };
        if targets.is_empty() {
            return Err("nobody to send to".into());
        }
        let mut receipts = Vec::new();
        for (target, target_name, sender) in targets {
            let receipt = self
                .deliver(caller, target.as_deref(), &sender, text, mode)
                .await?;
            if receipt == Receipt::Rejected {
                return Err(format!(
                    "{target_name} has {MAX_PENDING_MESSAGES} messages waiting already"
                ));
            }
            receipts.push((target_name, receipt));
        }
        Ok(receipts)
    }

    async fn deliver(
        &self,
        caller: &Identity,
        target: Option<&str>,
        sender: &str,
        text: &str,
        mode: DeliveryMode,
    ) -> Result<Receipt, String> {
        let timestamp_ms = Timestamp::now();
        let (sender_session, receiver_session) = {
            let inner = self.lock();
            let session_of = |id: Option<&str>| match id {
                Some(id) => inner
                    .children
                    .get(id)
                    .map(|node| node.summary.session_id.clone())
                    .unwrap_or_default(),
                None => inner.root_session_id.clone(),
            };
            (session_of(caller.id.as_deref()), session_of(target))
        };
        let _ = receiver_session;
        let event = EventMessage {
            content: format!("[agent-message from {sender}]\n\n{text}"),
            timestamp_ms,
            kind: EventKind::Message,
            receipts: vec![EventReceipt {
                id: format!("agent-message-{}", mush_ai::types::SessionId::new()).into(),
                timestamp_ms,
                kind: EventKind::Message,
                origin: EventOrigin::Agent {
                    session_id: sender_session,
                    name: caller.name.clone(),
                    relationship: sender
                        .split_once(':')
                        .map(|(relationship, _)| relationship.to_string())
                        .unwrap_or_default(),
                },
            }],
        };
        let (receipt_tx, receipt_rx) = oneshot::channel();
        let delivery = Delivery {
            event,
            mode,
            receipt: Some(receipt_tx),
        };
        match target {
            Some(id) => {
                let inbox = self
                    .lock()
                    .children
                    .get(id)
                    .filter(|node| node.summary.status != ChildStatus::Deleted)
                    .and_then(|node| node.inbox.clone())
                    .ok_or_else(|| format!("{id} is no longer running"))?;
                inbox
                    .send(delivery)
                    .map_err(|_| format!("{id} is no longer running"))?;
            }
            None => {
                let inbox = self
                    .lock()
                    .root_inbox
                    .clone()
                    .ok_or_else(|| "the parent is not accepting messages".to_string())?;
                inbox
                    .send(delivery)
                    .map_err(|_| "the parent is not accepting messages".to_string())?;
            }
        }
        tokio::time::timeout(RECEIPT_TIMEOUT, receipt_rx)
            .await
            .map_err(|_| "the receiver did not acknowledge in time".to_string())?
            .map_err(|_| "the receiver went away".to_string())
    }

    pub async fn shutdown(&self) {
        let tasks: Vec<_> = {
            let mut inner = self.lock();
            inner
                .children
                .values_mut()
                .filter_map(|node| {
                    if let Some(shutdown) = node.shutdown.take() {
                        let _ = shutdown.send(());
                    }
                    if !node.summary.status.settled() {
                        node.summary.status = ChildStatus::Cancelled;
                    }
                    node.task.take()
                })
                .collect()
        };
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
        self.persist();
    }
}

async fn drive_child(
    family: Arc<Family>,
    id: ChildId,
    mut connection: InProcessConnection,
    shutdown: oneshot::Receiver<()>,
    first_prompt: String,
) {
    let model = connection.default_model();
    match connection
        .send(ClientCommand::Prompt {
            text: first_prompt,
            images: Vec::new(),
        })
        .await
    {
        Ok(events) => {
            for event in &events {
                family.record(&id, event, Some(&model));
            }
        }
        Err(error) => {
            family.update(&id, |child| {
                child.status = ChildStatus::Failed;
                child.error = Some(error.to_string());
            });
            return;
        }
    }
    let mut shutdown = shutdown;
    let mut closing = false;
    loop {
        connection.start_pending().await;
        tokio::select! {
            _ = &mut shutdown, if !closing => {
                closing = true;
                let _ = connection.send(ClientCommand::Shutdown).await;
            }
            event = connection.next_event() => {
                let Some(event) = event else {
                    break;
                };
                family.record(&id, &event, Some(&model));
            }
            () = tokio::time::sleep(TICK) => {}
        }
    }
    family.update(&id, |child| {
        if !child.status.settled() {
            child.status = ChildStatus::Cancelled;
        }
    });
}

pub struct RlmHandler {
    family: Arc<Family>,
    identity: Identity,
    goal: Option<Arc<crate::goal::GoalStore>>,
    scheduler: Option<Arc<crate::schedule::Scheduler>>,
    notes: Option<Arc<crate::notes::Notes>>,
}

impl RlmHandler {
    #[must_use]
    pub fn new(family: Arc<Family>, identity: Identity) -> Self {
        Self {
            family,
            identity,
            goal: None,
            scheduler: None,
            notes: None,
        }
    }

    #[must_use]
    pub fn with_notes(mut self, notes: Arc<crate::notes::Notes>) -> Self {
        self.notes = Some(notes);
        self
    }

    #[must_use]
    pub fn with_goal(mut self, goal: Arc<crate::goal::GoalStore>) -> Self {
        self.goal = Some(goal);
        self
    }

    #[must_use]
    pub fn with_scheduler(mut self, scheduler: Arc<crate::schedule::Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    fn rlm_heartbeats(
        &self,
    ) -> Result<(&Arc<crate::schedule::Scheduler>, Vec<crate::schedule::Job>), mush_kernel::HostError>
    {
        let scheduler = self
            .scheduler
            .as_ref()
            .ok_or_else(|| mush_kernel::HostError("heartbeats are not available here".into()))?;
        let jobs = scheduler
            .jobs()
            .into_iter()
            .filter(|job| job.source == crate::schedule::JobSource::RlmHeartbeat)
            .collect();
        Ok((scheduler, jobs))
    }

    fn goal_reply(&self) -> serde_json::Value {
        let goal = self.goal.as_ref().and_then(|goal| goal.get());
        serde_json::json!({
            "goal": goal,
            "remaining_tokens": goal.as_ref().and_then(Goal::remaining_tokens),
        })
    }

    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
}

fn summary_json(child: &ChildSummary) -> serde_json::Value {
    serde_json::to_value(child).unwrap_or(serde_json::Value::Null)
}

#[async_trait::async_trait]
impl mush_kernel::HostRequestHandler for RlmHandler {
    async fn handle(
        &self,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, mush_kernel::HostError> {
        let str_field = |key: &str| -> Option<String> {
            payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        match kind {
            "rlm.spawn" => {
                let prompt = str_field("prompt")
                    .ok_or_else(|| mush_kernel::HostError("prompt is required".into()))?;
                let name = str_field("name")
                    .ok_or_else(|| mush_kernel::HostError("name is required".into()))?;
                let thinking =
                    str_field("thinking").map(|level| ThinkingLevel::from_effort_str(&level));
                let handle = self
                    .family
                    .spawn(
                        &self.identity,
                        prompt,
                        name,
                        str_field("model"),
                        thinking,
                        &self.family,
                    )
                    .await
                    .map_err(mush_kernel::HostError)?;
                Ok(serde_json::to_value(handle).unwrap_or(serde_json::Value::Null))
            }
            "rlm.list_subagents" => Ok(serde_json::Value::Array(
                self.family
                    .children_of(&self.identity)
                    .iter()
                    .map(summary_json)
                    .collect(),
            )),
            "rlm.collect" => {
                let targets: Vec<String> = payload
                    .get("targets")
                    .and_then(serde_json::Value::as_array)
                    .map(|targets| {
                        targets
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let timeout_ms = payload
                    .get("timeout_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let children = self
                    .family
                    .collect(&self.identity, targets, Duration::from_millis(timeout_ms))
                    .await
                    .map_err(mush_kernel::HostError)?;
                Ok(serde_json::Value::Array(
                    children.iter().map(summary_json).collect(),
                ))
            }
            "rlm.delete_subagent" => {
                let target = str_field("target")
                    .ok_or_else(|| mush_kernel::HostError("target is required".into()))?;
                let child = self
                    .family
                    .delete(&self.identity, &target)
                    .await
                    .map_err(mush_kernel::HostError)?;
                Ok(summary_json(&child))
            }
            "rlm.progress_note" => {
                let text = str_field("text")
                    .ok_or_else(|| mush_kernel::HostError("text is required".into()))?;
                self.family
                    .progress_note(&self.identity, &text)
                    .map_err(mush_kernel::HostError)?;
                Ok(serde_json::json!({ "ok": true }))
            }
            "agent_message.send" => {
                let message = str_field("message")
                    .ok_or_else(|| mush_kernel::HostError("message is required".into()))?;
                let role = str_field("receiver_role").unwrap_or_else(|| "parent".into());
                let mode = match str_field("mode").as_deref() {
                    None | Some("auto") => DeliveryMode::Auto,
                    Some("steer") => DeliveryMode::Steer,
                    Some("follow_up") => DeliveryMode::FollowUp,
                    Some(other) => {
                        return Err(mush_kernel::HostError(format!(
                            "mode must be auto, steer or follow_up, not {other}"
                        )));
                    }
                };
                let receipts = self
                    .family
                    .send_message(
                        &self.identity,
                        &role,
                        str_field("receiver_name").as_deref(),
                        &message,
                        mode,
                    )
                    .await
                    .map_err(mush_kernel::HostError)?;
                Ok(serde_json::json!({
                    "receipts": receipts
                        .iter()
                        .map(|(name, receipt)| serde_json::json!({ "receiver": name, "status": receipt }))
                        .collect::<Vec<_>>()
                }))
            }
            "rlm.note" | "rlm.notes" | "rlm.clear_notes" => {
                let notes = self
                    .notes
                    .as_ref()
                    .ok_or_else(|| mush_kernel::HostError("notes are not available here".into()))?;
                let text = match kind {
                    "rlm.note" => {
                        let text = str_field("text")
                            .ok_or_else(|| mush_kernel::HostError("text is required".into()))?;
                        notes.append(&text).map_err(mush_kernel::HostError)?
                    }
                    "rlm.clear_notes" => {
                        notes.replace("");
                        String::new()
                    }
                    _ => notes.read(),
                };
                Ok(serde_json::json!({ "notes": text }))
            }
            "rlm_heartbeat.list" => {
                let (_, jobs) = self.rlm_heartbeats()?;
                Ok(serde_json::to_value(jobs).unwrap_or(serde_json::Value::Null))
            }
            "rlm_heartbeat.create" => {
                let (scheduler, _) = self.rlm_heartbeats()?;
                let prompt = str_field("prompt")
                    .ok_or_else(|| mush_kernel::HostError("prompt is required".into()))?;
                let expression = str_field("interval").unwrap_or_else(|| "every 5m".into());
                let expression = if expression.trim_start().starts_with("every ") {
                    expression
                } else {
                    format!("every {expression}")
                };
                let jobs = scheduler
                    .apply(crate::schedule::ScheduleCommand::Add {
                        expression,
                        prompt,
                        label: str_field("label"),
                        source: crate::schedule::JobSource::RlmHeartbeat,
                    })
                    .map_err(mush_kernel::HostError)?;
                let created = jobs
                    .iter()
                    .rev()
                    .find(|job| job.source == crate::schedule::JobSource::RlmHeartbeat)
                    .cloned();
                Ok(serde_json::to_value(created).unwrap_or(serde_json::Value::Null))
            }
            "rlm_heartbeat.update" => {
                let (scheduler, jobs) = self.rlm_heartbeats()?;
                let id = str_field("id")
                    .ok_or_else(|| mush_kernel::HostError("id is required".into()))?;
                if !jobs.iter().any(|job| job.id == id) {
                    return Err(mush_kernel::HostError(format!("no rlm heartbeat {id}")));
                }
                let command = match str_field("status").as_deref() {
                    Some("pause") | Some("paused") => {
                        crate::schedule::ScheduleCommand::Pause(id.clone())
                    }
                    Some("resume") | Some("active") => {
                        crate::schedule::ScheduleCommand::Resume(id.clone())
                    }
                    Some("cancel") | Some("cancelled") | Some("clear") => {
                        crate::schedule::ScheduleCommand::Cancel(id.clone())
                    }
                    other => {
                        return Err(mush_kernel::HostError(format!(
                            "status must be pause, resume or cancel, not {other:?}"
                        )));
                    }
                };
                let jobs = scheduler.apply(command).map_err(mush_kernel::HostError)?;
                let updated = jobs.into_iter().find(|job| job.id == id);
                Ok(serde_json::to_value(updated).unwrap_or(serde_json::Value::Null))
            }
            "goal.get" => Ok(self.goal_reply()),
            "goal.create" | "goal.complete" | "goal.pause" | "goal.resume" | "goal.clear" => {
                let store = self
                    .goal
                    .as_ref()
                    .ok_or_else(|| mush_kernel::HostError("goals are not available here".into()))?;
                let command = match kind {
                    "goal.create" => crate::goal::GoalCommand::Create {
                        objective: str_field("objective").ok_or_else(|| {
                            mush_kernel::HostError("objective is required".into())
                        })?,
                        token_budget: payload
                            .get("token_budget")
                            .and_then(serde_json::Value::as_u64),
                    },
                    "goal.complete" => crate::goal::GoalCommand::Complete {
                        summary: str_field("summary"),
                    },
                    "goal.pause" => crate::goal::GoalCommand::Pause,
                    "goal.resume" => crate::goal::GoalCommand::Resume,
                    _ => crate::goal::GoalCommand::Clear,
                };
                store.apply(command).map_err(mush_kernel::HostError)?;
                Ok(self.goal_reply())
            }
            other => Err(mush_kernel::HostError(format!(
                "unknown host request {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Echo, Requests, echo_config, echo_model};
    use mush_ai::types::Message;
    use std::sync::Mutex as StdMutex;

    fn echo_factory(requests: Requests, reply: &str) -> ChildFactory {
        let reply = reply.to_string();
        Arc::new(move |spec: ChildSpec, _family: Arc<Family>| {
            let requests = Arc::clone(&requests);
            let reply = reply.clone();
            Box::pin(async move {
                let mut registry = ApiRegistry::new();
                registry.register(Box::new(Echo { requests, reply }));
                let model = echo_model();
                let mut config = WorkerConfig::new(echo_config(model));
                config.title = Some(spec.name.clone());
                config.rlm_depth = spec.depth;
                config.system_prompt = Some(format!("child {} of {}", spec.name, spec.parent.name));
                Ok(ChildParts {
                    config,
                    tools: ToolRegistry::new(),
                    registry,
                })
            })
        })
    }

    fn family(requests: Requests, max_depth: u32, registry_path: Option<PathBuf>) -> Arc<Family> {
        Family::new(
            Identity::root("root"),
            "root-session".into(),
            max_depth,
            Some(echo_factory(requests, "child says hi")),
            registry_path,
        )
    }

    async fn settle(family: &Family, caller: &Identity) -> Vec<ChildSummary> {
        family
            .collect(caller, Vec::new(), Duration::from_secs(5))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_spawned_child_runs_its_task_and_collect_reports_the_answer() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 2, None);
        let root = Identity::root("root");

        let handle = family
            .spawn(
                &root,
                "review the auth flow".into(),
                "auth".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap();
        assert_eq!(handle.name, "auth");
        assert!(handle.rlm_child_id.starts_with("sub-"));

        let children = settle(&family, &root).await;
        assert_eq!(children.len(), 1);
        let child = &children[0];
        assert_eq!(child.status, ChildStatus::Idle);
        assert_eq!(child.answer.as_deref(), Some("child says hi"));
        assert_eq!(child.turns, 1);
        assert_eq!(child.depth, 1);
        let asked = requests.lock().unwrap();
        assert!(matches!(
            &asked[0][0],
            Message::User(user) if user.text().starts_with("[task from parent]\n\nreview the auth flow")
        ));
    }

    #[tokio::test]
    async fn depth_and_duplicate_names_are_refused() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 1, None);
        let root = Identity::root("root");
        family
            .spawn(&root, "one".into(), "worker".into(), None, None, &family)
            .await
            .unwrap();

        let duplicate = family
            .spawn(&root, "two".into(), "worker".into(), None, None, &family)
            .await
            .unwrap_err();
        assert!(duplicate.contains("already have a child called worker"));

        let child = Identity {
            id: Some(family.children_of(&root)[0].id.clone()),
            name: "worker".into(),
            depth: 1,
        };
        let too_deep = family
            .spawn(
                &child,
                "three".into(),
                "grandchild".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap_err();
        assert!(too_deep.contains("depth"), "{too_deep}");
    }

    #[tokio::test]
    async fn a_child_message_reaches_an_idle_parent_as_a_prompt() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 2, None);
        let root = Identity::root("root");
        let (mut root_connection, _root_requests) = crate::testing::echo_connection();
        root_connection.attach_agent_inbox(family.root_inbox());
        family
            .spawn(&root, "task".into(), "auth".into(), None, None, &family)
            .await
            .unwrap();
        let children = settle(&family, &root).await;
        let child = Identity {
            id: Some(children[0].id.clone()),
            name: "auth".into(),
            depth: 1,
        };

        let (receipts, ()) = tokio::join!(
            family.send_message(
                &child,
                "parent",
                None,
                "found two issues",
                DeliveryMode::Auto
            ),
            async {
                for _ in 0..50 {
                    root_connection.start_pending().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        );

        let receipts = receipts.unwrap();
        assert_eq!(receipts, vec![("root".to_string(), Receipt::Delivered)]);
        let injected = std::iter::from_fn(|| root_connection.try_next_event())
            .find_map(|event| match event {
                ClientEvent::MessageInjected(event) => Some(event),
                _ => None,
            })
            .expect("the parent saw the message");
        assert_eq!(
            injected.content,
            "[agent-message from child:auth]\n\nfound two issues"
        );
        assert!(matches!(
            injected.receipts[0].origin,
            EventOrigin::Agent { ref name, ref relationship, .. } if name == "auth" && relationship == "child"
        ));
        assert!(root_connection.state().turn_active || root_connection.state().context_len >= 1);
    }

    #[tokio::test]
    async fn a_parent_message_steers_a_busy_child() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Notify::new());
        let factory: ChildFactory = Arc::new({
            let release = Arc::clone(&release);
            move |_spec: ChildSpec, _family: Arc<Family>| {
                let release = Arc::clone(&release);
                Box::pin(async move {
                    let mut registry = ApiRegistry::new();
                    registry.register(Box::new(crate::testing::Stall { release }));
                    let config = WorkerConfig::new(echo_config(echo_model()));
                    Ok(ChildParts {
                        config,
                        tools: ToolRegistry::new(),
                        registry,
                    })
                })
            }
        });
        let family = Family::new(
            Identity::root("root"),
            "root".into(),
            2,
            Some(factory),
            None,
        );
        let root = Identity::root("root");
        family
            .spawn(
                &root,
                "slow task".into(),
                "slow".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while family.children_of(&root)[0].status != ChildStatus::Running {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let receipts = family
            .send_message(&root, "child", Some("slow"), "hurry up", DeliveryMode::Auto)
            .await
            .unwrap();
        assert_eq!(receipts, vec![("slow".to_string(), Receipt::Delivered)]);

        let queued = family
            .send_message(
                &root,
                "child",
                Some("slow"),
                "and then rest",
                DeliveryMode::FollowUp,
            )
            .await
            .unwrap();
        assert_eq!(queued, vec![("slow".to_string(), Receipt::Queued)]);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !family.children_of(&root)[0].status.settled() {
            assert!(Instant::now() < deadline);
            release.notify_one();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(family.children_of(&root)[0].status, ChildStatus::Idle);
        let _ = requests;
    }

    #[tokio::test]
    async fn progress_notes_are_throttled_and_delete_leaves_a_tombstone() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let registry_path = dir.path().join("subagents.json");
        let family = family(requests, 2, Some(registry_path.clone()));
        let root = Identity::root("root");
        family
            .spawn(&root, "task".into(), "noter".into(), None, None, &family)
            .await
            .unwrap();
        let children = settle(&family, &root).await;
        let child = Identity {
            id: Some(children[0].id.clone()),
            name: "noter".into(),
            depth: 1,
        };

        family.progress_note(&child, "halfway").unwrap();
        let throttled = family.progress_note(&child, "nearly").unwrap_err();
        assert!(throttled.contains("one every"));
        family.progress_note(&root, "reading the results").unwrap();
        assert_eq!(
            family.take_root_note().as_deref(),
            Some("reading the results")
        );
        assert!(family.take_root_note().is_none());
        assert_eq!(
            family.children_of(&root)[0].progress_note.as_deref(),
            Some("halfway")
        );

        let deleted = family.delete(&root, "noter").await.unwrap();
        assert_eq!(deleted.status, ChildStatus::Deleted);
        assert!(
            family
                .send_message(&root, "child", Some("noter"), "hi", DeliveryMode::Auto)
                .await
                .is_err()
        );

        let reloaded = Family::new(
            Identity::root("root"),
            "root".into(),
            2,
            None,
            Some(registry_path),
        );
        let entries = reloaded.all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, ChildStatus::Deleted);
        assert_eq!(entries[0].progress_note.as_deref(), Some("halfway"));
    }

    #[tokio::test]
    async fn a_running_child_in_a_saved_registry_comes_back_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagents.json");
        let registry = Registry {
            children: vec![ChildSummary {
                id: "sub-1".into(),
                name: "left".into(),
                parent: None,
                depth: 1,
                session_id: "s".into(),
                model_id: "m".into(),
                status: ChildStatus::Running,
                spawned_at: Timestamp::zero(),
                updated_at: Timestamp::zero(),
                answer: None,
                error: None,
                progress_note: None,
                usage: Usage::default(),
                cost: Cost::default(),
                turns: 1,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();

        let family = Family::new(Identity::root("root"), "root".into(), 2, None, Some(path));

        assert_eq!(family.all()[0].status, ChildStatus::Interrupted);
        let spawn = family
            .spawn(
                &Identity::root("root"),
                "t".into(),
                "n".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap_err();
        assert!(spawn.contains("not available"));
    }

    #[tokio::test]
    async fn the_kernel_spawns_and_collects_a_child_through_the_host() {
        use mush_agent::tool::AgentTool;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 2, None);
        let dir = tempfile::tempdir().unwrap();
        let host = mush_tools::ipython::MushHost::new(dir.path().into());
        host.set_extension(Arc::new(RlmHandler::new(
            Arc::clone(&family),
            Identity::root("root"),
        )));
        let tool = mush_tools::ipython::IpythonTool::with_mush_host(host, dir.path().into());

        let spawned = tool
            .execute(serde_json::json!({
                "code": "h = await rlm.spawn('count the tests', name='counter')\nprint(h.name, h.rlm_child_id.startswith('sub-'))"
            }))
            .await;
        let text = mush_tools::util::extract_text(&spawned);
        assert!(spawned.outcome.is_success(), "{text}");
        assert_eq!(text.trim(), "counter True");

        let collected = tool
            .execute(serde_json::json!({
                "code": "kids = await rlm.collect(timeout_ms=5000)\nprint(kids[0].status, kids[0].answer, kids[0].turns)\nprint((await rlm.list_subagents())[0].name)"
            }))
            .await;
        let text = mush_tools::util::extract_text(&collected);
        assert!(collected.outcome.is_success(), "{text}");
        assert_eq!(text.trim(), "idle child says hi 1\ncounter");

        let noted = tool
            .execute(serde_json::json!({ "code": "await rlm.progress_note('nearly there')" }))
            .await;
        assert!(noted.outcome.is_success());
        assert_eq!(family.take_root_note().as_deref(), Some("nearly there"));
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn a_child_kernel_sends_a_message_to_its_parent() {
        use mush_agent::tool::AgentTool;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 2, None);
        let root = Identity::root("root");
        let (mut root_connection, _root_requests) = crate::testing::echo_connection();
        root_connection.attach_agent_inbox(family.root_inbox());
        family
            .spawn(&root, "task".into(), "auth".into(), None, None, &family)
            .await
            .unwrap();
        let children = settle(&family, &root).await;
        let child = Identity {
            id: Some(children[0].id.clone()),
            name: "auth".into(),
            depth: 1,
        };
        let dir = tempfile::tempdir().unwrap();
        let host = mush_tools::ipython::MushHost::new(dir.path().into());
        host.set_extension(Arc::new(RlmHandler::new(Arc::clone(&family), child)));
        let tool = mush_tools::ipython::IpythonTool::with_mush_host(host, dir.path().into());

        let (sent, ()) = tokio::join!(
            tool.execute(serde_json::json!({
                "code": "r = await agent_message.send('done: two findings')\nprint(r['receipts'][0]['receiver'], r['receipts'][0]['status'])"
            })),
            async {
                for _ in 0..100 {
                    root_connection.start_pending().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        );

        let text = mush_tools::util::extract_text(&sent);
        assert!(sent.outcome.is_success(), "{text}");
        assert_eq!(text.trim(), "root delivered");
        tool.shutdown().await;
    }
}
