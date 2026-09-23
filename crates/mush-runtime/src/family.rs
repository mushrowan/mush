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
pub const CHILD_NAME_MAX_CHARS: usize = 64;
pub const PROGRESS_NOTE_CHARS: usize = 512;
pub const PROGRESS_NOTE_INTERVAL: Duration = Duration::from_secs(10);
pub const MESSAGE_MAX_CHARS: usize = 16 * 1024;
pub const MAX_PENDING_MESSAGES: usize = 20;
pub const MESSAGE_BURST: u32 = 3;
pub const MESSAGE_REFILL: Duration = Duration::from_secs(1);
pub const TASK_LABEL: &str = "[task from parent]";
const DELETED_REASON: &str = "Deleted by parent orchestrator";
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
    pub parent_session_id: String,
}

pub struct ChildParts {
    pub config: WorkerConfig,
    pub tools: ToolRegistry,
    pub registry: ApiRegistry,
}

pub type ChildFactory = Arc<
    dyn Fn(ChildSpec, Arc<Family>) -> BoxFuture<'static, Result<ChildParts, String>> + Send + Sync,
>;

pub struct CreateSessionRequest {
    pub prompt: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedSession {
    pub session_id: String,
    pub name: Option<String>,
    pub model: String,
}

pub type SessionSpawner = Arc<
    dyn Fn(CreateSessionRequest) -> BoxFuture<'static, Result<CreatedSession, String>>
        + Send
        + Sync,
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

    #[must_use]
    pub fn alive(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Idle)
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
    #[serde(default)]
    pub streaming: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_running: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting_confirmation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<mush_session::Pause>,
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

const TOOL_LABEL: usize = 120;
const OBSERVE_DEFAULT_LIMIT: usize = 8;
pub const OBSERVE_MAX_LIMIT: usize = 50;
pub const OBSERVE_DEFAULT_CHARS: usize = 800;
pub const OBSERVE_MIN_CHARS: usize = 80;
pub const OBSERVE_MAX_CHARS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePreview {
    pub role: String,
    pub text: String,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
}

pub struct ObserveRequest {
    pub limit: usize,
    pub max_chars: usize,
    pub reply: oneshot::Sender<Vec<MessagePreview>>,
}

pub type ObserveInbox = mpsc::UnboundedReceiver<ObserveRequest>;

#[must_use]
pub fn preview_messages(
    messages: &[mush_ai::types::Message],
    limit: usize,
    max_chars: usize,
) -> Vec<MessagePreview> {
    use mush_ai::types::Message;
    messages
        .iter()
        .rev()
        .take(limit)
        .rev()
        .map(|message| {
            let (role, text, tool_calls) = match message {
                Message::User(user) => ("user", user.text(), Vec::new()),
                Message::Assistant(assistant) => (
                    "assistant",
                    assistant.text(),
                    assistant
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            mush_ai::types::AssistantContentPart::ToolCall(call) => {
                                Some(call.name.to_string())
                            }
                            _ => None,
                        })
                        .collect(),
                ),
                Message::ToolResult(result) => (
                    "tool_result",
                    result
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            mush_ai::types::ToolResultContentPart::Text(text) => {
                                Some(text.text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    vec![result.tool_name.to_string()],
                ),
                Message::Event(event) => ("event", event.content.clone(), Vec::new()),
            };
            let truncated = text.chars().count() > max_chars;
            let text = if truncated {
                text.chars().take(max_chars).collect()
            } else {
                text
            };
            MessagePreview {
                role: role.to_string(),
                text,
                truncated,
                tool_calls,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentView {
    pub name: String,
    pub relationship: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rlm_child_id: Option<ChildId>,
    pub status: String,
    pub observable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_note: Option<String>,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_running: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting_confirmation: Option<String>,
    #[serde(default)]
    pub children_running: usize,
}

struct Node {
    summary: ChildSummary,
    inbox: Option<mpsc::UnboundedSender<Delivery>>,
    observe: Option<mpsc::UnboundedSender<ObserveRequest>>,
    control: Option<mpsc::UnboundedSender<ClientCommand>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    last_note_at: Option<Instant>,
    replied: bool,
    notified: bool,
    awaiting_quiet: bool,
    cancel_reason: Option<String>,
}

struct Inner {
    root: Identity,
    root_session_id: String,
    root_inbox: Option<mpsc::UnboundedSender<Delivery>>,
    root_observe: Option<mpsc::UnboundedSender<ObserveRequest>>,
    root_note: Option<String>,
    root_note_at: Option<Instant>,
    root_running: bool,
    children: BTreeMap<ChildId, Node>,
    reserved_names: std::collections::HashSet<(Option<ChildId>, String)>,
    message_buckets: std::collections::HashMap<(String, String), Bucket>,
}

#[derive(Clone, Copy)]
struct Bucket {
    tokens: u32,
    updated_at: Instant,
}

impl Bucket {
    fn take(&mut self, now: Instant) -> Result<(), Duration> {
        let refills = (now.saturating_duration_since(self.updated_at).as_millis()
            / MESSAGE_REFILL.as_millis()) as u32;
        if refills > 0 {
            self.tokens = (self.tokens + refills).min(MESSAGE_BURST);
            self.updated_at += MESSAGE_REFILL * refills;
        }
        if self.tokens == 0 {
            return Err((self.updated_at + MESSAGE_REFILL)
                .saturating_duration_since(now)
                .max(Duration::from_millis(1)));
        }
        self.tokens -= 1;
        Ok(())
    }
}

pub struct Family {
    inner: Mutex<Inner>,
    factory: Option<ChildFactory>,
    registry_path: Mutex<Option<PathBuf>>,
    max_depth: std::sync::atomic::AtomicU32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    children: Vec<ChildSummary>,
}

fn load_registry(path: Option<&std::path::Path>) -> BTreeMap<ChildId, Node> {
    let Some(registry) = path
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Registry>(&bytes).ok())
    else {
        return BTreeMap::new();
    };
    registry
        .children
        .into_iter()
        .map(|mut summary| {
            if !summary.status.settled() {
                summary.status = ChildStatus::Interrupted;
            }
            let node = Node {
                summary,
                inbox: None,
                observe: None,
                control: None,
                shutdown: None,
                task: None,
                last_note_at: None,
                replied: false,
                notified: false,
                awaiting_quiet: false,
                cancel_reason: None,
            };
            (node.summary.id.clone(), node)
        })
        .collect()
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
        let children = load_registry(registry_path.as_deref());
        Arc::new(Self {
            inner: Mutex::new(Inner {
                root,
                root_session_id,
                root_inbox: None,
                root_observe: None,
                root_note: None,
                root_note_at: None,
                root_running: false,
                children,
                reserved_names: std::collections::HashSet::new(),
                message_buckets: std::collections::HashMap::new(),
            }),
            factory,
            registry_path: Mutex::new(registry_path),
            max_depth: std::sync::atomic::AtomicU32::new(max_depth),
        })
    }

    #[must_use]
    pub fn max_depth(&self) -> u32 {
        self.max_depth.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_max_depth(&self, max_depth: u32) {
        self.max_depth
            .store(max_depth, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn root_inbox(&self) -> AgentInbox {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().root_inbox = Some(tx);
        rx
    }

    pub fn root_observe_inbox(&self) -> ObserveInbox {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().root_observe = Some(tx);
        rx
    }

    pub fn rebind(&self, session_id: String, registry_path: Option<PathBuf>) {
        for node in self.lock().children.values_mut() {
            if let Some(shutdown) = node.shutdown.take() {
                let _ = shutdown.send(());
            }
            if !node.summary.status.settled() {
                node.summary.status = ChildStatus::Cancelled;
            }
        }
        self.persist();
        let children = load_registry(registry_path.as_deref());
        {
            let mut inner = self.lock();
            inner.root_session_id = session_id;
            inner.children = children;
        }
        *self
            .registry_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = registry_path;
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
            .filter(|node| {
                node.summary.parent == caller.id && node.summary.status != ChildStatus::Deleted
            })
            .map(|node| node.summary.clone())
            .collect()
    }

    pub fn set_root_running(&self, running: bool) {
        self.lock().root_running = running;
    }

    #[must_use]
    pub fn descendants_running(&self, id: Option<&str>) -> bool {
        let inner = self.lock();
        match id {
            Some(id) => subtree_busy(&inner, id),
            None => inner
                .children
                .values()
                .any(|node| !node.summary.status.settled()),
        }
    }

    #[must_use]
    pub fn all(&self) -> Vec<ChildSummary> {
        self.lock()
            .children
            .values()
            .map(|node| node.summary.clone())
            .collect()
    }

    #[must_use]
    pub fn roster(&self, caller: &Identity) -> Vec<AgentView> {
        let inner = self.lock();
        let mut views = Vec::new();
        match &caller.id {
            None => views.push(AgentView {
                name: inner.root.name.clone(),
                relationship: "self".into(),
                session_id: inner.root_session_id.clone(),
                rlm_child_id: None,
                status: root_status(&inner).into(),
                observable: false,
                model_id: None,
                progress_note: inner.root_note.clone(),
                streaming: false,
                tool_running: None,
                awaiting_confirmation: None,
                children_running: running_children(&inner, &None),
            }),
            Some(id) => {
                let me = inner.children.get(id);
                if let Some(node) = me {
                    views.push(view_of(
                        node,
                        "self",
                        false,
                        running_children(&inner, &Some(node.summary.id.clone())),
                    ));
                }
                let parent = me.and_then(|node| node.summary.parent.clone());
                match &parent {
                    None => views.push(AgentView {
                        name: inner.root.name.clone(),
                        relationship: "parent".into(),
                        session_id: inner.root_session_id.clone(),
                        rlm_child_id: None,
                        status: root_status(&inner).into(),
                        observable: inner.root_observe.is_some(),
                        model_id: None,
                        progress_note: inner.root_note.clone(),
                        streaming: false,
                        tool_running: None,
                        awaiting_confirmation: None,
                        children_running: running_children(&inner, &None),
                    }),
                    Some(parent_id) => {
                        if let Some(node) = inner.children.get(parent_id) {
                            views.push(view_of(
                                node,
                                "parent",
                                node.observe.is_some(),
                                running_children(&inner, &Some(node.summary.id.clone())),
                            ));
                        }
                    }
                }
                for node in inner.children.values() {
                    if &node.summary.id != id
                        && node.summary.parent == parent
                        && node.summary.status != ChildStatus::Deleted
                    {
                        views.push(view_of(
                            node,
                            "sibling",
                            node.observe.is_some(),
                            running_children(&inner, &Some(node.summary.id.clone())),
                        ));
                    }
                }
            }
        }
        for node in inner.children.values() {
            if node.summary.parent == caller.id && node.summary.status != ChildStatus::Deleted {
                views.push(view_of(
                    node,
                    "child",
                    node.observe.is_some(),
                    running_children(&inner, &Some(node.summary.id.clone())),
                ));
            }
        }
        views
    }

    pub fn get_agent(&self, caller: &Identity, target: &str) -> Result<AgentView, String> {
        let mut matches = self.roster(caller).into_iter().filter(|view| {
            view.name == target
                || view.session_id == target
                || view.rlm_child_id.as_deref() == Some(target)
        });
        let found = matches
            .next()
            .ok_or_else(|| format!("{target} is not you, your parent, a sibling or a child"))?;
        if matches.next().is_some() {
            return Err(format!(
                "{target} names more than one agent, use its session id"
            ));
        }
        Ok(found)
    }

    pub async fn recent_messages(
        &self,
        caller: &Identity,
        target: &str,
        limit: usize,
        max_chars: usize,
    ) -> Result<Vec<MessagePreview>, String> {
        let limit = limit.clamp(1, OBSERVE_MAX_LIMIT);
        let max_chars = max_chars.clamp(OBSERVE_MIN_CHARS, OBSERVE_MAX_CHARS);
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let reachable = self.roster(caller);
            let view = reachable
                .iter()
                .find(|view| {
                    view.relationship != "self"
                        && (view.name == target
                            || view.session_id == target
                            || view.rlm_child_id.as_deref() == Some(target))
                })
                .ok_or_else(|| {
                    format!(
                        "{target} is not your parent, sibling or child, or it has no live session"
                    )
                })?;
            if !view.observable {
                return Err(format!("{target} has no live session to read"));
            }
            let inner = self.lock();
            let sender = match &view.rlm_child_id {
                None => inner.root_observe.clone(),
                Some(id) => inner.children.get(id).and_then(|node| node.observe.clone()),
            };
            sender
                .ok_or_else(|| format!("{target} has no live session to read"))?
                .send(ObserveRequest {
                    limit,
                    max_chars,
                    reply: reply_tx,
                })
                .map_err(|_| format!("{target} stopped listening"))?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), reply_rx)
            .await
            .map_err(|_| format!("{target} did not answer in time"))?
            .map_err(|_| format!("{target} stopped listening"))
    }

    fn resolve(&self, caller: &Identity, target: &str) -> Result<ChildId, String> {
        let inner = self.lock();
        let mut matches = inner.children.values().filter(|node| {
            node.summary.parent == caller.id
                && node.summary.status != ChildStatus::Deleted
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
        if name.chars().count() > CHILD_NAME_MAX_CHARS {
            return Err(format!(
                "name must be at most {CHILD_NAME_MAX_CHARS} characters"
            ));
        }
        if prompt.trim().is_empty() {
            return Err("prompt is required".into());
        }
        let max_depth = self.max_depth();
        if caller.depth + 1 > max_depth {
            return Err(format!(
                "recursion depth {max_depth} is the limit, this agent cannot spawn children"
            ));
        }
        let factory = self
            .factory
            .clone()
            .ok_or_else(|| "subagents are not available in this session".to_string())?;
        let id = format!("sub-{}", crate::schedule::short_id());
        let reservation = (caller.id.clone(), name.clone());
        {
            let mut inner = self.lock();
            let live = inner
                .children
                .values()
                .filter(|node| !node.summary.status.settled())
                .count();
            if live >= MAX_CHILDREN {
                return Err(format!("{MAX_CHILDREN} children are already running"));
            }
            if inner.reserved_names.contains(&reservation)
                || inner.children.values().any(|node| {
                    node.summary.parent == caller.id
                        && node.summary.name == name
                        && node.summary.status != ChildStatus::Deleted
                })
            {
                return Err(format!("you already have a child called {name}"));
            }
            inner.reserved_names.insert(reservation.clone());
        }
        let parent_session_id = {
            let inner = self.lock();
            match caller.id.as_deref() {
                Some(id) => inner
                    .children
                    .get(id)
                    .map(|node| node.summary.session_id.clone())
                    .unwrap_or_default(),
                None => inner.root_session_id.clone(),
            }
        };
        let spec = ChildSpec {
            id: id.clone(),
            name: name.clone(),
            prompt: prompt.clone(),
            model,
            thinking,
            depth: caller.depth + 1,
            parent: caller.clone(),
            parent_session_id,
        };
        let parts = match (factory)(spec, Arc::clone(this)).await {
            Ok(parts) => parts,
            Err(error) => {
                self.lock().reserved_names.remove(&reservation);
                return Err(error);
            }
        };
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
            streaming: false,
            tool_running: None,
            awaiting_confirmation: None,
            paused: None,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut connection = parts.config.connect(&parts.tools, &parts.registry);
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        connection.attach_agent_inbox(inbox_rx);
        let (observe_tx, observe_rx) = mpsc::unbounded_channel();
        connection.attach_observe_inbox(observe_rx);
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(drive_child(
            Arc::clone(this),
            id.clone(),
            connection,
            shutdown_rx,
            control_rx,
            format!("{TASK_LABEL}\n\n{prompt}"),
        ));
        let mut inner = self.lock();
        inner.reserved_names.remove(&reservation);
        inner.children.insert(
            id.clone(),
            Node {
                summary,
                inbox: Some(inbox_tx),
                observe: Some(observe_tx),
                control: Some(control_tx),
                shutdown: Some(shutdown_tx),
                task: Some(task),
                last_note_at: None,
                replied: false,
                notified: false,
                awaiting_quiet: false,
                cancel_reason: None,
            },
        );
        drop(inner);
        self.persist();
        Ok(ChildHandle {
            rlm_child_id: id,
            name,
            session_id,
            model: model_id,
        })
    }

    fn update(&self, id: &str, apply: impl FnOnce(&mut ChildSummary)) {
        let settled_parent = {
            let mut inner = self.lock();
            let Some(node) = inner.children.get_mut(id) else {
                return;
            };
            let was_settled = node.summary.status.settled();
            apply(&mut node.summary);
            node.summary.updated_at = Timestamp::now();
            (!was_settled && node.summary.status.settled())
                .then(|| node.summary.parent.clone())
                .flatten()
        };
        self.persist();
        if let Some(parent) = settled_parent {
            self.settle_if_quiet(&parent);
        }
    }

    fn settle_if_quiet(&self, id: &str) {
        let settled = {
            let mut inner = self.lock();
            if subtree_busy(&inner, id) {
                return;
            }
            let Some(node) = inner.children.get_mut(id) else {
                return;
            };
            if !node.awaiting_quiet {
                return;
            }
            node.awaiting_quiet = false;
            node.summary.status == ChildStatus::Running
        };
        if settled {
            self.update(id, |child| child.status = ChildStatus::Idle);
            self.post_exit_notice(id, ExitNotice::NoReply);
        }
    }

    fn record(&self, id: &str, event: &ClientEvent, model: Option<&mush_ai::types::Model>) {
        match event {
            ClientEvent::TurnStarted => {
                if let Some(node) = self.lock().children.get_mut(id) {
                    node.awaiting_quiet = false;
                }
                self.update(id, |child| {
                    child.status = ChildStatus::Running;
                    child.turns += 1;
                });
            }
            ClientEvent::Agent {
                event: AgentEvent::MessageStart { .. },
                ..
            } => self.update(id, |child| child.streaming = true),
            ClientEvent::Agent {
                event:
                    AgentEvent::ToolExecStart {
                        tool_name, args, ..
                    },
                ..
            } => {
                let label = tool_label(tool_name, args);
                self.update(id, |child| {
                    child.awaiting_confirmation = None;
                    child.tool_running = Some(label);
                });
            }
            ClientEvent::Agent {
                event: AgentEvent::ToolExecEnd { .. },
                ..
            } => self.update(id, |child| child.tool_running = None),
            ClientEvent::ConfirmRequest { prompt, .. } => {
                let prompt = prompt.clone();
                self.update(id, |child| child.awaiting_confirmation = Some(prompt));
            }
            ClientEvent::ToolsCancelled(_) => self.update(id, |child| {
                child.tool_running = None;
                child.awaiting_confirmation = None;
            }),
            ClientEvent::Agent {
                event: AgentEvent::MessageEnd { message },
                ..
            } => {
                let text = message.text();
                let usage = message.usage;
                let cost = model.map(|model| mush_ai::models::calculate_cost(model, &usage));
                self.update(id, |child| {
                    child.streaming = false;
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
            } => {
                self.update(id, |child| {
                    child.status = ChildStatus::Failed;
                    child.error = Some(error.clone());
                });
                self.post_exit_notice(id, ExitNotice::Failed);
            }
            ClientEvent::TurnEnded { cancelled } => {
                let busy = subtree_busy(&self.lock(), id);
                let mut ended = None;
                self.update(id, |child| {
                    child.streaming = false;
                    child.tool_running = None;
                    child.awaiting_confirmation = None;
                    if child.status == ChildStatus::Running {
                        ended = Some(if *cancelled {
                            ChildStatus::Cancelled
                        } else if busy {
                            ChildStatus::Running
                        } else {
                            ChildStatus::Idle
                        });
                        child.status = ended.unwrap_or(child.status);
                    }
                });
                match ended {
                    Some(ChildStatus::Cancelled) => {
                        self.post_exit_notice(id, ExitNotice::Cancelled)
                    }
                    Some(ChildStatus::Idle) => self.post_exit_notice(id, ExitNotice::NoReply),
                    Some(_) => {
                        if let Some(node) = self.lock().children.get_mut(id) {
                            node.awaiting_quiet = true;
                        }
                        self.settle_if_quiet(id);
                    }
                    None => {}
                }
            }
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
        let (shutdown, running) = {
            let mut inner = self.lock();
            let Some(node) = inner.children.get_mut(&id) else {
                return Err(format!("{id} vanished"));
            };
            let running = !node.summary.status.settled();
            if running {
                node.cancel_reason = Some(DELETED_REASON.into());
            }
            (node.shutdown.take(), running)
        };
        if let Some(shutdown) = shutdown {
            let _ = shutdown.send(());
        }
        self.update(&id, |child| child.status = ChildStatus::Deleted);
        if running {
            self.post_exit_notice(&id, ExitNotice::Cancelled);
        }
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

    pub fn set_paused(
        &self,
        caller: &Identity,
        targets: &[String],
        pause: Option<&mush_session::Pause>,
    ) -> Result<Vec<ChildSummary>, String> {
        let roots = if targets.is_empty() {
            self.lock()
                .children
                .values()
                .filter(|node| node.summary.parent == caller.id && node.summary.status.alive())
                .map(|node| node.summary.id.clone())
                .collect()
        } else {
            targets
                .iter()
                .map(|target| self.resolve(caller, target))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut queue = roots;
        let mut reached = Vec::new();
        while let Some(id) = queue.pop() {
            if reached.contains(&id) {
                continue;
            }
            let descendants: Vec<ChildId> = self
                .lock()
                .children
                .values()
                .filter(|node| node.summary.parent.as_ref() == Some(&id))
                .map(|node| node.summary.id.clone())
                .collect();
            queue.extend(descendants);
            reached.push(id);
        }
        let mut summaries = Vec::new();
        for id in reached {
            let control = self
                .lock()
                .children
                .get(&id)
                .and_then(|node| node.control.clone());
            if let Some(control) = control {
                let _ = control.send(ClientCommand::SetPaused(pause.cloned()));
            }
            self.update(&id, |child| child.paused = pause.cloned());
            if let Some(summary) = self
                .lock()
                .children
                .get(&id)
                .map(|node| node.summary.clone())
            {
                summaries.push(summary);
            }
        }
        self.persist();
        Ok(summaries)
    }

    pub fn take_root_note(&self) -> Option<String> {
        self.lock().root_note.take()
    }

    pub fn progress_note(&self, caller: &Identity, text: &str) -> Result<Option<Duration>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a progress note needs some text".into());
        }
        if text.chars().count() > PROGRESS_NOTE_CHARS {
            return Err(format!(
                "a progress note must be at most {PROGRESS_NOTE_CHARS} characters"
            ));
        }
        let throttled = |last: Option<Instant>| {
            last.map(|at| at.elapsed())
                .filter(|elapsed| *elapsed < PROGRESS_NOTE_INTERVAL)
                .map(|elapsed| PROGRESS_NOTE_INTERVAL - elapsed)
        };
        {
            let mut inner = self.lock();
            let Some(id) = caller.id.as_deref() else {
                if let Some(retry) = throttled(inner.root_note_at) {
                    return Ok(Some(retry));
                }
                inner.root_note_at = Some(Instant::now());
                inner.root_note = Some(text.to_string());
                return Ok(None);
            };
            let node = inner
                .children
                .get_mut(id)
                .ok_or_else(|| format!("{id} is not in the family"))?;
            if let Some(retry) = throttled(node.last_note_at) {
                return Ok(Some(retry));
            }
            node.last_note_at = Some(Instant::now());
            node.summary.progress_note = Some(text.to_string());
            node.summary.updated_at = Timestamp::now();
        }
        self.persist();
        Ok(None)
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
    ) -> Result<Vec<(String, Result<Receipt, String>)>, String> {
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
                drop(inner);
                if let Some(node) = self.lock().children.get_mut(id) {
                    node.replied = true;
                }
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
                            && node.summary.status != ChildStatus::Deleted
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
                    if node.summary.parent == caller.id
                        && node.summary.status != ChildStatus::Deleted
                    {
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
        if receiver_role != "all" {
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
                receipts.push((target_name, Ok(receipt)));
            }
            return Ok(receipts);
        }
        if let Some(id) = caller.id.as_deref()
            && let Some(node) = self.lock().children.get_mut(id)
        {
            node.replied = true;
        }
        let sends = targets
            .iter()
            .map(|(target, _, sender)| self.deliver(caller, target.as_deref(), sender, text, mode));
        let results = futures::future::join_all(sends).await;
        Ok(targets
            .into_iter()
            .zip(results)
            .map(|((_, target_name, _), result)| {
                let result = match result {
                    Ok(Receipt::Rejected) => Err(format!(
                        "{target_name} has {MAX_PENDING_MESSAGES} messages waiting already"
                    )),
                    other => other,
                };
                (target_name, result)
            })
            .collect())
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
        let (sender_session, bucket_key) = {
            let mut inner = self.lock();
            let session_of = |id: Option<&str>| match id {
                Some(id) => inner
                    .children
                    .get(id)
                    .map(|node| node.summary.session_id.clone())
                    .unwrap_or_default(),
                None => inner.root_session_id.clone(),
            };
            let sender_session = session_of(caller.id.as_deref());
            let key = (sender_session.clone(), session_of(target));
            let now = Instant::now();
            inner
                .message_buckets
                .entry(key.clone())
                .or_insert(Bucket {
                    tokens: MESSAGE_BURST,
                    updated_at: now,
                })
                .take(now)
                .map_err(|retry| {
                    format!(
                        "agent messaging rate limit exceeded, retry after {}ms",
                        retry.as_millis()
                    )
                })?;
            (sender_session, key)
        };
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
        let delivered = self.send_delivery(target, event, mode).await;
        if delivered.is_err()
            && let Some(bucket) = self.lock().message_buckets.get_mut(&bucket_key)
        {
            bucket.tokens = (bucket.tokens + 1).min(MESSAGE_BURST);
        }
        delivered
    }

    async fn send_delivery(
        &self,
        target: Option<&str>,
        event: EventMessage,
        mode: DeliveryMode,
    ) -> Result<Receipt, String> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitNotice {
    NoReply,
    Cancelled,
    Failed,
}

pub const EXIT_NOTICE_PREVIEW_CHARS: usize = 160;

impl Family {
    fn post_exit_notice(&self, id: &str, notice: ExitNotice) {
        let timestamp_ms = Timestamp::now();
        let (parent_inbox, content, child_session) = {
            let mut inner = self.lock();
            let Some(node) = inner.children.get_mut(id) else {
                return;
            };
            if node.notified || (notice == ExitNotice::NoReply && node.replied) {
                return;
            }
            node.notified = true;
            let name = node.summary.name.clone();
            let parent = node.summary.parent.clone();
            let session = node.summary.session_id.clone();
            let content = match notice {
                ExitNotice::NoReply => match node.summary.answer.as_deref() {
                    Some(answer) if !answer.trim().is_empty() => format!(
                        "[child-exited: no-reply child:{name}]\n\nLast assistant text: {}",
                        compact_preview(answer, EXIT_NOTICE_PREVIEW_CHARS)
                    ),
                    _ => format!("[child-exited: no-reply child:{name}]"),
                },
                ExitNotice::Cancelled => match node
                    .cancel_reason
                    .as_deref()
                    .or(node.summary.error.as_deref())
                {
                    Some(reason) => format!("[child-exited: cancelled child:{name}]\n\n{reason}"),
                    None => format!("[child-exited: cancelled child:{name}]"),
                },
                ExitNotice::Failed => format!(
                    "[child-failed child:{name}]\n\n{}",
                    node.summary.error.as_deref().unwrap_or("unknown error")
                ),
            };
            let inbox = match &parent {
                Some(parent) => inner
                    .children
                    .get(parent)
                    .and_then(|node| node.inbox.clone()),
                None => inner.root_inbox.clone(),
            };
            (inbox, content, session)
        };
        let Some(inbox) = parent_inbox else {
            return;
        };
        let _ = inbox.send(Delivery {
            event: EventMessage {
                content,
                timestamp_ms,
                kind: EventKind::Message,
                receipts: vec![EventReceipt {
                    id: format!("child-exit-{}", mush_ai::types::SessionId::new()).into(),
                    timestamp_ms,
                    kind: EventKind::Message,
                    origin: EventOrigin::Agent {
                        session_id: child_session,
                        name: id.to_string(),
                        relationship: "child".into(),
                    },
                }],
            },
            mode: DeliveryMode::Auto,
            receipt: None,
        });
    }
}

fn compact_preview(text: &str, max: usize) -> String {
    let normalised = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalised.chars().count() <= max {
        return normalised;
    }
    let head: String = normalised.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn view_of(
    node: &Node,
    relationship: &str,
    observable: bool,
    children_running: usize,
) -> AgentView {
    let summary = &node.summary;
    AgentView {
        name: summary.name.clone(),
        relationship: relationship.to_string(),
        session_id: summary.session_id.clone(),
        rlm_child_id: Some(summary.id.clone()),
        status: family_status(summary.status, node.inbox.is_some()).into(),
        observable,
        model_id: Some(summary.model_id.clone()),
        progress_note: summary.progress_note.clone(),
        streaming: summary.streaming,
        tool_running: summary.tool_running.clone(),
        awaiting_confirmation: summary.awaiting_confirmation.clone(),
        children_running,
    }
}

fn root_status(inner: &Inner) -> &'static str {
    if inner.root_running {
        "running"
    } else {
        "idle"
    }
}

fn family_status(status: ChildStatus, live: bool) -> &'static str {
    match status {
        ChildStatus::Starting | ChildStatus::Running => "running",
        ChildStatus::Idle | ChildStatus::Failed | ChildStatus::Cancelled if live => "idle",
        ChildStatus::Idle
        | ChildStatus::Failed
        | ChildStatus::Cancelled
        | ChildStatus::Interrupted
        | ChildStatus::Deleted => "inactive",
    }
}

fn tool_label(tool_name: &str, args: &serde_json::Value) -> String {
    let summary = mush_agent::summarise_tool_args(tool_name, args);
    let summary = summary.split('\n').next().unwrap_or_default();
    let clipped: String = summary.chars().take(TOOL_LABEL).collect();
    if clipped.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}: {clipped}")
    }
}

fn subtree_busy(inner: &Inner, id: &str) -> bool {
    let mut queue = vec![id.to_string()];
    while let Some(parent) = queue.pop() {
        for node in inner.children.values() {
            if node.summary.parent.as_deref() == Some(parent.as_str()) {
                if !node.summary.status.settled() {
                    return true;
                }
                queue.push(node.summary.id.clone());
            }
        }
    }
    false
}

fn running_children(inner: &Inner, parent: &Option<ChildId>) -> usize {
    inner
        .children
        .values()
        .filter(|node| {
            &node.summary.parent == parent && node.summary.status == ChildStatus::Running
        })
        .count()
}

async fn drive_child(
    family: Arc<Family>,
    id: ChildId,
    mut connection: InProcessConnection,
    shutdown: oneshot::Receiver<()>,
    mut control: mpsc::UnboundedReceiver<ClientCommand>,
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
            family.post_exit_notice(&id, ExitNotice::Failed);
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
            Some(command) = control.recv() => {
                match connection.send(command).await {
                    Ok(events) => {
                        for event in &events {
                            family.record(&id, event, Some(&model));
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%id, %error, "a child refused a control command");
                    }
                }
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
    harness: Option<Arc<crate::harness::HarnessStore>>,
    sessions: Option<SessionSpawner>,
    host_commands: Option<Arc<crate::connection::HostCommands>>,
    compaction_callable: bool,
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
            harness: None,
            sessions: None,
            host_commands: None,
            compaction_callable: true,
        }
    }

    #[must_use]
    pub fn with_compaction_callable(mut self, callable: bool) -> Self {
        self.compaction_callable = callable;
        self
    }

    #[must_use]
    pub fn with_host_commands(mut self, commands: Arc<crate::connection::HostCommands>) -> Self {
        self.host_commands = Some(commands);
        self
    }

    #[must_use]
    pub fn with_session_spawner(mut self, spawner: SessionSpawner) -> Self {
        self.sessions = Some(spawner);
        self
    }

    #[must_use]
    pub fn with_notes(mut self, notes: Arc<crate::notes::Notes>) -> Self {
        self.notes = Some(notes);
        self
    }

    #[must_use]
    pub fn with_harness(mut self, harness: Arc<crate::harness::HarnessStore>) -> Self {
        self.harness = Some(harness);
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

    fn goal_store(&self) -> Result<&Arc<crate::goal::GoalStore>, mush_kernel::HostError> {
        self.goal
            .as_ref()
            .ok_or_else(|| mush_kernel::HostError("goals are not available here".into()))
    }

    fn harness_store(&self) -> Result<&Arc<crate::harness::HarnessStore>, mush_kernel::HostError> {
        self.harness
            .as_ref()
            .ok_or_else(|| mush_kernel::HostError("the harness is not available here".into()))
    }

    fn harness_call(
        &self,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, mush_kernel::HostError> {
        use mush_session::harness::{Action, Edit, Kind as EntryKind};

        let store = self.harness_store()?;
        let text = |key: &str| -> Option<String> {
            payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let object = |key: &str| {
            payload
                .get(key)
                .and_then(serde_json::Value::as_object)
                .cloned()
        };
        let global = payload
            .get("global")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let entry_kind = || -> Result<Option<EntryKind>, mush_kernel::HostError> {
            let Some(raw) = text("kind") else {
                return Ok(None);
            };
            EntryKind::parse(&raw).map(Some).ok_or_else(|| {
                mush_kernel::HostError(format!(
                    "kind must be prompt, memory, skill or subagent, not {raw}"
                ))
            })
        };
        let required_kind = || -> Result<EntryKind, mush_kernel::HostError> {
            entry_kind()?.ok_or_else(|| mush_kernel::HostError("kind is required".into()))
        };
        let required_id = || -> Result<String, mush_kernel::HostError> {
            text("id").ok_or_else(|| mush_kernel::HostError("id is required".into()))
        };
        let entries =
            |entries: Vec<mush_session::harness::Entry>| serde_json::json!({ "entries": entries });

        match kind {
            "harness.list" => Ok(entries(store.list(entry_kind()?, global))),
            "harness.overview" => Ok(serde_json::json!({ "overview": store.overview(global) })),
            "harness.search" => {
                let query = text("query")
                    .ok_or_else(|| mush_kernel::HostError("query is required".into()))?;
                let limit = payload
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(10)
                    .clamp(1, 100) as usize;
                Ok(entries(store.search(&query, entry_kind()?, limit, global)))
            }
            "harness.record_refinement" => {
                let trigger = text("trigger")
                    .ok_or_else(|| mush_kernel::HostError("trigger is required".into()))?;
                let changes = match payload.get("changes") {
                    Some(serde_json::Value::String(one)) => vec![one.clone()],
                    Some(serde_json::Value::Array(many)) => many
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    _ => Vec::new(),
                };
                let event = mush_session::harness::RefinementEvent {
                    id: text("id").unwrap_or_else(mush_session::harness::new_id),
                    trigger,
                    changes,
                    evidence: text("evidence").unwrap_or_default(),
                    outcome: text("outcome").unwrap_or_default(),
                    created_at: mush_session::harness::now(),
                };
                let recorded = store
                    .record_refinement(global, event)
                    .map_err(mush_kernel::HostError)?;
                Ok(serde_json::json!({ "refinement": recorded }))
            }
            "harness.get" => {
                let found = store.find(required_kind()?, &required_id()?, global);
                Ok(serde_json::json!({ "entry": found.map(|(_, entry)| entry) }))
            }
            "harness.create" | "harness.update" => {
                let creating = kind == "harness.create";
                let edit = Edit {
                    action: if creating {
                        Action::Create
                    } else {
                        Action::Update
                    },
                    kind: required_kind()?,
                    id: text("id"),
                    title: text("title"),
                    content: text("content"),
                    path: text("path"),
                    reference: object("reference"),
                    arguments: object("arguments"),
                    metadata: object("metadata"),
                    reason: None,
                };
                let entry = if creating {
                    store.create(global, &edit)
                } else {
                    store.update(&required_id()?, global, &edit)
                }
                .map_err(mush_kernel::HostError)?;
                Ok(serde_json::json!({ "entry": entry }))
            }
            "harness.delete" => Ok(serde_json::json!({
                "deleted": store
                    .delete(required_kind()?, &required_id()?, global)
                    .is_ok(),
            })),
            other => Err(mush_kernel::HostError(format!("unknown request {other}"))),
        }
    }

    fn goal_reply(&self, completing: bool) -> serde_json::Value {
        let goal = self.goal.as_ref().and_then(|goal| goal.get());
        let report = goal
            .as_ref()
            .filter(|_| completing)
            .and_then(Goal::completion_budget_report);
        let remaining = goal.as_ref().and_then(Goal::remaining_tokens);
        let goal = goal.map(|goal| {
            let seconds = goal.elapsed_secs();
            let mut value = serde_json::to_value(goal).unwrap_or(serde_json::Value::Null);
            if let Some(object) = value.as_object_mut() {
                object.insert("time_used_seconds".into(), seconds.into());
            }
            value
        });
        serde_json::json!({
            "goal": goal,
            "remaining_tokens": remaining,
            "completion_budget_report": report,
        })
    }

    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
}

fn heartbeat_interval(interval: String) -> String {
    if interval.trim_start().starts_with("every ") {
        interval
    } else {
        format!("every {interval}")
    }
}

fn heartbeat_delivery(
    mode: Option<String>,
) -> Result<Option<crate::schedule::Delivery>, mush_kernel::HostError> {
    match mode.as_deref() {
        None => Ok(None),
        Some("steer") => Ok(Some(crate::schedule::Delivery::Steer)),
        Some("follow_up") => Ok(Some(crate::schedule::Delivery::FollowUp)),
        Some(other) => Err(mush_kernel::HostError(format!(
            "delivery_mode must be steer or follow_up, not {other}"
        ))),
    }
}

fn requested_thinking(
    level: Option<String>,
) -> Result<Option<ThinkingLevel>, mush_kernel::HostError> {
    match level.as_deref().map(ThinkingLevel::from_effort_str) {
        Some(ThinkingLevel::Other(other)) => Err(mush_kernel::HostError(format!(
            "thinking must be off, minimal, low, medium, high or xhigh, not {other}"
        ))),
        known => Ok(known),
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
                let thinking = requested_thinking(str_field("thinking"))?;
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
            "rlm.pause_subagents" | "rlm.resume_subagents" => {
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
                let pause = (kind == "rlm.pause_subagents").then(|| mush_session::Pause {
                    at: Timestamp::now(),
                    reason: str_field("reason"),
                    by: Some(self.identity.name.clone()),
                });
                let children = self
                    .family
                    .set_paused(&self.identity, &targets, pause.as_ref())
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
                let retry = self
                    .family
                    .progress_note(&self.identity, &text)
                    .map_err(mush_kernel::HostError)?;
                Ok(match retry {
                    None => serde_json::json!({ "accepted": true }),
                    Some(retry) => serde_json::json!({
                        "accepted": false,
                        "retry_after_ms": u64::try_from(retry.as_millis()).unwrap_or(u64::MAX),
                    }),
                })
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
                        .map(|(name, receipt)| match receipt {
                            Ok(status) => serde_json::json!({ "receiver": name, "status": status }),
                            Err(error) => serde_json::json!({ "receiver": name, "error": error }),
                        })
                        .collect::<Vec<_>>()
                }))
            }
            "compact.status" => {
                let commands = self.host_commands.as_ref().ok_or_else(|| {
                    mush_kernel::HostError("compaction is not available here".into())
                })?;
                let usage = commands.context_usage();
                Ok(serde_json::json!({
                    "tokens": usage.tokens,
                    "context_window": usage.context_window,
                    "percent": usage.percent(),
                    "scheduled": commands.compaction_queued(),
                }))
            }
            "compact.run" => {
                if !self.compaction_callable {
                    return Err(mush_kernel::HostError(
                        "compaction from the agent is turned off (compaction.agent_callable)"
                            .into(),
                    ));
                }
                let commands = self.host_commands.as_ref().ok_or_else(|| {
                    mush_kernel::HostError("compaction is not available here".into())
                })?;
                if commands.compaction_queued() {
                    return Ok(serde_json::json!({
                        "scheduled": false,
                        "note": "a compaction is already queued for the end of this turn",
                    }));
                }
                let focus = mush_session::compact::CompactionFocus {
                    context_budget: None,
                    instructions: str_field("instructions").filter(|text| !text.trim().is_empty()),
                };
                commands.push(ClientCommand::Compact { focus, auto: false });
                Ok(
                    serde_json::json!({ "scheduled": true, "note": "compaction runs after this turn ends" }),
                )
            }
            "harness.list"
            | "harness.get"
            | "harness.search"
            | "harness.overview"
            | "harness.create"
            | "harness.update"
            | "harness.delete"
            | "harness.record_refinement" => return self.harness_call(kind, &payload),
            "refine.run" | "refine.status" => {
                let commands = self.host_commands.as_ref().ok_or_else(|| {
                    mush_kernel::HostError("refinement is not available here".into())
                })?;
                let pending = commands.pending_refine();
                if kind == "refine.status" {
                    return Ok(serde_json::json!({ "pending": pending, "in_flight": false }));
                }
                let refine = mush_session::refine::RefineRequest {
                    instructions: str_field("instructions").filter(|text| !text.trim().is_empty()),
                    rollback: None,
                    global: payload
                        .get("global")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    source: mush_session::harness::RefinementSource::Agent,
                };
                commands.replace_refine(refine);
                Ok(
                    serde_json::json!({ "scheduled": true, "note": "refinement runs after this turn ends; applied edits reach your context as a refinement notice" }),
                )
            }
            "agent_observe.list_agents" => Ok(serde_json::json!({
                "agents": self.family.roster(&self.identity),
            })),
            "agent_observe.get_agent" => {
                let target = str_field("target")
                    .ok_or_else(|| mush_kernel::HostError("target is required".into()))?;
                let agent = self
                    .family
                    .get_agent(&self.identity, &target)
                    .map_err(mush_kernel::HostError)?;
                Ok(serde_json::json!({ "agent": agent }))
            }
            "agent_observe.recent_messages" => {
                let target = str_field("target")
                    .ok_or_else(|| mush_kernel::HostError("target is required".into()))?;
                let limit = payload
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(OBSERVE_DEFAULT_LIMIT, |value| value as usize);
                let max_chars = payload
                    .get("max_chars")
                    .and_then(serde_json::Value::as_u64)
                    .map_or(OBSERVE_DEFAULT_CHARS, |value| value as usize);
                let messages = self
                    .family
                    .recent_messages(&self.identity, &target, limit, max_chars)
                    .await
                    .map_err(mush_kernel::HostError)?;
                Ok(serde_json::json!({ "messages": messages }))
            }
            "rlm.create_session" => {
                if self.identity.depth > 0 {
                    return Err(mush_kernel::HostError(
                        "only the root agent starts new sessions; ask your parent".into(),
                    ));
                }
                let spawner = self.sessions.as_ref().ok_or_else(|| {
                    mush_kernel::HostError("new sessions are not available here".into())
                })?;
                let prompt = str_field("prompt")
                    .ok_or_else(|| mush_kernel::HostError("prompt is required".into()))?;
                let created = (spawner)(CreateSessionRequest {
                    prompt,
                    name: str_field("name"),
                    model: str_field("model"),
                    thinking: requested_thinking(str_field("thinking"))?,
                    cwd: str_field("cwd").map(PathBuf::from),
                })
                .await
                .map_err(mush_kernel::HostError)?;
                Ok(serde_json::to_value(created).unwrap_or(serde_json::Value::Null))
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
                let include_inactive = payload
                    .get("include_inactive")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let jobs: Vec<_> = jobs
                    .into_iter()
                    .filter(|job| {
                        include_inactive
                            || matches!(
                                job.status,
                                crate::schedule::JobStatus::Active
                                    | crate::schedule::JobStatus::Paused
                            )
                    })
                    .collect();
                Ok(serde_json::to_value(jobs).unwrap_or(serde_json::Value::Null))
            }
            "rlm_heartbeat.create" => {
                let (scheduler, _) = self.rlm_heartbeats()?;
                let prompt = str_field("instruction")
                    .or_else(|| str_field("prompt"))
                    .ok_or_else(|| mush_kernel::HostError("instruction is required".into()))?;
                let delivery = heartbeat_delivery(str_field("delivery_mode"))?;
                let expression =
                    heartbeat_interval(str_field("interval").unwrap_or_else(|| "5m".into()));
                let jobs = scheduler
                    .apply(crate::schedule::ScheduleCommand::Add {
                        expression,
                        prompt,
                        label: str_field("label"),
                        source: crate::schedule::JobSource::RlmHeartbeat,
                    })
                    .map_err(mush_kernel::HostError)?;
                let mut created = jobs
                    .iter()
                    .rev()
                    .find(|job| job.source == crate::schedule::JobSource::RlmHeartbeat)
                    .cloned();
                if let (Some(job), Some(delivery)) = (&created, delivery) {
                    let jobs = scheduler
                        .apply(crate::schedule::ScheduleCommand::Edit {
                            id: job.id.clone(),
                            prompt: None,
                            expression: None,
                            label: None,
                            delivery: Some(delivery),
                        })
                        .map_err(mush_kernel::HostError)?;
                    let id = job.id.clone();
                    created = jobs.into_iter().find(|job| job.id == id);
                }
                Ok(serde_json::to_value(created).unwrap_or(serde_json::Value::Null))
            }
            "rlm_heartbeat.update" | "rlm_heartbeat.delete" => {
                let (scheduler, jobs) = self.rlm_heartbeats()?;
                let id = str_field("id")
                    .ok_or_else(|| mush_kernel::HostError("id is required".into()))?;
                if !jobs.iter().any(|job| job.id == id) {
                    return Err(mush_kernel::HostError(format!("no rlm heartbeat {id}")));
                }
                let mut commands = Vec::new();
                if kind == "rlm_heartbeat.delete" {
                    commands.push(crate::schedule::ScheduleCommand::Cancel(id.clone()));
                } else {
                    let prompt = str_field("instruction").or_else(|| str_field("prompt"));
                    let expression = str_field("interval").map(heartbeat_interval);
                    let label = str_field("label");
                    let delivery = heartbeat_delivery(str_field("delivery_mode"))?;
                    if prompt.is_some()
                        || expression.is_some()
                        || label.is_some()
                        || delivery.is_some()
                    {
                        commands.push(crate::schedule::ScheduleCommand::Edit {
                            id: id.clone(),
                            prompt,
                            expression,
                            label,
                            delivery,
                        });
                    }
                    match str_field("status").as_deref() {
                        None => {}
                        Some("pause" | "paused") => {
                            commands.push(crate::schedule::ScheduleCommand::Pause(id.clone()));
                        }
                        Some("resume" | "active") => {
                            commands.push(crate::schedule::ScheduleCommand::Resume(id.clone()));
                        }
                        Some("cancel" | "cancelled" | "clear") => {
                            commands.push(crate::schedule::ScheduleCommand::Cancel(id.clone()));
                        }
                        Some(other) => {
                            return Err(mush_kernel::HostError(format!(
                                "status must be pause or resume, not {other}"
                            )));
                        }
                    }
                    if commands.is_empty() {
                        return Err(mush_kernel::HostError(
                            "update needs instruction, interval, label, status or delivery_mode"
                                .into(),
                        ));
                    }
                }
                let mut jobs = Vec::new();
                for command in commands {
                    jobs = scheduler.apply(command).map_err(mush_kernel::HostError)?;
                }
                let updated = jobs.into_iter().find(|job| job.id == id);
                Ok(serde_json::to_value(updated).unwrap_or(serde_json::Value::Null))
            }
            "goal.get" => Ok(self.goal_reply(false)),
            "goal.create" => {
                let objective = str_field("objective")
                    .ok_or_else(|| mush_kernel::HostError("objective is required".into()))?;
                let token_budget = match payload.get("token_budget") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(value) => {
                        Some(value.as_u64().filter(|budget| *budget > 0).ok_or_else(|| {
                            mush_kernel::HostError("token_budget must be a positive integer".into())
                        })?)
                    }
                };
                self.goal_store()?
                    .create_from_host(objective, token_budget)
                    .map_err(mush_kernel::HostError)?;
                Ok(self.goal_reply(false))
            }
            "goal.complete" => {
                self.goal_store()?
                    .apply(crate::goal::GoalCommand::Complete {
                        summary: str_field("summary"),
                    })
                    .map_err(mush_kernel::HostError)?;
                Ok(self.goal_reply(true))
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
                config.parent = Some(spec.parent_session_id.clone().into());
                let parent_session_id = spec.parent_session_id.clone();
                config.saver = Some(Arc::new(
                    move |snapshot: crate::snapshot::SessionSnapshot| {
                        assert_eq!(
                            snapshot.parent.as_ref().map(ToString::to_string).as_deref(),
                            Some(parent_session_id.as_str())
                        );
                    },
                ));
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
            &asked[0].messages[0],
            Message::User(user) if user.text().starts_with("[task from parent]\n\nreview the auth flow")
        ));
    }

    #[tokio::test]
    async fn pausing_with_no_targets_reaches_every_descendant() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 3, None);
        let root = Identity::root("root");
        let parent = family
            .spawn(&root, "one".into(), "alpha".into(), None, None, &family)
            .await
            .unwrap();
        let alpha = Identity {
            id: Some(parent.rlm_child_id.clone()),
            name: "alpha".into(),
            depth: 1,
        };
        let grandchild = family
            .spawn(&alpha, "two".into(), "beta".into(), None, None, &family)
            .await
            .unwrap();
        settle(&family, &root).await;

        let pause = mush_session::Pause {
            at: Timestamp::now(),
            reason: Some("rebuilding".into()),
            by: Some("root".into()),
        };
        let touched = family.set_paused(&root, &[], Some(&pause)).unwrap();
        let ids: Vec<&str> = touched.iter().map(|child| child.id.as_str()).collect();
        assert!(ids.contains(&parent.rlm_child_id.as_str()), "{ids:?}");
        assert!(ids.contains(&grandchild.rlm_child_id.as_str()), "{ids:?}");
        assert!(touched.iter().all(|child| child.paused.is_some()));

        family.set_paused(&root, &[], None).unwrap();
        assert!(family.all().iter().all(|child| child.paused.is_none()));
    }

    #[tokio::test]
    async fn pausing_a_child_that_is_not_yours_is_refused() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 3, None);
        let root = Identity::root("root");
        family
            .spawn(&root, "one".into(), "alpha".into(), None, None, &family)
            .await
            .unwrap();
        settle(&family, &root).await;
        let stranger = Identity {
            id: Some("sub-nope".into()),
            name: "stranger".into(),
            depth: 1,
        };
        assert!(
            family
                .set_paused(&stranger, &["alpha".to_string()], None)
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_roster_names_self_parent_siblings_and_children() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 3, None);
        let root = Identity::root("root");
        let first = family
            .spawn(&root, "one".into(), "alpha".into(), None, None, &family)
            .await
            .unwrap();
        family
            .spawn(&root, "two".into(), "beta".into(), None, None, &family)
            .await
            .unwrap();
        settle(&family, &root).await;
        let alpha = Identity {
            id: Some(first.rlm_child_id.clone()),
            name: "alpha".into(),
            depth: 1,
        };
        family
            .spawn(&alpha, "three".into(), "gamma".into(), None, None, &family)
            .await
            .unwrap();
        settle(&family, &alpha).await;

        let statuses: Vec<String> = family
            .roster(&root)
            .into_iter()
            .map(|view| view.status)
            .collect();
        assert_eq!(statuses, vec!["idle", "idle", "idle"]);
        family.set_root_running(true);
        assert_eq!(family.roster(&root)[0].status, "running");

        let mut root_view: Vec<(String, String)> = family
            .roster(&root)
            .into_iter()
            .map(|view| (view.relationship, view.name))
            .collect();
        root_view[1..].sort();
        assert_eq!(
            root_view,
            vec![
                ("self".to_string(), "root".to_string()),
                ("child".into(), "alpha".into()),
                ("child".into(), "beta".into()),
            ]
        );

        let gamma = family.get_agent(&alpha, "gamma").unwrap();
        assert_eq!(gamma.relationship, "child");
        assert_eq!(
            family.get_agent(&alpha, "root").unwrap().relationship,
            "parent"
        );
        assert!(family.get_agent(&root, "gamma").is_err());

        let mut alpha_view: Vec<(String, String)> = family
            .roster(&alpha)
            .into_iter()
            .map(|view| (view.relationship, view.name))
            .collect();
        alpha_view[1..].sort();
        assert_eq!(
            alpha_view,
            vec![
                ("self".to_string(), "alpha".to_string()),
                ("child".into(), "gamma".into()),
                ("parent".into(), "root".into()),
                ("sibling".into(), "beta".into()),
            ]
        );
    }

    #[tokio::test]
    async fn a_parent_reads_its_child_transcript_and_nothing_else() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 2, None);
        let root = Identity::root("root");
        let handle = family
            .spawn(
                &root,
                "review it".into(),
                "auth".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap();
        settle(&family, &root).await;

        let messages = family.recent_messages(&root, "auth", 8, 800).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert!(messages[0].text.starts_with("[task from parent]"));
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].text, "child says hi");

        let clipped = family
            .recent_messages(&root, &handle.rlm_child_id, 1, 80)
            .await
            .unwrap();
        assert_eq!(clipped.len(), 1);
        assert_eq!(clipped[0].role, "assistant");

        let refused = family.recent_messages(&root, "nobody", 8, 800).await;
        assert!(
            refused
                .unwrap_err()
                .contains("is not your parent, sibling or child"),
        );
    }

    #[tokio::test]
    async fn a_child_cannot_read_a_parent_without_a_live_session() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 2, None);
        let root = Identity::root("root");
        let handle = family
            .spawn(&root, "one".into(), "alpha".into(), None, None, &family)
            .await
            .unwrap();
        settle(&family, &root).await;
        let alpha = Identity {
            id: Some(handle.rlm_child_id),
            name: "alpha".into(),
            depth: 1,
        };

        let refused = family.recent_messages(&alpha, "root", 8, 800).await;
        assert_eq!(
            refused.unwrap_err(),
            "root has no live session to read".to_string()
        );

        let _inbox = family.root_observe_inbox();
        let view = family
            .roster(&alpha)
            .into_iter()
            .find(|view| view.relationship == "parent")
            .unwrap();
        assert!(view.observable);
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

        let long_name = "n".repeat(CHILD_NAME_MAX_CHARS + 1);
        let too_long = family
            .spawn(&root, "two".into(), long_name, None, None, &family)
            .await
            .unwrap_err();
        assert!(too_long.contains("at most 64"), "{too_long}");

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
    async fn a_child_that_finishes_without_replying_wakes_its_parent() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(Arc::clone(&requests), 2, None);
        let root = Identity::root("root");
        let (mut root_connection, _root_requests) = crate::testing::echo_connection();
        root_connection.attach_agent_inbox(family.root_inbox());
        family
            .spawn(&root, "task".into(), "auth".into(), None, None, &family)
            .await
            .unwrap();
        settle(&family, &root).await;

        let mut injected = Vec::new();
        for _ in 0..50 {
            root_connection.start_pending().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            while let Some(event) = root_connection.try_next_event() {
                if let ClientEvent::MessageInjected(event) = event {
                    injected.push(event.content);
                }
            }
        }
        assert_eq!(
            injected,
            vec![
                "[child-exited: no-reply child:auth]\n\nLast assistant text: child says hi"
                    .to_string()
            ]
        );
    }

    fn stall_factory(stalled: &'static str, release: Arc<tokio::sync::Notify>) -> ChildFactory {
        Arc::new(move |spec: ChildSpec, _family: Arc<Family>| {
            let release = Arc::clone(&release);
            Box::pin(async move {
                let mut registry = ApiRegistry::new();
                if spec.name == stalled {
                    registry.register(Box::new(crate::testing::Stall { release }));
                } else {
                    registry.register(Box::new(Echo {
                        requests: Arc::new(StdMutex::new(Vec::new())),
                        reply: format!("{} says hi", spec.name),
                    }));
                }
                Ok(ChildParts {
                    config: WorkerConfig::new(echo_config(echo_model())),
                    tools: ToolRegistry::new(),
                    registry,
                })
            })
        })
    }

    async fn wait_for(family: &Family, id: &str, status: ChildStatus) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while family
            .all()
            .iter()
            .find(|child| child.id == id)
            .is_none_or(|child| child.status != status)
        {
            assert!(Instant::now() < deadline, "{id} never reached {status:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn notices(inbox: &mut AgentInbox) -> Vec<String> {
        std::iter::from_fn(|| inbox.try_recv().ok())
            .map(|delivery| delivery.event.content)
            .collect()
    }

    #[tokio::test]
    async fn a_child_settles_only_when_its_own_children_are_quiet() {
        let release = Arc::new(tokio::sync::Notify::new());
        let family = Family::new(
            Identity::root("root"),
            "root".into(),
            3,
            Some(stall_factory("beta", Arc::clone(&release))),
            None,
        );
        let mut root_inbox = family.root_inbox();
        let root = Identity::root("root");
        let alpha = family
            .spawn(
                &root,
                "delegate".into(),
                "alpha".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap();
        let alpha_identity = Identity {
            id: Some(alpha.rlm_child_id.clone()),
            name: "alpha".into(),
            depth: 1,
        };
        let beta = family
            .spawn(
                &alpha_identity,
                "slow".into(),
                "beta".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap();
        wait_for(&family, &beta.rlm_child_id, ChildStatus::Running).await;

        let gamma = family
            .spawn(&root, "quick".into(), "gamma".into(), None, None, &family)
            .await
            .unwrap();
        assert!(family.descendants_running(Some(&alpha.rlm_child_id)));
        assert!(!family.descendants_running(Some(&gamma.rlm_child_id)));
        assert!(family.descendants_running(None));

        let early = family
            .collect(&root, vec!["alpha".into()], Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!(early[0].status, ChildStatus::Running);
        let mut seen = notices(&mut root_inbox);
        assert!(!seen.iter().any(|notice| notice.contains("child:alpha")));

        let deadline = Instant::now() + Duration::from_secs(5);
        while family.children_of(&alpha_identity)[0].status != ChildStatus::Idle {
            assert!(Instant::now() < deadline);
            release.notify_one();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let settled = settle(&family, &root).await;
        assert!(
            settled
                .iter()
                .all(|child| child.status == ChildStatus::Idle)
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.len() < 2 {
            assert!(Instant::now() < deadline);
            seen.extend(notices(&mut root_inbox));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            seen.contains(
                &"[child-exited: no-reply child:alpha]\n\nLast assistant text: alpha says hi"
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn a_reply_does_not_hide_a_later_failure() {
        let release = Arc::new(tokio::sync::Notify::new());
        let family = Family::new(
            Identity::root("root"),
            "root".into(),
            2,
            Some(stall_factory("slow", release)),
            None,
        );
        let mut root_inbox = family.root_inbox();
        let root = Identity::root("root");
        let slow = family
            .spawn(&root, "task".into(), "slow".into(), None, None, &family)
            .await
            .unwrap();
        wait_for(&family, &slow.rlm_child_id, ChildStatus::Running).await;
        if let Some(node) = family.lock().children.get_mut(&slow.rlm_child_id) {
            node.replied = true;
        }

        family.record(
            &slow.rlm_child_id,
            &ClientEvent::Agent {
                generation: crate::session::StreamGeneration(0),
                event: AgentEvent::Error {
                    error: "provider exploded".into(),
                },
                applied: crate::Applied::Nothing,
            },
            None,
        );
        assert_eq!(
            notices(&mut root_inbox),
            vec!["[child-failed child:slow]\n\nprovider exploded"]
        );
    }

    #[tokio::test]
    async fn deleting_a_running_child_says_why_it_was_cancelled() {
        let release = Arc::new(tokio::sync::Notify::new());
        let family = Family::new(
            Identity::root("root"),
            "root".into(),
            2,
            Some(stall_factory("slow", release)),
            None,
        );
        let mut root_inbox = family.root_inbox();
        let root = Identity::root("root");
        let slow = family
            .spawn(&root, "task".into(), "slow".into(), None, None, &family)
            .await
            .unwrap();
        wait_for(&family, &slow.rlm_child_id, ChildStatus::Running).await;

        family.delete(&root, "slow").await.unwrap();
        assert_eq!(
            notices(&mut root_inbox),
            vec!["[child-exited: cancelled child:slow]\n\nDeleted by parent orchestrator"]
        );
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
        let child = Identity {
            id: Some(family.children_of(&root)[0].id.clone()),
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
        assert_eq!(receipts, vec![("root".to_string(), Ok(Receipt::Delivered))]);
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

        settle(&family, &root).await;
        assert!(
            !std::iter::from_fn(|| root_connection.try_next_event()).any(|event| {
                matches!(event, ClientEvent::MessageInjected(event)
                    if event.content.starts_with("[child-exited"))
            }),
            "a child that replied gets no exit notice"
        );
    }

    #[tokio::test]
    async fn a_broadcast_reaches_idle_children_and_one_failure_spares_the_rest() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let root = Identity::root("root");
        for name in ["alpha", "beta"] {
            family
                .spawn(&root, "task".into(), name.into(), None, None, &family)
                .await
                .unwrap();
        }
        settle(&family, &root).await;
        let beta = family
            .children_of(&root)
            .into_iter()
            .find(|child| child.name == "beta")
            .unwrap();
        if let Some(node) = family.lock().children.get_mut(&beta.id) {
            node.inbox = None;
        }

        let mut receipts = family
            .send_message(&root, "all", None, "status please", DeliveryMode::Auto)
            .await
            .unwrap();
        receipts.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(receipts[0], ("alpha".to_string(), Ok(Receipt::Delivered)));
        assert_eq!(receipts[1].0, "beta");
        assert!(receipts[1].1.is_err());
    }

    #[test]
    fn the_message_bucket_allows_a_burst_then_one_a_second() {
        let start = Instant::now();
        let mut bucket = Bucket {
            tokens: MESSAGE_BURST,
            updated_at: start,
        };
        for _ in 0..MESSAGE_BURST {
            assert!(bucket.take(start).is_ok());
        }
        let retry = bucket.take(start).unwrap_err();
        assert!(retry <= MESSAGE_REFILL && retry > Duration::ZERO);
        assert!(bucket.take(start + MESSAGE_REFILL).is_ok());
        assert!(bucket.take(start + MESSAGE_REFILL).is_err());
    }

    #[tokio::test]
    async fn the_rlm_heartbeat_api_edits_deletes_and_lists_inactive_ones() {
        use mush_kernel::HostRequestHandler;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let scheduler = Arc::new(crate::schedule::Scheduler::new(None));
        let handler =
            RlmHandler::new(family, Identity::root("root")).with_scheduler(Arc::clone(&scheduler));
        let created = handler
            .handle(
                "rlm_heartbeat.create",
                serde_json::json!({
                    "instruction": "check ci",
                    "interval": "10m",
                    "delivery_mode": "follow_up",
                }),
            )
            .await
            .unwrap();
        let id = created["id"].as_str().unwrap().to_string();
        assert_eq!(created["delivery"], "follow_up");
        assert_eq!(created["expression"], "every 10m");

        let updated = handler
            .handle(
                "rlm_heartbeat.update",
                serde_json::json!({
                    "id": id,
                    "instruction": "check ci and docs",
                    "interval": "20m",
                    "label": "ci",
                    "status": "pause",
                    "delivery_mode": "steer",
                }),
            )
            .await
            .unwrap();
        assert_eq!(updated["prompt"], "check ci and docs");
        assert_eq!(updated["expression"], "every 20m");
        assert_eq!(updated["label"], "ci");
        assert_eq!(updated["status"], "paused");
        assert_eq!(updated["delivery"], "steer");

        handler
            .handle("rlm_heartbeat.delete", serde_json::json!({ "id": id }))
            .await
            .unwrap();
        let active = handler
            .handle("rlm_heartbeat.list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(active.as_array().unwrap().len(), 0);
        let all = handler
            .handle(
                "rlm_heartbeat.list",
                serde_json::json!({ "include_inactive": true }),
            )
            .await
            .unwrap();
        assert_eq!(all.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn spawn_refuses_an_unknown_thinking_level() {
        use mush_kernel::HostRequestHandler;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let handler = RlmHandler::new(Arc::clone(&family), Identity::root("root"));
        let refused = handler
            .handle(
                "rlm.spawn",
                serde_json::json!({ "prompt": "p", "name": "n", "thinking": "hgih" }),
            )
            .await
            .unwrap_err();
        assert!(refused.0.contains("not hgih"), "{}", refused.0);
        assert!(family.all().is_empty());
    }

    #[tokio::test]
    async fn goal_replies_carry_time_used_and_the_completion_report() {
        use mush_kernel::HostRequestHandler;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let goal = Arc::new(crate::goal::GoalStore::new(None));
        let handler = RlmHandler::new(family, Identity::root("root")).with_goal(Arc::clone(&goal));
        handler
            .handle(
                "goal.create",
                serde_json::json!({ "objective": "ship", "token_budget": 100 }),
            )
            .await
            .unwrap();
        assert!(
            handler
                .handle("goal.create", serde_json::json!({ "objective": "again" }))
                .await
                .is_err()
        );
        assert!(
            handler
                .handle(
                    "goal.create",
                    serde_json::json!({ "objective": "x", "token_budget": -5 })
                )
                .await
                .is_err()
        );
        let got = handler
            .handle("goal.get", serde_json::json!({}))
            .await
            .unwrap();
        assert!(got["goal"]["time_used_seconds"].is_u64());
        assert!(got["completion_budget_report"].is_null());
        let done = handler
            .handle("goal.complete", serde_json::json!({}))
            .await
            .unwrap();
        assert!(
            done["completion_budget_report"]
                .as_str()
                .unwrap()
                .contains("tokens used: 0 of 100")
        );
    }

    #[tokio::test]
    async fn the_kernel_cannot_fail_pause_resume_or_clear_a_goal() {
        use mush_kernel::HostRequestHandler;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let goal = Arc::new(crate::goal::GoalStore::new(None));
        let handler = RlmHandler::new(family, Identity::root("root")).with_goal(Arc::clone(&goal));
        handler
            .handle("goal.create", serde_json::json!({ "objective": "ship" }))
            .await
            .unwrap();
        for kind in ["goal.fail", "goal.pause", "goal.resume", "goal.clear"] {
            let refused = handler
                .handle(kind, serde_json::json!({ "error": "stuck" }))
                .await
                .unwrap_err();
            assert!(refused.0.contains("unknown host request"), "{}", refused.0);
        }
        assert_eq!(goal.get().unwrap().status, crate::goal::GoalStatus::Active);
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
        assert_eq!(receipts, vec![("slow".to_string(), Ok(Receipt::Delivered))]);

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
        assert_eq!(queued, vec![("slow".to_string(), Ok(Receipt::Queued))]);

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

        assert_eq!(family.progress_note(&child, "halfway"), Ok(None));
        let throttled = family.progress_note(&child, "nearly").unwrap();
        assert!(throttled.is_some_and(|retry| retry <= PROGRESS_NOTE_INTERVAL));
        let too_long = "x".repeat(PROGRESS_NOTE_CHARS + 1);
        assert!(family.progress_note(&child, &too_long).is_err());
        assert_eq!(family.progress_note(&root, "reading the results"), Ok(None));
        assert!(
            family
                .progress_note(&root, "still reading")
                .unwrap()
                .is_some()
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_spawns_with_one_name_admit_only_one() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let inner = echo_factory(requests, "hi");
        let factory: ChildFactory = Arc::new(move |spec: ChildSpec, family: Arc<Family>| {
            let made = inner(spec, family);
            Box::pin(async move {
                tokio::task::yield_now().await;
                made.await
            })
        });
        let family = Family::new(
            Identity::root("root"),
            "root".into(),
            2,
            Some(factory),
            None,
        );
        let root = Identity::root("root");
        let (first, second) = tokio::join!(
            family.spawn(&root, "one".into(), "a".into(), None, None, &family),
            family.spawn(&root, "two".into(), "a".into(), None, None, &family),
        );
        assert_eq!(
            [first.is_ok(), second.is_ok()]
                .iter()
                .filter(|ok| **ok)
                .count(),
            1
        );
        assert_eq!(family.children_of(&root).len(), 1);
    }

    #[tokio::test]
    async fn a_name_reused_after_delete_resolves_to_the_live_child() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let root = Identity::root("root");
        family
            .spawn(&root, "one".into(), "w".into(), None, None, &family)
            .await
            .unwrap();
        settle(&family, &root).await;
        family.delete(&root, "w").await.unwrap();
        let second = family
            .spawn(&root, "two".into(), "w".into(), None, None, &family)
            .await
            .unwrap();

        let listed = family.children_of(&root);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, second.rlm_child_id);
        let collected = family
            .collect(&root, vec!["w".into()], Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(collected[0].id, second.rlm_child_id);
        assert_eq!(
            family
                .roster(&root)
                .iter()
                .filter(|view| view.relationship == "child")
                .count(),
            1
        );
        assert_eq!(family.all().len(), 2);
        let deleted = family.delete(&root, "w").await.unwrap();
        assert_eq!(deleted.id, second.rlm_child_id);
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
                streaming: false,
                tool_running: None,
                awaiting_confirmation: None,
                paused: None,
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

    fn saved_child(id: &str, name: &str) -> ChildSummary {
        ChildSummary {
            id: id.into(),
            name: name.into(),
            parent: None,
            depth: 1,
            session_id: format!("session-{id}"),
            model_id: "m".into(),
            status: ChildStatus::Idle,
            spawned_at: Timestamp::zero(),
            updated_at: Timestamp::zero(),
            answer: None,
            error: None,
            progress_note: None,
            usage: Usage::default(),
            cost: Cost::default(),
            turns: 1,
            streaming: false,
            tool_running: None,
            awaiting_confirmation: None,
            paused: None,
        }
    }

    #[test]
    fn rebinding_swaps_in_the_new_sessions_children_and_leaves_the_old_registry_alone() {
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old/subagents.json");
        let new_path = dir.path().join("new/subagents.json");
        std::fs::create_dir_all(old_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        let write = |path: &std::path::Path, children: Vec<ChildSummary>| {
            std::fs::write(path, serde_json::to_vec(&Registry { children }).unwrap()).unwrap();
        };
        write(&old_path, vec![saved_child("sub-old", "old")]);
        write(&new_path, vec![saved_child("sub-new", "new")]);
        let family = Family::new(
            Identity::root("root"),
            "old".into(),
            2,
            None,
            Some(old_path.clone()),
        );

        family.rebind("new".into(), Some(new_path.clone()));

        let names: Vec<_> = family.all().into_iter().map(|child| child.name).collect();
        assert_eq!(names, vec!["new".to_string()]);
        let names_in = |path: &std::path::Path| -> Vec<String> {
            serde_json::from_slice::<Registry>(&std::fs::read(path).unwrap())
                .unwrap()
                .children
                .into_iter()
                .map(|child| child.name)
                .collect()
        };
        assert_eq!(names_in(&old_path), vec!["old".to_string()]);
        assert_eq!(names_in(&new_path), vec!["new".to_string()]);

        family.rebind(
            "fresh".into(),
            Some(dir.path().join("fresh/subagents.json")),
        );

        assert!(family.all().is_empty());
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

    #[tokio::test]
    async fn compact_status_reports_usage_and_run_takes_instructions() {
        use mush_kernel::HostRequestHandler;

        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let commands = Arc::new(crate::connection::HostCommands::default());
        commands.record_context(crate::connection::ContextUsage {
            tokens: Some(50_000),
            context_window: Some(200_000),
        });
        let handler = RlmHandler::new(Arc::clone(&family), Identity::root("root"))
            .with_host_commands(Arc::clone(&commands));

        let before = handler
            .handle("compact.status", serde_json::json!({}))
            .await
            .unwrap();
        handler
            .handle(
                "compact.run",
                serde_json::json!({ "instructions": "keep the plan" }),
            )
            .await
            .unwrap();
        let after = handler
            .handle("compact.status", serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(
            before,
            serde_json::json!({
                "tokens": 50_000,
                "context_window": 200_000,
                "percent": 25.0,
                "scheduled": false,
            })
        );
        assert_eq!(after["scheduled"], true);
        assert!(commands.compaction_queued());
    }

    #[tokio::test]
    async fn the_harness_crud_reaches_the_store_and_reports_what_it_wrote() {
        use mush_kernel::HostRequestHandler;

        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let dir = tempfile::tempdir().unwrap();
        let handler = RlmHandler::new(Arc::clone(&family), Identity::root("root")).with_harness(
            Arc::new(crate::harness::HarnessStore::with_dirs(
                dir.path().join("local"),
                dir.path().join("global"),
            )),
        );

        let created = handler
            .handle(
                "harness.create",
                serde_json::json!({
                    "kind": "memory",
                    "title": "the worktree gate",
                    "content": "cargo nextest run --workspace",
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["entry"]["id"], "the_worktree_gate");
        assert_eq!(created["entry"]["scope"], "local");
        assert_eq!(created["entry"]["source"], "agent");

        let found = handler
            .handle(
                "harness.get",
                serde_json::json!({ "kind": "memory", "id": "local:the_worktree_gate" }),
            )
            .await
            .unwrap();
        assert_eq!(found["entry"]["content"], "cargo nextest run --workspace");

        let hits = handler
            .handle("harness.search", serde_json::json!({ "query": "worktree" }))
            .await
            .unwrap();
        assert_eq!(hits["entries"].as_array().unwrap().len(), 1);

        let updated = handler
            .handle(
                "harness.update",
                serde_json::json!({
                    "kind": "memory",
                    "id": "the_worktree_gate",
                    "content": "cargo nextest run --workspace, then cargo shear",
                }),
            )
            .await
            .unwrap();
        assert_eq!(updated["entry"]["version"], 2);
        assert_eq!(updated["entry"]["title"], "the worktree gate");

        let deleted = handler
            .handle(
                "harness.delete",
                serde_json::json!({ "kind": "memory", "id": "the_worktree_gate" }),
            )
            .await
            .unwrap();
        assert_eq!(deleted["deleted"], true);
        let gone = handler
            .handle("harness.list", serde_json::json!({}))
            .await
            .unwrap();
        assert!(gone["entries"].as_array().unwrap().is_empty());

        let bad_kind = handler
            .handle("harness.list", serde_json::json!({ "kind": "recipe" }))
            .await
            .unwrap_err();
        assert!(bad_kind.0.contains("kind must be"));

        let recorded = handler
            .handle(
                "harness.record_refinement",
                serde_json::json!({
                    "trigger": "the gate moved",
                    "changes": "update memory:the_worktree_gate",
                    "outcome": "the next run reads the right command",
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            recorded["refinement"]["changes"][0],
            "update memory:the_worktree_gate"
        );
        let overview = handler
            .handle("harness.overview", serde_json::json!({}))
            .await
            .unwrap();
        assert!(
            overview["overview"]
                .as_str()
                .unwrap()
                .contains("the gate moved")
        );
    }

    #[tokio::test]
    async fn create_session_goes_through_the_spawner_and_only_from_the_root() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let spawner: SessionSpawner = Arc::new(|request: CreateSessionRequest| {
            Box::pin(async move {
                assert_eq!(request.thinking, Some(ThinkingLevel::High));
                assert_eq!(request.cwd.as_deref(), Some(std::path::Path::new("/tmp")));
                Ok(CreatedSession {
                    session_id: "deadbeef".into(),
                    name: request.name,
                    model: request.model.unwrap_or_else(|| "default".into()),
                })
            })
        });
        let root = RlmHandler::new(Arc::clone(&family), Identity::root("root"))
            .with_session_spawner(Arc::clone(&spawner));
        let child = RlmHandler::new(
            Arc::clone(&family),
            Identity {
                id: Some("sub-1".into()),
                name: "kid".into(),
                depth: 1,
            },
        )
        .with_session_spawner(spawner);
        use mush_kernel::HostRequestHandler;

        let created = root
            .handle(
                "rlm.create_session",
                serde_json::json!({
                    "prompt": "audit the deps",
                    "name": "deps",
                    "thinking": "high",
                    "cwd": "/tmp",
                }),
            )
            .await
            .unwrap();
        assert_eq!(created["session_id"], "deadbeef");
        assert_eq!(created["name"], "deps");
        assert_eq!(created["model"], "default");

        let refused = child
            .handle(
                "rlm.create_session",
                serde_json::json!({ "prompt": "audit the deps" }),
            )
            .await
            .unwrap_err();
        assert!(refused.0.contains("only the root agent"));
    }

    #[tokio::test]
    async fn a_child_reports_the_tool_it_is_running_and_what_it_waits_on() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let root = Identity::root("root");
        family
            .spawn(&root, "task".into(), "scout".into(), None, None, &family)
            .await
            .unwrap();
        let id = settle(&family, &root).await[0].id.clone();

        family.record(
            &id,
            &ClientEvent::Agent {
                generation: crate::StreamGeneration(1),
                event: mush_agent::AgentEvent::ToolExecStart {
                    tool_call_id: mush_ai::types::ToolCallId::from("call_1"),
                    tool_name: "bash".into(),
                    args: serde_json::json!({ "command": "sleep 600" }),
                },
                applied: crate::Applied::Nothing,
            },
            None,
        );
        assert_eq!(
            family.children_of(&root)[0].tool_running.as_deref(),
            Some("bash: sleep 600")
        );

        family.record(
            &id,
            &ClientEvent::ConfirmRequest {
                tool_call_id: mush_ai::types::ToolCallId::from("call_2"),
                prompt: "run `rm -rf build`?".into(),
            },
            None,
        );
        assert_eq!(
            family.children_of(&root)[0]
                .awaiting_confirmation
                .as_deref(),
            Some("run `rm -rf build`?")
        );

        family.record(&id, &ClientEvent::TurnEnded { cancelled: false }, None);
        let settled = family.children_of(&root).remove(0);
        assert!(settled.tool_running.is_none());
        assert!(settled.awaiting_confirmation.is_none());
        assert!(!settled.streaming);
    }
}
