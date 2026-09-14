use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::inbox::TerminalReason;

mod files;
mod registration;
pub use registration::RegistrationStore;

pub const MAX_ROOTS: usize = 64;
pub const MAX_TASKS: usize = 128;
pub const MAX_WORKERS: usize = 32;
pub const MAX_TEXT: usize = 512;
pub const REQUEST_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_FILES: usize = 384;
pub const STALE_SECONDS: i64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    #[error("missing file")]
    Missing,
    #[error("unsafe path or permissions")]
    Unsafe,
    #[error("read unavailable")]
    Unavailable,
    #[error("snapshot exceeds limit")]
    Oversized,
    #[error("malformed snapshot")]
    Malformed,
    #[error("registration identity mismatch")]
    Identity,
}

impl From<std::io::Error> for ReadError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::Missing,
            std::io::ErrorKind::PermissionDenied => Self::Unsafe,
            _ => Self::Unavailable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub version: u32,
    pub session_id: String,
    pub cwd: PathBuf,
    pub roots: Vec<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub directory: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_queue: Option<ReviewSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_bindings: Vec<TaskBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBinding {
    pub worker: String,
    pub task_id: String,
}

impl Registration {
    fn validate_bindings(&self) -> Result<(), ReadError> {
        let mut tasks = HashSet::new();
        for source in &self.roots {
            if source.task_bindings.len() > MAX_WORKERS {
                return Err(ReadError::Oversized);
            }
            let mut workers = HashSet::new();
            for binding in &source.task_bindings {
                let review = source.review_queue.as_ref().ok_or(ReadError::Identity)?;
                if !worker_name(&binding.worker)
                    || !task_id(&binding.task_id)
                    || !workers.insert(&binding.worker)
                    || !tasks.insert((&review.path, &binding.task_id))
                {
                    return Err(ReadError::Identity);
                }
            }
        }
        if tasks.len() > MAX_TASKS {
            return Err(ReadError::Oversized);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSource {
    pub path: PathBuf,
    pub schema: ReviewSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSchema {
    OperatorQueueV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Queued,
    GateWaiting,
    Running,
    Blocked,
    AwaitingInput,
    Finished,
    Failed,
    Cancelled,
    Unknown,
}

impl RunState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::GateWaiting => "gate waiting",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::AwaitingInput => "awaiting input",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    Missing,
    AwaitingReview,
    Accepted,
    Integrated,
    Rejected,
    Unknown,
}

impl ReviewState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "review missing",
            Self::AwaitingReview => "awaiting review",
            Self::Accepted => "accepted",
            Self::Integrated => "integrated",
            Self::Rejected => "rejected",
            Self::Unknown => "review unknown",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkerState {
    #[serde(default)]
    pub name: Option<String>,
    pub state: String,
    #[serde(default)]
    pub round: u32,
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub terminal_event: Option<crate::inbox::Event>,
}

impl WorkerState {
    pub fn run_state(&self) -> RunState {
        if let Some(event) = &self.terminal_event {
            return match event.reason {
                TerminalReason::Completed => RunState::Finished,
                TerminalReason::Blocked => RunState::Blocked,
                TerminalReason::AwaitingInput => RunState::AwaitingInput,
                TerminalReason::Cancelled | TerminalReason::Deadline => RunState::Cancelled,
                TerminalReason::Failed
                | TerminalReason::Timeout
                | TerminalReason::RoundLimit
                | TerminalReason::SpawnFailed
                | TerminalReason::SupervisorError
                | TerminalReason::ChildSurvived => RunState::Failed,
            };
        }
        match self.state.as_str() {
            "prepared" | "queued" | "follow_up_pending" | "idle" | "retrying"
            | "switching_provider" => RunState::Queued,
            "waiting" => RunState::GateWaiting,
            "running" => RunState::Running,
            "blocked" => RunState::Blocked,
            "awaiting_input" => RunState::AwaitingInput,
            "done" => RunState::Finished,
            "error" | "failed" => RunState::Failed,
            "cancelled" => RunState::Cancelled,
            "stopped" if matches!(self.detail.as_str(), "cancelled" | "deadline") => {
                RunState::Cancelled
            }
            "stopped" if self.detail == "round limit" => RunState::Failed,
            _ => RunState::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskStatus {
    pub worker: String,
    pub name: String,
    pub run: RunState,
    pub recorded_state: String,
    pub round: u32,
    pub activity: String,
    pub headless_activity: Option<String>,
    pub headless_error: Option<ReadError>,
    pub updated_at: Option<DateTime<Utc>>,
    pub read_error: Option<ReadError>,
    pub review: ReviewState,
    pub review_updated_at: Option<DateTime<Utc>>,
    pub review_detail: String,
}

impl TaskStatus {
    fn unknown(worker: &str, error: ReadError) -> Self {
        Self {
            worker: worker.into(),
            name: safe_text(worker),
            run: RunState::Unknown,
            recorded_state: "unknown".into(),
            round: 0,
            activity: "execution state unavailable".into(),
            headless_activity: None,
            headless_error: None,
            updated_at: None,
            read_error: Some(error),
            review: ReviewState::Missing,
            review_updated_at: None,
            review_detail: String::new(),
        }
    }

    pub fn age_seconds(&self, now: DateTime<Utc>) -> Option<i64> {
        self.updated_at.map(|at| (now - at).num_seconds().max(0))
    }

    pub fn record_is_old(&self, now: DateTime<Utc>) -> bool {
        self.age_seconds(now).is_none_or(|age| age > STALE_SECONDS)
    }

    pub fn review_predates_worker(&self) -> bool {
        matches!((self.review_updated_at, self.updated_at), (Some(review), Some(run)) if review < run)
    }
}

#[derive(Debug, Clone)]
pub struct RootStatus {
    pub source: Source,
    pub tasks: Vec<TaskStatus>,
    pub read_error: Option<ReadError>,
    pub review_error: Option<ReadError>,
    pub supervisor: Option<SupervisorState>,
    pub supervisor_error: Option<ReadError>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub roots: Vec<RootStatus>,
    pub registration_error: Option<ReadError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SupervisorState {
    pub state: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct HeadlessProjection {
    state: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OperatorQueue {
    updated_at: DateTime<Utc>,
    root_owner: String,
    items: Vec<QueueItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct QueueItem {
    id: String,
    worker: String,
    state: String,
    #[serde(default)]
    accepted: Option<bool>,
    #[serde(default)]
    integrated: Option<String>,
    #[serde(default)]
    reviewed: Option<String>,
    #[serde(default)]
    reviewer: Option<String>,
    #[serde(default)]
    supervisor_root: Option<PathBuf>,
}

impl OperatorQueue {
    fn item_for(
        &self,
        source: &Source,
        worker: &str,
        shared: bool,
    ) -> Result<Option<&QueueItem>, ReadError> {
        if let Some(binding) = source
            .task_bindings
            .iter()
            .find(|binding| binding.worker == worker)
        {
            let item = self.items.iter().find(|item| item.id == binding.task_id);
            return match item {
                Some(item)
                    if item.worker != worker
                        || item
                            .supervisor_root
                            .as_ref()
                            .is_some_and(|root| *root != source.directory) =>
                {
                    Err(ReadError::Identity)
                }
                item => Ok(item),
            };
        }
        let mut candidates = self.items.iter().filter(|item| {
            item.worker == worker
                && item
                    .supervisor_root
                    .as_ref()
                    .is_none_or(|root| *root == source.directory)
        });
        let Some(item) = candidates.next() else {
            return Ok(None);
        };
        if candidates.next().is_some() || (shared && item.supervisor_root.is_none()) {
            return Err(ReadError::Identity);
        }
        Ok(Some(item))
    }
}

impl QueueItem {
    fn review_state(&self) -> ReviewState {
        let reviewed = self.reviewed.as_ref().is_some_and(|s| !s.is_empty());
        let integrated = self
            .integrated
            .as_ref()
            .is_some_and(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        match (self.state.as_str(), self.accepted, reviewed, integrated) {
            ("integrated", Some(true), true, true) => ReviewState::Integrated,
            ("accepted", Some(true), true, _) => ReviewState::Accepted,
            ("rejected", Some(false), true, _) => ReviewState::Rejected,
            ("awaiting_review" | "independent_review", None | Some(false), _, false) => {
                ReviewState::AwaitingReview
            }
            ("queued" | "running" | "blocked" | "done" | "finished", None, false, false) => {
                ReviewState::Missing
            }
            _ => ReviewState::Unknown,
        }
    }
}

#[derive(Default)]
pub struct Loader {
    snapshot: Snapshot,
    queues: BTreeMap<usize, OperatorQueue>,
    identity: Option<Registration>,
}

impl Loader {
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn refresh(&mut self, registration: &Registration, now: DateTime<Utc>) {
        if self.identity.as_ref() != Some(registration) {
            self.snapshot = Snapshot::default();
            self.queues.clear();
            self.identity = Some(registration.clone());
        }
        if let Err(error) = registration.validate_bindings() {
            self.snapshot = Snapshot::default();
            self.queues.clear();
            self.snapshot.registration_error = Some(error);
            return;
        }
        let mut budget = files::Budget::new();
        let old = std::mem::take(&mut self.snapshot.roots);
        self.snapshot.registration_error = None;
        let mut task_count = 0;
        let mut queues = BTreeMap::new();
        for (index, source) in registration.roots.iter().take(MAX_ROOTS).enumerate() {
            let previous = old.iter().find(|r| r.source == *source);
            let result = Self::load_root(source, previous, &mut budget, now, &mut task_count);
            self.snapshot.roots.push(match result {
                Ok(root) => root,
                Err(error) => {
                    let mut root = previous.cloned().unwrap_or_else(|| RootStatus {
                        source: source.clone(),
                        tasks: Vec::new(),
                        read_error: None,
                        review_error: None,
                        supervisor: None,
                        supervisor_error: None,
                    });
                    root.tasks.truncate(MAX_TASKS.saturating_sub(task_count));
                    task_count += root.tasks.len();
                    root.read_error = Some(error);
                    root
                }
            });
            let shared = registration
                .roots
                .iter()
                .filter(|other| other.review_queue == source.review_queue)
                .count()
                > 1;
            self.load_review(index, shared, &mut budget, now, &mut queues);
        }
        if registration.roots.len() > MAX_ROOTS {
            self.snapshot.registration_error = Some(ReadError::Oversized);
        }
    }

    pub fn registration_failed(&mut self, error: ReadError) {
        if matches!(error, ReadError::Unsafe | ReadError::Identity) {
            *self = Self::default();
        }
        self.snapshot.registration_error = Some(error);
    }

    fn load_root(
        source: &Source,
        previous: Option<&RootStatus>,
        budget: &mut files::Budget,
        now: DateTime<Utc>,
        task_count: &mut usize,
    ) -> Result<RootStatus, ReadError> {
        let directory = files::Directory::open(&source.directory)?;
        let mut names: BTreeSet<_> = directory.workers()?.into_iter().collect();
        if let Some(previous) = previous {
            names.extend(previous.tasks.iter().map(|t| t.worker.clone()));
        }
        let mut root = RootStatus {
            source: source.clone(),
            tasks: Vec::new(),
            read_error: None,
            review_error: None,
            supervisor: None,
            supervisor_error: None,
        };
        match directory.json::<SupervisorState>("supervisor.json", 4 * 1024, budget) {
            Ok(mut supervisor) => {
                supervisor.state = safe_text(&supervisor.state);
                root.supervisor = Some(supervisor);
            }
            Err(error) => {
                root.supervisor = previous.and_then(|r| r.supervisor.clone());
                root.supervisor_error = Some(error);
            }
        }
        for worker in names {
            if *task_count == MAX_TASKS || root.tasks.len() == MAX_WORKERS {
                root.read_error = Some(ReadError::Oversized);
                break;
            }
            let state = directory
                .child(&worker)
                .and_then(|d| d.json::<WorkerState>("state.json", 16 * 1024, budget))
                .and_then(|state| {
                    if state.name.as_ref().is_some_and(|n| n != &worker)
                        || state.at.is_some_and(|at| at > now)
                        || state.terminal_event.as_ref().is_some_and(|e| {
                            e.worker_id != worker || e.round != state.round || e.validate().is_err()
                        })
                    {
                        return Err(ReadError::Identity);
                    }
                    Ok(state)
                });
            if matches!(state, Err(ReadError::Missing))
                && previous.is_none_or(|root| !root.tasks.iter().any(|task| task.worker == worker))
            {
                continue;
            }
            *task_count += 1;
            let task = match state {
                Ok(state) => TaskStatus {
                    worker: worker.clone(),
                    name: worker.clone(),
                    run: state.run_state(),
                    recorded_state: safe_text(&state.state),
                    round: state.round,
                    activity: safe_text(&state.detail),
                    headless_activity: None,
                    headless_error: None,
                    updated_at: state.at,
                    read_error: None,
                    review: ReviewState::Missing,
                    review_updated_at: None,
                    review_detail: String::new(),
                },
                Err(error) => {
                    let mut task = previous
                        .and_then(|root| root.tasks.iter().find(|t| t.worker == worker))
                        .cloned()
                        .unwrap_or_else(|| TaskStatus::unknown(&worker, error));
                    task.read_error = Some(error);
                    task
                }
            };
            let mut task = task;
            if task.run == RunState::Running {
                let previous_activity = previous
                    .and_then(|r| {
                        r.tasks
                            .iter()
                            .find(|t| t.worker == task.worker && t.round == task.round)
                    })
                    .and_then(|t| t.headless_activity.clone());
                let headless = directory.child(&worker).and_then(|d| {
                    d.json::<HeadlessProjection>(
                        &format!("round-{:03}.json", task.round),
                        8 * 1024,
                        budget,
                    )
                });
                match headless {
                    Ok(value) => {
                        task.headless_activity = Some(safe_text(&format!(
                            "headless record {} / timestamp unavailable",
                            safe_text(&value.state)
                        )));
                        task.headless_error = None;
                    }
                    Err(ReadError::Missing) if previous_activity.is_none() => {}
                    Err(error) => {
                        task.headless_activity = previous_activity;
                        task.headless_error = Some(error);
                    }
                }
            }
            root.tasks.push(task);
        }
        Ok(root)
    }

    fn load_review(
        &mut self,
        index: usize,
        shared: bool,
        budget: &mut files::Budget,
        now: DateTime<Utc>,
        queues: &mut BTreeMap<PathBuf, Result<OperatorQueue, ReadError>>,
    ) {
        let root = &mut self.snapshot.roots[index];
        let source = &root.source;
        for task in &mut root.tasks {
            task.name = task.worker.clone();
            task.review = ReviewState::Missing;
            task.review_updated_at = None;
            task.review_detail.clear();
        }
        root.review_error = None;
        if let Some(review) = &source.review_queue {
            let queue = queues
                .entry(review.path.clone())
                .or_insert_with(|| {
                    files::read_json::<OperatorQueue>(&review.path, 64 * 1024, budget).and_then(
                        |queue| {
                            if queue.items.len() > MAX_TASKS
                                || queue.updated_at > now
                                || queue.root_owner.is_empty()
                            {
                                return Err(ReadError::Malformed);
                            }
                            let mut ids = HashSet::new();
                            if queue.items.iter().any(|i| {
                                !worker_name(&i.worker)
                                    || !task_id(&i.id)
                                    || !ids.insert(&i.id)
                                    || i.supervisor_root
                                        .as_ref()
                                        .is_some_and(|path| !files::valid_path(path))
                            }) {
                                return Err(ReadError::Malformed);
                            }
                            Ok(queue)
                        },
                    )
                })
                .clone();
            match queue {
                Ok(queue) => {
                    self.queues.insert(index, queue);
                }
                Err(error) => root.review_error = Some(error),
            }
            if let Some(queue) = self.queues.get(&index) {
                for task in &mut root.tasks {
                    match queue.item_for(source, &task.worker, shared) {
                        Ok(Some(item)) => {
                            task.name = safe_text(&item.id);
                            task.review = item.review_state();
                            task.review_updated_at = Some(queue.updated_at);
                            task.review_detail = safe_text(&format!(
                                "operator {} | queue {} | reviewer {}",
                                queue.root_owner,
                                item.state,
                                item.reviewer.as_deref().unwrap_or("missing")
                            ));
                        }
                        Ok(None) => {}
                        Err(_) => {
                            task.review = ReviewState::Unknown;
                            task.review_detail =
                                "ambiguous task association / use subagents bind".into();
                        }
                    }
                }
            }
        }
    }
}

pub fn task_id(id: &str) -> bool {
    !id.trim().is_empty() && id.chars().count() <= MAX_TEXT && safe_text(id) == id
}

pub fn worker_name(name: &str) -> bool {
    name.len() <= 64 && crate::inbox::identifier(name)
}

pub fn safe_text(value: &str) -> String {
    value.chars().take(MAX_TEXT).map(|c| {
        if c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            '�'
        } else { c }
    }).collect()
}

#[cfg(test)]
mod tests;
