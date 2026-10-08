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
use crate::worker::WorkerConfig;

mod host;
pub use host::RlmHandler;

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
const STOPPED_REASON: &str = "Stopped by the user";
const TICK: Duration = Duration::from_millis(50);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(5);
pub const HIBERNATE_AFTER: Duration = Duration::from_secs(30 * 60);

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
    pub parent_thinking: Option<ThinkingLevel>,
    pub resume_session: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Working,
    Napping,
    Hibernating,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildSummary {
    pub id: ChildId,
    pub name: String,
    pub parent: Option<ChildId>,
    pub depth: u32,
    pub session_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    pub status: ChildStatus,
    pub spawned_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default)]
    pub idle_since: Timestamp,
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

impl ChildSummary {
    #[must_use]
    pub fn activity(&self, now: Timestamp) -> Activity {
        let idle_for = now.as_ms().saturating_sub(self.idle_since.as_ms());
        match self.status {
            ChildStatus::Starting | ChildStatus::Running => Activity::Working,
            ChildStatus::Idle if u128::from(idle_for) < HIBERNATE_AFTER.as_millis() => {
                Activity::Napping
            }
            ChildStatus::Idle
            | ChildStatus::Failed
            | ChildStatus::Cancelled
            | ChildStatus::Interrupted
            | ChildStatus::Deleted => Activity::Hibernating,
        }
    }
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

pub const MESSAGE_SOURCE: &str = "agent_message";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    TopLevel,
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageTarget {
    pub session_id: String,
    pub session_name: String,
    pub runtime_kind: RuntimeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageReceipt {
    pub id: String,
    pub source: &'static str,
    pub target: MessageTarget,
    pub message: String,
    pub delivery_status: Receipt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
    pub delivery_mode: &'static str,
}

struct Live {
    inbox: mpsc::UnboundedSender<Delivery>,
    observe: mpsc::UnboundedSender<ObserveRequest>,
    control: mpsc::UnboundedSender<ClientCommand>,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

pub const CHILD_INTERRUPTED: &str = "The parent session restarted while this agent was working. \
The saved transcript was recovered, but unfinished model, tool, bash or child-agent work was not \
replayed. Inspect external side effects before continuing.";

struct Target {
    child: Option<ChildId>,
    endpoint: MessageTarget,
    sender: String,
}

pub struct Broadcast {
    pub receipts: Vec<(MessageTarget, Result<MessageReceipt, String>)>,
    pub hibernating: usize,
}

pub struct Delivery {
    pub event: EventMessage,
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
    pub state: Activity,
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

impl Node {
    fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if !self.summary.status.settled() {
            self.summary.status = ChildStatus::Cancelled;
        }
    }
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
    me: std::sync::Weak<Family>,
    inner: Mutex<Inner>,
    factory: Option<ChildFactory>,
    registry_path: Mutex<Option<PathBuf>>,
    // held from snapshot to write and over a rebind, so the file never goes back in time
    // or pairs one session's path with another's children
    persisting: Mutex<()>,
    persist_scheduled: std::sync::atomic::AtomicBool,
    revision: std::sync::atomic::AtomicU64,
    max_depth: std::sync::atomic::AtomicU32,
    waking: tokio::sync::Mutex<()>,
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
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
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
            persisting: Mutex::new(()),
            persist_scheduled: std::sync::atomic::AtomicBool::new(false),
            revision: std::sync::atomic::AtomicU64::new(0),
            max_depth: std::sync::atomic::AtomicU32::new(max_depth),
            waking: tokio::sync::Mutex::new(()),
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
            node.stop();
        }
        self.persist();
        let children = load_registry(registry_path.as_deref());
        let _persisting = self
            .persisting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let mut inner = self.lock();
            inner.root_session_id = session_id;
            inner.children = children;
        }
        self.bump_revision();
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

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Acquire)
    }

    fn bump_revision(&self) {
        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn persist(&self) {
        self.bump_revision();
        let _persisting = self
            .persisting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    fn persist_soon(&self) {
        use std::sync::atomic::Ordering;
        self.bump_revision();
        if self.persist_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let (Ok(runtime), Some(family)) =
            (tokio::runtime::Handle::try_current(), self.me.upgrade())
        else {
            self.persist_scheduled.store(false, Ordering::Release);
            self.persist();
            return;
        };
        runtime.spawn_blocking(move || {
            family.persist_scheduled.store(false, Ordering::Release);
            family.persist();
        });
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

    pub fn map_children<T>(&self, row: impl FnMut(&ChildSummary) -> Option<T>) -> Vec<T> {
        self.lock()
            .children
            .values()
            .map(|node| &node.summary)
            .filter_map(row)
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

    #[must_use]
    pub fn roster(&self, caller: &Identity) -> Vec<AgentView> {
        let inner = self.lock();
        let mut views = Vec::new();
        match &caller.id {
            None => views.push(root_view(&inner, "self", false)),
            Some(id) => {
                let me = inner.children.get(id);
                if let Some(node) = me {
                    views.push(view_of(&inner, node, "self", false));
                }
                let parent = me.and_then(|node| node.summary.parent.clone());
                match &parent {
                    None => views.push(root_view(&inner, "parent", inner.root_observe.is_some())),
                    Some(parent_id) => {
                        if let Some(node) = inner.children.get(parent_id) {
                            views.push(view_of(&inner, node, "parent", node.observe.is_some()));
                        }
                    }
                }
                for node in inner.children.values() {
                    if &node.summary.id != id
                        && node.summary.parent == parent
                        && node.summary.status != ChildStatus::Deleted
                    {
                        views.push(view_of(&inner, node, "sibling", node.observe.is_some()));
                    }
                }
            }
        }
        for node in inner.children.values() {
            if node.summary.parent == caller.id && node.summary.status != ChildStatus::Deleted {
                views.push(view_of(&inner, node, "child", node.observe.is_some()));
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

    pub fn stop_session(&self, session_id: &str) -> Result<ChildSummary, String> {
        let id = self
            .lock()
            .children
            .values()
            .find(|node| {
                node.summary.session_id == session_id && node.summary.status != ChildStatus::Deleted
            })
            .map(|node| node.summary.id.clone())
            .ok_or_else(|| format!("no subagent has session {session_id}"))?;
        self.cancel(&id, STOPPED_REASON)?;
        self.lock()
            .children
            .get(&id)
            .map(|node| node.summary.clone())
            .ok_or_else(|| format!("{id} vanished"))
    }

    fn cancel(&self, id: &str, reason: &str) -> Result<(), String> {
        let (shutdown, running) = {
            let mut inner = self.lock();
            let Some(node) = inner.children.get_mut(id) else {
                return Err(format!("{id} vanished"));
            };
            let running = !node.summary.status.settled();
            if running {
                node.cancel_reason = Some(reason.into());
            }
            (node.shutdown.take(), running)
        };
        if let Some(shutdown) = shutdown {
            let _ = shutdown.send(());
        }
        self.update(id, |child| child.status = ChildStatus::Deleted);
        if running {
            self.post_exit_notice(id, ExitNotice::Cancelled);
        }
        Ok(())
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
        let (parent_session_id, parent_thinking) = {
            let inner = self.lock();
            match caller.id.as_deref() {
                Some(id) => inner
                    .children
                    .get(id)
                    .map(|node| {
                        (
                            node.summary.session_id.clone(),
                            node.summary.thinking.clone(),
                        )
                    })
                    .unwrap_or_default(),
                None => (inner.root_session_id.clone(), None),
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
            parent_thinking,
            resume_session: None,
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
            thinking: Some(parts.config.thinking_level.clone()),
            status: ChildStatus::Starting,
            spawned_at: now,
            updated_at: now,
            idle_since: Timestamp::zero(),
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
        let live = Self::launch(this, &id, parts, Some(format!("{TASK_LABEL}\n\n{prompt}")));
        let mut inner = self.lock();
        inner.reserved_names.remove(&reservation);
        inner.children.insert(
            id.clone(),
            Node {
                summary,
                inbox: Some(live.inbox),
                observe: Some(live.observe),
                control: Some(live.control),
                shutdown: Some(live.shutdown),
                task: Some(live.task),
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

    fn launch(this: &Arc<Self>, id: &str, parts: ChildParts, first_prompt: Option<String>) -> Live {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut connection = parts.config.connect(&parts.tools, &parts.registry);
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        connection.attach_agent_inbox(inbox_rx);
        let (observe_tx, observe_rx) = mpsc::unbounded_channel();
        connection.attach_observe_inbox(observe_rx);
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(drive_child(
            Arc::clone(this),
            id.to_string(),
            connection,
            shutdown_rx,
            control_rx,
            first_prompt,
        ));
        Live {
            inbox: inbox_tx,
            observe: observe_tx,
            control: control_tx,
            shutdown: shutdown_tx,
            task,
        }
    }

    fn asleep(&self, id: &str) -> bool {
        self.lock()
            .children
            .get(id)
            .is_some_and(|node| node.inbox.is_none() && node.summary.status != ChildStatus::Deleted)
    }

    async fn wake(&self, id: &str, this: &Arc<Self>) -> Result<(), String> {
        let _waking = self.waking.lock().await;
        if !self.asleep(id) {
            return Ok(());
        }
        let factory = self
            .factory
            .clone()
            .ok_or_else(|| format!("{id} is no longer running"))?;
        let (spec, interrupted) = {
            let inner = self.lock();
            let node = inner
                .children
                .get(id)
                .ok_or_else(|| format!("{id} is not in the family"))?;
            let summary = &node.summary;
            let (parent_name, parent_depth, parent_session_id, parent_thinking) =
                match &summary.parent {
                    Some(parent) => inner
                        .children
                        .get(parent)
                        .map(|parent| {
                            (
                                parent.summary.name.clone(),
                                parent.summary.depth,
                                parent.summary.session_id.clone(),
                                parent.summary.thinking.clone(),
                            )
                        })
                        .unwrap_or_default(),
                    None => (
                        inner.root.name.clone(),
                        inner.root.depth,
                        inner.root_session_id.clone(),
                        None,
                    ),
                };
            let spec = ChildSpec {
                id: summary.id.clone(),
                name: summary.name.clone(),
                prompt: String::new(),
                model: Some(summary.model_id.clone()),
                thinking: summary.thinking.clone(),
                depth: summary.depth,
                parent: Identity {
                    id: summary.parent.clone(),
                    name: parent_name,
                    depth: parent_depth,
                },
                parent_session_id,
                parent_thinking,
                resume_session: Some(summary.session_id.clone()),
            };
            (spec, summary.status == ChildStatus::Interrupted)
        };
        let mut parts = (factory)(spec, Arc::clone(this)).await?;
        if interrupted {
            parts.config.recovery_notice = Some(CHILD_INTERRUPTED.into());
        }
        let live = Self::launch(this, id, parts, None);
        {
            let mut inner = self.lock();
            let Some(node) = inner.children.get_mut(id) else {
                let _ = live.shutdown.send(());
                return Err(format!("{id} is not in the family"));
            };
            node.inbox = Some(live.inbox);
            node.observe = Some(live.observe);
            node.control = Some(live.control);
            node.shutdown = Some(live.shutdown);
            node.task = Some(live.task);
            node.replied = false;
            node.notified = false;
            node.awaiting_quiet = false;
            node.cancel_reason = None;
            node.summary.status = ChildStatus::Idle;
            node.summary.error = None;
            node.summary.updated_at = Timestamp::now();
        }
        self.persist();
        Ok(())
    }

    fn update(&self, id: &str, apply: impl FnOnce(&mut ChildSummary)) {
        self.update_then(id, apply, Self::persist);
    }

    fn record_update(&self, id: &str, apply: impl FnOnce(&mut ChildSummary)) {
        self.update_then(id, apply, Self::persist_soon);
    }

    fn update_then(&self, id: &str, apply: impl FnOnce(&mut ChildSummary), persist: fn(&Self)) {
        let settled_parent = {
            let mut inner = self.lock();
            let Some(node) = inner.children.get_mut(id) else {
                return;
            };
            let was = node.summary.status;
            apply(&mut node.summary);
            let now = Timestamp::now();
            node.summary.updated_at = now;
            if node.summary.status == ChildStatus::Idle && was != ChildStatus::Idle {
                node.summary.idle_since = now;
            }
            let was_settled = was.settled();
            (!was_settled && node.summary.status.settled())
                .then(|| node.summary.parent.clone())
                .flatten()
        };
        persist(self);
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
                self.record_update(id, |child| {
                    child.status = ChildStatus::Running;
                    child.turns += 1;
                });
            }
            ClientEvent::Agent {
                event: AgentEvent::MessageStart { .. },
                ..
            } => self.record_update(id, |child| child.streaming = true),
            ClientEvent::Agent {
                event:
                    AgentEvent::ToolExecStart {
                        tool_name, args, ..
                    },
                ..
            } => {
                let label = tool_label(tool_name, args);
                self.record_update(id, |child| {
                    child.awaiting_confirmation = None;
                    child.tool_running = Some(label);
                });
            }
            ClientEvent::Agent {
                event: AgentEvent::ToolExecEnd { .. },
                ..
            } => self.record_update(id, |child| child.tool_running = None),
            ClientEvent::ConfirmRequest { prompt, .. } => {
                let prompt = prompt.clone();
                self.record_update(id, |child| child.awaiting_confirmation = Some(prompt));
            }
            ClientEvent::ToolsCancelled(_) => self.record_update(id, |child| {
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
                self.record_update(id, |child| {
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
                self.record_update(id, |child| {
                    child.status = ChildStatus::Failed;
                    child.error = Some(error.clone());
                });
                self.post_exit_notice(id, ExitNotice::Failed);
            }
            ClientEvent::TurnEnded { cancelled } => {
                let busy = subtree_busy(&self.lock(), id);
                let mut ended = None;
                self.record_update(id, |child| {
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
        self.cancel(&id, DELETED_REASON)?;
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
            self.update_then(&id, |child| child.paused = pause.cloned(), |_| {});
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
        self.persist_soon();
        Ok(None)
    }

    fn sender_label(&self, caller: &Identity, relationship: &str) -> String {
        format!("{relationship}:{}", caller.name)
    }

    fn endpoint(inner: &Inner, id: Option<&str>) -> MessageTarget {
        match id.and_then(|id| inner.children.get(id)) {
            Some(node) => MessageTarget {
                session_id: node.summary.session_id.clone(),
                session_name: node.summary.name.clone(),
                runtime_kind: RuntimeKind::Subagent,
            },
            None => MessageTarget {
                session_id: inner.root_session_id.clone(),
                session_name: inner.root.name.clone(),
                runtime_kind: RuntimeKind::TopLevel,
            },
        }
    }

    fn checked_text(text: &str) -> Result<&str, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("message is required".into());
        }
        if text.chars().count() > MESSAGE_MAX_CHARS {
            return Err(format!("message is over {MESSAGE_MAX_CHARS} characters"));
        }
        Ok(text)
    }

    fn direct_target(
        &self,
        caller: &Identity,
        receiver_role: &str,
        receiver_name: Option<&str>,
    ) -> Result<Target, String> {
        let receiver_name = receiver_name.map(str::trim);
        match receiver_role {
            "parent" => {
                if receiver_name.is_some() {
                    return Err("receiver_name must be omitted for parent messages".into());
                }
                let id = caller
                    .id
                    .as_deref()
                    .ok_or_else(|| "the root agent has no parent".to_string())?;
                let mut inner = self.lock();
                let node = inner
                    .children
                    .get_mut(id)
                    .ok_or_else(|| format!("{id} is not in the family"))?;
                node.replied = true;
                let parent = node.summary.parent.clone();
                Ok(Target {
                    endpoint: Self::endpoint(&inner, parent.as_deref()),
                    child: parent,
                    sender: self.sender_label(caller, "child"),
                })
            }
            "child" => {
                let name = receiver_name
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        "receiver_name is required for sibling and child messages".to_string()
                    })?;
                let id = self.resolve(caller, name)?;
                Ok(Target {
                    endpoint: Self::endpoint(&self.lock(), Some(&id)),
                    child: Some(id),
                    sender: self.sender_label(caller, "parent"),
                })
            }
            "sibling" => {
                let name = receiver_name
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        "receiver_name is required for sibling and child messages".to_string()
                    })?;
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
                    .ok_or_else(|| format!("no sibling of yours is called {name}"))?
                    .summary
                    .id
                    .clone();
                Ok(Target {
                    endpoint: Self::endpoint(&inner, Some(&sibling)),
                    child: Some(sibling),
                    sender: self.sender_label(caller, "sibling"),
                })
            }
            _ => Err(r#"receiver_role must be "parent", "sibling", or "child""#.into()),
        }
    }

    fn broadcast_targets(&self, caller: &Identity) -> Result<(Vec<Target>, usize), String> {
        let now = Timestamp::now();
        let mut inner = self.lock();
        let mut targets = Vec::new();
        let mut siblings_of = None;
        if let Some(my_id) = caller.id.as_deref() {
            let node = inner
                .children
                .get_mut(my_id)
                .ok_or_else(|| format!("{my_id} is not in the family"))?;
            node.replied = true;
            let my_parent = node.summary.parent.clone();
            targets.push(Target {
                endpoint: Self::endpoint(&inner, my_parent.as_deref()),
                child: my_parent.clone(),
                sender: self.sender_label(caller, "child"),
            });
            siblings_of = Some((my_id, my_parent));
        }
        let mut hibernating = 0;
        let mut reach = |node: &Node, role: &str| {
            if node.summary.activity(now) == Activity::Hibernating {
                hibernating += 1;
            } else {
                targets.push(Target {
                    endpoint: Self::endpoint(&inner, Some(&node.summary.id)),
                    child: Some(node.summary.id.clone()),
                    sender: self.sender_label(caller, role),
                });
            }
        };
        if let Some((my_id, my_parent)) = &siblings_of {
            for node in inner.children.values() {
                if node.summary.parent == *my_parent
                    && node.summary.id != *my_id
                    && node.summary.status != ChildStatus::Deleted
                {
                    reach(node, "sibling");
                }
            }
        }
        for node in inner.children.values() {
            if node.summary.parent == caller.id && node.summary.status != ChildStatus::Deleted {
                reach(node, "parent");
            }
        }
        Ok((targets, hibernating))
    }

    pub async fn send_message(
        &self,
        caller: &Identity,
        receiver_role: &str,
        receiver_name: Option<&str>,
        text: &str,
        this: &Arc<Self>,
    ) -> Result<MessageReceipt, String> {
        let text = Self::checked_text(text)?;
        let target = self.direct_target(caller, receiver_role, receiver_name)?;
        self.deliver(caller, &target, text, this).await
    }

    pub async fn broadcast(
        &self,
        caller: &Identity,
        text: &str,
        this: &Arc<Self>,
    ) -> Result<Broadcast, String> {
        let text = Self::checked_text(text)?;
        let (targets, hibernating) = self.broadcast_targets(caller)?;
        let sends = targets
            .iter()
            .map(|target| self.deliver(caller, target, text, this));
        let results = futures::future::join_all(sends).await;
        Ok(Broadcast {
            receipts: targets
                .into_iter()
                .map(|target| target.endpoint)
                .zip(results)
                .collect(),
            hibernating,
        })
    }

    async fn deliver(
        &self,
        caller: &Identity,
        target: &Target,
        text: &str,
        this: &Arc<Self>,
    ) -> Result<MessageReceipt, String> {
        let timestamp_ms = Timestamp::now();
        let (sender_session, bucket_key) = {
            let mut inner = self.lock();
            let sender_session = Self::endpoint(&inner, caller.id.as_deref()).session_id;
            let key = (sender_session.clone(), target.endpoint.session_id.clone());
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
        let id = format!("agentmsg_{}", mush_ai::types::SessionId::new());
        let event = EventMessage {
            content: format!("[agent-message from {}]\n\n{text}", target.sender),
            timestamp_ms,
            kind: EventKind::Message,
            receipts: vec![EventReceipt {
                id: id.clone().into(),
                timestamp_ms,
                kind: EventKind::Message,
                origin: EventOrigin::Agent {
                    session_id: sender_session,
                    name: caller.name.clone(),
                    relationship: target
                        .sender
                        .split_once(':')
                        .map(|(relationship, _)| relationship.to_string())
                        .unwrap_or_default(),
                },
            }],
        };
        let woken = match target.child.as_deref() {
            Some(child) if self.asleep(child) => self.wake(child, this).await,
            _ => Ok(()),
        };
        let delivered = match woken {
            Ok(()) => match self.send_delivery(target.child.as_deref(), event).await {
                Ok(Receipt::Rejected) => Err(format!(
                    "{} has {MAX_PENDING_MESSAGES} messages waiting already",
                    target.endpoint.session_name
                )),
                other => other,
            },
            Err(error) => Err(error),
        };
        let status = match delivered {
            Ok(status) => status,
            Err(error) => {
                if let Some(bucket) = self.lock().message_buckets.get_mut(&bucket_key) {
                    bucket.tokens = (bucket.tokens + 1).min(MESSAGE_BURST);
                }
                return Err(error);
            }
        };
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let (delivered_at, queued_at) = match status {
            Receipt::Delivered => (Some(at), None),
            Receipt::Queued | Receipt::Rejected => (None, Some(at)),
        };
        Ok(MessageReceipt {
            id,
            source: MESSAGE_SOURCE,
            target: target.endpoint.clone(),
            message: text.to_string(),
            delivery_status: status,
            delivered_at,
            queued_at,
            delivery_mode: "steer",
        })
    }

    async fn send_delivery(
        &self,
        target: Option<&str>,
        event: EventMessage,
    ) -> Result<Receipt, String> {
        let (receipt_tx, receipt_rx) = oneshot::channel();
        let delivery = Delivery {
            event,
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
                    node.stop();
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

fn root_view(inner: &Inner, relationship: &str, observable: bool) -> AgentView {
    AgentView {
        name: inner.root.name.clone(),
        relationship: relationship.to_string(),
        session_id: inner.root_session_id.clone(),
        rlm_child_id: None,
        status: root_status(inner).into(),
        state: root_activity(inner),
        observable,
        model_id: None,
        progress_note: inner.root_note.clone(),
        streaming: false,
        tool_running: None,
        awaiting_confirmation: None,
        children_running: running_children(inner, &None),
    }
}

fn view_of(inner: &Inner, node: &Node, relationship: &str, observable: bool) -> AgentView {
    let summary = &node.summary;
    let children_running = running_children(inner, &Some(summary.id.clone()));
    AgentView {
        name: summary.name.clone(),
        relationship: relationship.to_string(),
        session_id: summary.session_id.clone(),
        rlm_child_id: Some(summary.id.clone()),
        status: family_status(summary.status, node.inbox.is_some()).into(),
        state: summary.activity(Timestamp::now()),
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

fn root_activity(inner: &Inner) -> Activity {
    if inner.root_running {
        Activity::Working
    } else {
        Activity::Napping
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
    first_prompt: Option<String>,
) {
    let model = connection.default_model();
    if let Some(text) = first_prompt {
        match connection
            .send(ClientCommand::Prompt {
                text,
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
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
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
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
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
    async fn stopping_a_child_by_session_cancels_only_that_child() {
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
        let other = family
            .spawn(&root, "task".into(), "other".into(), None, None, &family)
            .await
            .unwrap();
        wait_for(&family, &slow.rlm_child_id, ChildStatus::Running).await;

        let stopped = family.stop_session(&slow.session_id).unwrap();
        assert_eq!(stopped.status, ChildStatus::Deleted);
        assert!(
            notices(&mut root_inbox)
                .contains(&"[child-exited: cancelled child:slow]\n\nStopped by the user".into())
        );
        let survivor = family
            .all()
            .into_iter()
            .find(|child| child.id == other.rlm_child_id)
            .unwrap();
        assert_ne!(survivor.status, ChildStatus::Deleted);
        assert!(family.stop_session(&slow.session_id).is_err());
        assert!(family.stop_session("no-such-session").is_err());
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
            family.send_message(&child, "parent", None, "found two issues", &family),
            async {
                for _ in 0..50 {
                    root_connection.start_pending().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        );

        let receipt = receipts.unwrap();
        assert_eq!(receipt.delivery_status, Receipt::Delivered);
        assert_eq!(receipt.target.session_name, "root");
        assert_eq!(receipt.target.runtime_kind, RuntimeKind::TopLevel);
        assert!(receipt.id.starts_with("agentmsg_"));
        assert!(receipt.delivered_at.is_some() && receipt.queued_at.is_none());
        let json = serde_json::to_value(&receipt).unwrap();
        assert_eq!(json["deliveryStatus"], "delivered");
        assert_eq!(json["target"]["sessionName"], "root");
        assert_eq!(json["deliveryMode"], "steer");
        assert!(json["deliveredAt"].is_string());
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
            let (closed, _) = mpsc::unbounded_channel();
            node.inbox = Some(closed);
        }

        let mut receipts = family
            .broadcast(&root, "status please", &family)
            .await
            .unwrap()
            .receipts;
        receipts.sort_by(|a, b| a.0.session_name.cmp(&b.0.session_name));
        assert_eq!(receipts[0].0.session_name, "alpha");
        assert_eq!(
            receipts[0].1.as_ref().unwrap().delivery_status,
            Receipt::Delivered
        );
        assert_eq!(receipts[1].0.session_name, "beta");
        assert!(receipts[1].1.is_err());
    }

    #[test]
    fn activity_follows_status_and_time_since_the_last_turn() {
        let now = Timestamp::from_ms(10 * HIBERNATE_AFTER.as_millis() as u64);
        let child = |status, idle_since| ChildSummary {
            status,
            idle_since,
            ..saved_child("sub-1", "left")
        };
        let recently = Timestamp::from_ms(now.as_ms() - 1_000);
        let long_ago = Timestamp::from_ms(now.as_ms() - HIBERNATE_AFTER.as_millis() as u64);
        assert_eq!(
            child(ChildStatus::Running, long_ago).activity(now),
            Activity::Working
        );
        assert_eq!(
            child(ChildStatus::Starting, long_ago).activity(now),
            Activity::Working
        );
        assert_eq!(
            child(ChildStatus::Idle, recently).activity(now),
            Activity::Napping
        );
        assert_eq!(
            child(ChildStatus::Idle, long_ago).activity(now),
            Activity::Hibernating
        );
        for status in [
            ChildStatus::Failed,
            ChildStatus::Cancelled,
            ChildStatus::Interrupted,
        ] {
            assert_eq!(
                child(status, recently).activity(now),
                Activity::Hibernating,
                "{status:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_broadcast_skips_hibernating_children_and_a_direct_message_wakes_one() {
        use mush_kernel::HostRequestHandler;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        let root = Identity::root("root");
        for name in ["alpha", "beta", "gamma"] {
            family
                .spawn(&root, "task".into(), name.into(), None, None, &family)
                .await
                .unwrap();
        }
        let children = settle(&family, &root).await;
        assert!(children.iter().all(|child| child.idle_since.as_ms() > 0));
        let id = |name: &str| {
            children
                .iter()
                .find(|child| child.name == name)
                .unwrap()
                .id
                .clone()
        };
        {
            let mut inner = family.lock();
            let beta = &mut inner.children.get_mut(&id("beta")).unwrap().summary;
            beta.idle_since =
                Timestamp::from_ms(beta.idle_since.as_ms() - HIBERNATE_AFTER.as_millis() as u64);
            inner.children.get_mut(&id("gamma")).unwrap().summary.status = ChildStatus::Failed;
        }

        let mut states: Vec<_> = family
            .roster(&root)
            .into_iter()
            .map(|view| (view.name, view.state))
            .collect();
        states.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            states,
            vec![
                ("alpha".to_string(), Activity::Napping),
                ("beta".to_string(), Activity::Hibernating),
                ("gamma".to_string(), Activity::Hibernating),
                ("root".to_string(), Activity::Napping),
            ]
        );
        let handler = RlmHandler::new(Arc::clone(&family), root.clone());
        let listed = handler
            .handle("rlm.list_subagents", serde_json::json!({}))
            .await
            .unwrap();
        let mut listed: Vec<_> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|child| (child["name"].clone(), child["state"].clone()))
            .collect();
        listed.sort_by_key(|(name, _)| name.to_string());
        assert_eq!(
            listed,
            vec![
                ("alpha".into(), "napping".into()),
                ("beta".into(), "hibernating".into()),
                ("gamma".into(), "hibernating".into()),
            ]
        );

        let broadcast = handler
            .handle(
                "agent_message.send",
                serde_json::json!({ "message": "status?", "target": "all" }),
            )
            .await
            .unwrap();
        assert_eq!(broadcast["skipped_hibernating"], 2);
        let receipts = broadcast["receipts"].as_array().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["target"]["sessionName"], "alpha");

        let receipt = family
            .send_message(&root, "child", Some("beta"), "wake up", &family)
            .await
            .unwrap();
        assert_eq!(receipt.delivery_status, Receipt::Delivered);
        let deadline = Instant::now() + Duration::from_secs(5);
        while family
            .children_of(&root)
            .iter()
            .find(|child| child.name == "beta")
            .is_none_or(|child| child.turns < 2 || child.status != ChildStatus::Idle)
        {
            assert!(Instant::now() < deadline, "beta never took its turn");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let beta = family
            .children_of(&root)
            .into_iter()
            .find(|child| child.name == "beta")
            .unwrap();
        assert_eq!(beta.activity(Timestamp::now()), Activity::Napping);
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

        let receipt = family
            .send_message(&root, "child", Some("slow"), "hurry up", &family)
            .await
            .unwrap();
        assert_eq!(receipt.delivery_status, Receipt::Queued);
        assert_eq!(receipt.target.session_name, "slow");
        assert_eq!(receipt.target.runtime_kind, RuntimeKind::Subagent);
        assert!(receipt.queued_at.is_some() && receipt.delivered_at.is_none());

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
                .send_message(&root, "child", Some("noter"), "hi", &family)
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
                thinking: None,
                status: ChildStatus::Running,
                spawned_at: Timestamp::zero(),
                updated_at: Timestamp::zero(),
                idle_since: Timestamp::zero(),
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

    type Resumed = Arc<StdMutex<Vec<Option<String>>>>;
    type ParentThinking = (String, Option<ThinkingLevel>);

    fn resuming_factory(requests: Requests, resumed: Resumed) -> ChildFactory {
        let inner = echo_factory(requests, "back again");
        Arc::new(move |spec: ChildSpec, family: Arc<Family>| {
            let inner = Arc::clone(&inner);
            let resumed = Arc::clone(&resumed);
            Box::pin(async move {
                resumed.lock().unwrap().push(spec.resume_session.clone());
                let resume = spec.resume_session.clone();
                let mut parts = inner(spec, family).await?;
                if let Some(session) = resume {
                    parts.config.session_id = session.into();
                }
                Ok(parts)
            })
        })
    }

    fn restored_family(
        status: ChildStatus,
        requests: Requests,
        resumed: Resumed,
    ) -> (tempfile::TempDir, Arc<Family>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagents.json");
        let child = ChildSummary {
            status,
            ..saved_child("sub-1", "left")
        };
        std::fs::write(
            &path,
            serde_json::to_vec(&Registry {
                children: vec![child],
            })
            .unwrap(),
        )
        .unwrap();
        let family = Family::new(
            Identity::root("root"),
            "root-session".into(),
            2,
            Some(resuming_factory(requests, resumed)),
            Some(path),
        );
        (dir, family)
    }

    #[tokio::test]
    async fn a_grandchild_is_given_its_own_parents_thinking_level() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let seen: Arc<StdMutex<Vec<ParentThinking>>> = Arc::new(StdMutex::new(Vec::new()));
        let inner = echo_factory(requests, "hi");
        let factory: ChildFactory = Arc::new({
            let seen = Arc::clone(&seen);
            move |spec: ChildSpec, family: Arc<Family>| {
                let inner = Arc::clone(&inner);
                let seen = Arc::clone(&seen);
                Box::pin(async move {
                    seen.lock()
                        .unwrap()
                        .push((spec.name.clone(), spec.parent_thinking.clone()));
                    let level = if spec.depth == 1 {
                        ThinkingLevel::High
                    } else {
                        ThinkingLevel::Low
                    };
                    let mut parts = inner(spec, family).await?;
                    parts.config.thinking_level = level;
                    Ok(parts)
                })
            }
        });
        let family = Family::new(
            Identity::root("root"),
            "root-session".into(),
            3,
            Some(factory),
            None,
        );
        let root = Identity::root("root");
        family
            .spawn(&root, "task".into(), "child".into(), None, None, &family)
            .await
            .unwrap();
        let child = Identity {
            id: Some(family.children_of(&root)[0].id.clone()),
            name: "child".into(),
            depth: 1,
        };
        family
            .spawn(
                &child,
                "task".into(),
                "grandchild".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("child".to_string(), None),
                ("grandchild".to_string(), Some(ThinkingLevel::High)),
            ]
        );
        assert_eq!(
            family.children_of(&child)[0].thinking,
            Some(ThinkingLevel::Low)
        );
    }

    #[tokio::test]
    async fn a_message_wakes_an_interrupted_child_in_its_own_session() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let resumed: Resumed = Arc::new(StdMutex::new(Vec::new()));
        let (_dir, family) = restored_family(
            ChildStatus::Running,
            Arc::clone(&requests),
            Arc::clone(&resumed),
        );
        let root = Identity::root("root");
        assert_eq!(family.all()[0].status, ChildStatus::Interrupted);

        let receipt = family
            .send_message(&root, "child", Some("left"), "are you there?", &family)
            .await
            .unwrap();

        assert_eq!(receipt.delivery_status, Receipt::Delivered);
        assert_eq!(receipt.target.session_id, "session-sub-1");
        assert_eq!(*resumed.lock().unwrap(), vec![Some("session-sub-1".into())]);
        let children = settle(&family, &root).await;
        assert_eq!(children[0].status, ChildStatus::Idle);
        assert_eq!(children[0].session_id, "session-sub-1");
        assert_eq!(children[0].answer.as_deref(), Some("back again"));
        let asked = requests.lock().unwrap();
        let texts: Vec<String> = asked[0]
            .messages
            .iter()
            .map(|message| match message {
                Message::User(user) => user.text(),
                Message::Event(event) => event.content.clone(),
                _ => String::new(),
            })
            .collect();
        assert!(
            texts.iter().any(|text| text == CHILD_INTERRUPTED),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.ends_with("[agent-message from parent:root]\n\nare you there?")),
            "{texts:?}"
        );
    }

    #[tokio::test]
    async fn a_woken_child_stays_live_for_the_next_message() {
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let resumed: Resumed = Arc::new(StdMutex::new(Vec::new()));
        let (_dir, family) = restored_family(
            ChildStatus::Idle,
            Arc::clone(&requests),
            Arc::clone(&resumed),
        );
        let root = Identity::root("root");

        family
            .send_message(&root, "child", Some("left"), "first", &family)
            .await
            .unwrap();
        settle(&family, &root).await;
        family
            .send_message(&root, "child", Some("left"), "second", &family)
            .await
            .unwrap();
        settle(&family, &root).await;

        assert_eq!(resumed.lock().unwrap().len(), 1);
        assert_eq!(requests.lock().unwrap().len(), 2);
        let asked = requests.lock().unwrap();
        assert!(!asked[0].messages.iter().any(|message| matches!(
            message,
            Message::Event(event) if event.content == CHILD_INTERRUPTED
        )));
    }

    fn saved_child(id: &str, name: &str) -> ChildSummary {
        ChildSummary {
            id: id.into(),
            name: name.into(),
            parent: None,
            depth: 1,
            session_id: format!("session-{id}"),
            model_id: "m".into(),
            thinking: None,
            status: ChildStatus::Idle,
            spawned_at: Timestamp::zero(),
            updated_at: Timestamp::zero(),
            idle_since: Timestamp::zero(),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_burst_of_child_events_reaches_the_registry_off_the_event_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagents.json");
        let children = (0..3)
            .map(|i| saved_child(&format!("sub-{i}"), &format!("n{i}")))
            .collect();
        std::fs::write(&path, serde_json::to_vec(&Registry { children }).unwrap()).unwrap();
        let family = Family::new(
            Identity::root("root"),
            "root".into(),
            2,
            None,
            Some(path.clone()),
        );

        for _ in 0..50 {
            family.record("sub-1", &ClientEvent::TurnStarted, None);
        }

        let turns = || {
            serde_json::from_slice::<Registry>(&std::fs::read(&path).unwrap())
                .unwrap()
                .children
                .into_iter()
                .find(|child| child.id == "sub-1")
                .map(|child| child.turns)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while turns() != Some(51) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(turns(), Some(51));
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
                "code": "r = await agent_message.send('done: two findings', receiver_role='parent', mode='follow_up')\nprint(r['target']['sessionName'], r['deliveryStatus'])"
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
        assert!(
            ["root delivered", "root queued"].contains(&text.trim()),
            "{text}"
        );
        tool.shutdown().await;
    }

    #[tokio::test]
    async fn the_send_host_call_wants_a_role_and_broadcasts_only_to_all() {
        use mush_kernel::HostRequestHandler;
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let family = family(requests, 2, None);
        family
            .spawn(
                &Identity::root("root"),
                "task".into(),
                "alpha".into(),
                None,
                None,
                &family,
            )
            .await
            .unwrap();
        settle(&family, &Identity::root("root")).await;
        let handler = RlmHandler::new(Arc::clone(&family), Identity::root("root"));

        let missing = handler
            .handle("agent_message.send", serde_json::json!({ "message": "hi" }))
            .await
            .unwrap_err();
        assert!(missing.0.contains("receiver_role must be"), "{}", missing.0);
        let positional = handler
            .handle(
                "agent_message.send",
                serde_json::json!({ "message": "hi", "target": "alpha" }),
            )
            .await
            .unwrap_err();
        assert!(positional.0.contains("positional"), "{}", positional.0);
        let mixed = handler
            .handle(
                "agent_message.send",
                serde_json::json!({ "message": "hi", "target": "all", "receiver_role": "child" }),
            )
            .await
            .unwrap_err();
        assert!(mixed.0.contains("cannot be combined"), "{}", mixed.0);

        let broadcast = handler
            .handle(
                "agent_message.send",
                serde_json::json!({ "message": "status?", "target": "all" }),
            )
            .await
            .unwrap();
        let receipts = broadcast["receipts"].as_array().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["target"]["sessionName"], "alpha");
        assert_eq!(receipts[0]["deliveryStatus"], "delivered");
        assert_eq!(broadcast["skipped_hibernating"], 0);
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
