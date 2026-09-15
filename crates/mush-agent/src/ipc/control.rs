use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

pub const MAX_QUEUE: usize = 64;
pub const MAX_TIMERS: usize = 32;
pub const MAX_RECORDS: usize = 256;
pub const MAX_TEXT: usize = 8192;
pub const MAX_DELAY_MS: u64 = 86_400_000;
pub const MAX_TTL_MS: u64 = 3_600_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub instance: String,
    pub session: String,
    pub activation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Message { text: String },
    WorkerCompletion { worker: String, text: String },
    Wake { text: String },
}

impl Event {
    fn valid(&self) -> bool {
        let text = match self {
            Self::Message { text } | Self::Wake { text } => text,
            Self::WorkerCompletion { worker, text } => {
                if !valid_id(worker) {
                    return false;
                }
                text
            }
        };
        !text.trim().is_empty() && text.len() <= MAX_TEXT
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Send {
        event: Event,
    },
    Schedule {
        event: Event,
        delay_ms: u64,
        ttl_ms: u64,
    },
    Cancel {
        id: String,
    },
    Status {
        id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub target: Target,
    pub operation: Operation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Scheduled,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Expired,
    QueueFull,
    Unavailable,
    Shutdown,
}

impl Outcome {
    fn pending(self) -> bool {
        matches!(self, Self::Scheduled | Self::Queued | Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ControlError {
    #[error("invalid payload")]
    InvalidPayload,
    #[error("unknown request")]
    UnknownRequest,
    #[error("delivery queue full")]
    QueueFull,
    #[error("timer limit reached")]
    TimerLimit,
    #[error("target unavailable")]
    Unavailable,
    #[error("request id already used")]
    DuplicateRequest,
    #[error("request already dispatched or terminal")]
    TooLate,
    #[error("request exceeds byte limit")]
    TooLarge,
    #[error("request timed out")]
    Timeout,
    #[error("connection limit reached")]
    ConnectionLimit,
    #[error("peer is not the owning user")]
    Unauthorised,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordStatus {
    pub id: String,
    pub outcome: Outcome,
    pub due_in_ms: Option<u64>,
    pub expires_in_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub target: Target,
    pub available: bool,
    pub queued: usize,
    pub scheduled: usize,
    pub records: Vec<RecordStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Accepted {
        id: String,
        outcome: Outcome,
        duplicate: bool,
    },
    Status(Status),
    Error {
        code: ControlError,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub id: String,
    pub target: Target,
    pub claimed_source: String,
    pub peer_uid: u32,
    pub event: Event,
}

impl Delivery {
    pub fn message(&self) -> Result<mush_ai::types::Message, serde_json::Error> {
        use mush_ai::types::{
            EventKind, EventMessage, EventOrigin, EventReceipt, Message, Timestamp,
        };
        let timestamp_ms = Timestamp::now();
        let kind = match self.event {
            Event::Message { .. } => EventKind::Message,
            Event::WorkerCompletion { .. } => EventKind::WorkerCompletion,
            Event::Wake { .. } => EventKind::Wake,
        };
        Ok(Message::Event(EventMessage {
            content: serde_json::to_string(&self.event)?,
            timestamp_ms,
            kind,
            receipts: vec![EventReceipt {
                id: format!("local-control-{}-{}", self.target.activation, self.id).into(),
                timestamp_ms,
                kind,
                origin: EventOrigin::Control {
                    instance: self.target.instance.clone(),
                    session: self.target.session.clone(),
                    activation: self.target.activation.clone(),
                    claimed_source: self.claimed_source.clone(),
                    peer_uid: self.peer_uid,
                },
            }],
        }))
    }
}

struct Record {
    request: Request,
    delivery: Delivery,
    outcome: Outcome,
    due: Option<Instant>,
    expires: Option<Instant>,
}

struct State {
    target: Target,
    available: bool,
    records: VecDeque<Record>,
    queue: VecDeque<String>,
    running_failed: bool,
}

#[derive(Clone)]
pub struct Control(Arc<Mutex<State>>);

pub fn new_id() -> String {
    mush_ai::types::PromptId::new().to_string()
}

pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl Control {
    pub fn new(session: String) -> Self {
        Self(Arc::new(Mutex::new(State {
            target: Target {
                instance: new_id(),
                session,
                activation: new_id(),
            },
            available: true,
            records: VecDeque::new(),
            queue: VecDeque::new(),
            running_failed: false,
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn target(&self) -> Target {
        self.state().target.clone()
    }

    pub fn activate(&self, session: String) {
        let mut state = self.state();
        state.terminate(Outcome::Unavailable);
        state.target.session = session;
        state.target.activation = new_id();
    }

    pub fn shutdown(&self) {
        let mut state = self.state();
        state.available = false;
        state.terminate(Outcome::Shutdown);
    }

    pub fn request(&self, id: &str, source: &str, uid: u32, request: Request) -> Reply {
        let mut state = self.state();
        state.tick(Instant::now());
        match state.request(id, source, uid, request) {
            Ok(reply) => reply,
            Err(code) => Reply::Error { code },
        }
    }

    pub fn tick(&self) {
        self.state().tick(Instant::now());
    }

    pub fn take_next(&self, target: &Target) -> Option<Delivery> {
        let mut state = self.state();
        state.tick(Instant::now());
        if !state.available
            || state.target != *target
            || state.records.iter().any(|r| r.outcome == Outcome::Running)
        {
            return None;
        }
        let id = state.queue.pop_front()?;
        state.running_failed = false;
        let record = state.records.iter_mut().find(|r| r.delivery.id == id)?;
        record.outcome = Outcome::Running;
        Some(record.delivery.clone())
    }

    pub fn finish(&self, outcome: Outcome) {
        if !matches!(
            outcome,
            Outcome::Completed | Outcome::Failed | Outcome::Cancelled
        ) {
            return;
        }
        let mut state = self.state();
        let outcome = if outcome == Outcome::Completed && state.running_failed {
            Outcome::Failed
        } else {
            outcome
        };
        for record in &mut state.records {
            if record.outcome == Outcome::Running {
                record.outcome = outcome;
            }
        }
    }

    pub fn running_failed(&self, failed: bool) {
        self.state().running_failed = failed;
    }
}

impl State {
    fn terminate(&mut self, outcome: Outcome) {
        self.queue.clear();
        for record in &mut self.records {
            if record.outcome.pending() {
                record.outcome = outcome;
            }
        }
    }

    fn tick(&mut self, now: Instant) {
        for record in &mut self.records {
            if matches!(record.outcome, Outcome::Scheduled | Outcome::Queued)
                && record.expires.is_some_and(|expiry| expiry <= now)
            {
                record.outcome = Outcome::Expired;
                self.queue.retain(|id| *id != record.delivery.id);
            }
        }
        let mut due: Vec<_> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.outcome == Outcome::Scheduled && r.due.is_some_and(|t| t <= now))
            .map(|(i, r)| (r.due, i))
            .collect();
        due.sort_unstable();
        for (_, index) in due {
            let record = &mut self.records[index];
            if self.queue.len() == MAX_QUEUE {
                record.outcome = Outcome::QueueFull;
            } else {
                record.outcome = Outcome::Queued;
                self.queue.push_back(record.delivery.id.clone());
            }
        }
    }

    fn request(
        &mut self,
        id: &str,
        source: &str,
        uid: u32,
        request: Request,
    ) -> Result<Reply, ControlError> {
        if !valid_id(id) || !valid_id(source) {
            return Err(ControlError::InvalidPayload);
        }
        if request.target != self.target
            || (!self.available && !matches!(request.operation, Operation::Status { .. }))
        {
            return Err(ControlError::Unavailable);
        }
        match &request.operation {
            Operation::Status { id } => {
                if id.as_ref().is_some_and(|id| !valid_id(id)) {
                    return Err(ControlError::InvalidPayload);
                }
                if id
                    .as_ref()
                    .is_some_and(|id| !self.records.iter().any(|r| r.delivery.id == *id))
                {
                    return Err(ControlError::UnknownRequest);
                }
                let now = Instant::now();
                Ok(Reply::Status(Status {
                    target: self.target.clone(),
                    available: self.available,
                    queued: self.queue.len(),
                    scheduled: self
                        .records
                        .iter()
                        .filter(|r| r.outcome == Outcome::Scheduled)
                        .count(),
                    records: self
                        .records
                        .iter()
                        .filter(|r| id.as_ref().is_none_or(|id| r.delivery.id == *id))
                        .map(|r| RecordStatus {
                            id: r.delivery.id.clone(),
                            outcome: r.outcome,
                            due_in_ms: r
                                .due
                                .map(|t| t.saturating_duration_since(now).as_millis() as u64),
                            expires_in_ms: r
                                .expires
                                .map(|t| t.saturating_duration_since(now).as_millis() as u64),
                        })
                        .collect(),
                }))
            }
            Operation::Cancel { id } => {
                if !valid_id(id) {
                    return Err(ControlError::InvalidPayload);
                }
                let record = self
                    .records
                    .iter_mut()
                    .find(|r| r.delivery.id == *id)
                    .ok_or(ControlError::UnknownRequest)?;
                if record.outcome == Outcome::Cancelled {
                    return Ok(Reply::Accepted {
                        id: id.clone(),
                        outcome: Outcome::Cancelled,
                        duplicate: true,
                    });
                }
                if !matches!(record.outcome, Outcome::Scheduled | Outcome::Queued) {
                    return Err(ControlError::TooLate);
                }
                record.outcome = Outcome::Cancelled;
                self.queue.retain(|queued| queued != id);
                Ok(Reply::Accepted {
                    id: id.clone(),
                    outcome: Outcome::Cancelled,
                    duplicate: false,
                })
            }
            Operation::Send { event } | Operation::Schedule { event, .. } => {
                if !event.valid() {
                    return Err(ControlError::InvalidPayload);
                }
                if let Some(record) = self.records.iter().find(|r| r.delivery.id == id) {
                    if record.request != request
                        || record.delivery.claimed_source != source
                        || record.delivery.peer_uid != uid
                    {
                        return Err(ControlError::DuplicateRequest);
                    }
                    return Ok(Reply::Accepted {
                        id: id.to_owned(),
                        outcome: record.outcome,
                        duplicate: true,
                    });
                }
                let (outcome, due, expires) = if let Operation::Schedule {
                    delay_ms, ttl_ms, ..
                } = request.operation
                {
                    if !(100..=MAX_DELAY_MS).contains(&delay_ms)
                        || !(100..=MAX_TTL_MS).contains(&ttl_ms)
                    {
                        return Err(ControlError::InvalidPayload);
                    }
                    if self
                        .records
                        .iter()
                        .filter(|r| r.outcome == Outcome::Scheduled)
                        .count()
                        == MAX_TIMERS
                    {
                        return Err(ControlError::TimerLimit);
                    }
                    let due = Instant::now() + Duration::from_millis(delay_ms);
                    (
                        Outcome::Scheduled,
                        Some(due),
                        Some(due + Duration::from_millis(ttl_ms)),
                    )
                } else {
                    if self.queue.len() == MAX_QUEUE {
                        return Err(ControlError::QueueFull);
                    }
                    (Outcome::Queued, None, None)
                };
                if self.records.len() == MAX_RECORDS {
                    let index = self
                        .records
                        .iter()
                        .position(|r| !r.outcome.pending())
                        .ok_or(ControlError::QueueFull)?;
                    self.records.remove(index);
                }
                let delivery = Delivery {
                    id: id.to_owned(),
                    target: self.target.clone(),
                    claimed_source: source.to_owned(),
                    peer_uid: uid,
                    event: event.clone(),
                };
                self.records.push_back(Record {
                    request,
                    delivery,
                    outcome,
                    due,
                    expires,
                });
                if outcome == Outcome::Queued {
                    self.queue.push_back(id.to_owned());
                }
                Ok(Reply::Accepted {
                    id: id.to_owned(),
                    outcome,
                    duplicate: false,
                })
            }
        }
    }
}
