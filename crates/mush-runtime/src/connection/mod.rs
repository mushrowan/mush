use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures::FutureExt;
use tokio::task::{JoinError, JoinHandle};

use mush_agent::AgentEvent;
use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    Message, PaneId, ThinkingLevel, Timestamp, TokenCount, ToolCallId, UserMessage,
};
use mush_session::compact::CompactionFocus;

use crate::Applied;
use crate::autonomous::Autonomous;
use crate::commands::{SessionCommand, SessionEvent};
use crate::compaction::{AppliedCompaction, CompactionTaskResult};
use crate::family::{AgentInbox, ChildId, ChildSummary, Family, ObserveInbox};
use crate::goal::{Goal, GoalStore};
use crate::inbox::OrchestratorInbox;
use crate::kernel_channels::KernelSignals;
use crate::notes::Notes;
use crate::schedule::{Job, Scheduler};
use crate::session::{
    PendingConfirmation, PendingImage, PendingPrompt, SessionRuntime, StreamGeneration,
};
use crate::turn::{
    TurnConfig, TurnDeps, TurnRequest, aligned_tools, build_user_content, resolve_auth_for_model,
    resolve_auth_into_options,
};

mod compaction;
mod host_commands;
mod refine;

use compaction::FailedCompaction;
pub use host_commands::{ContextUsage, HostCommands};

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
    AbortAndSendQueued,
    ConfirmTool {
        allow: bool,
    },
    SetModel(String),
    SetSubagentModel(Option<String>),
    SetAccount(Option<mush_ai::types::AccountLabel>),
    SetPaused(Option<mush_session::Pause>),
    SetThinking(ThinkingLevel),
    SetRlmMaxDepth(u32),
    SetSystemPrompt(Option<String>),
    ReloadSystemPrompt,
    SetStreamOptions(Box<mush_ai::types::StreamOptions>),
    SetAutoCompact(bool),
    SetCompactionModel(Option<Box<(mush_ai::types::Model, mush_ai::types::StreamOptions)>>),
    Session(SessionCommand),
    SummariseAndNavigate {
        entry_id: mush_session::tree::EntryId,
    },
    Refine(mush_session::refine::RefineRequest),
    Compact {
        focus: CompactionFocus,
        auto: bool,
    },
    CompactionRetry,
    CompactionFallback,
    AnswerKernelBusy(mush_tools::ipython::BusyKernelChoice),
    SideQuestion(String),
    Goal(crate::goal::GoalCommand),
    Schedule(crate::schedule::ScheduleCommand),
    Autonomous(crate::autonomous::AutonomousCommand),
    StopChild {
        session_id: String,
    },
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
    CompactionCancelled,
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
    UserInput {
        text: String,
        images: Vec<PendingImage>,
        queued: bool,
    },
    SteerRetracted {
        text: String,
    },
    ToolOutput {
        exec_id: String,
        line: String,
    },
    BashDone(mush_tools::kernel_jobs::JobNotice),
    KernelBusy,
    KernelBusyWithdrawn,
    SideAnswer {
        question: String,
        answer: Result<String, String>,
    },
    ChildUpdated(Box<ChildSummary>),
    GoalChanged(Option<Box<Goal>>),
    ScheduleChanged(Vec<Job>),
    ProgressNote(String),
    PausedChanged(Option<mush_session::Pause>),
    AutonomousChanged(Box<crate::autonomous::AutonomousStatus>),
    Refined(Box<mush_session::harness::RefinementResult>),
    SyncChanged(mush_session::SyncState),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConnectionState {
    pub session_id: mush_ai::types::SessionId,
    pub title: Option<String>,
    pub model_id: String,
    #[serde(default)]
    pub subagent_model: Option<String>,
    #[serde(default)]
    pub accounts: mush_ai::types::SessionAccounts,
    #[serde(default)]
    pub paused: Option<mush_session::Pause>,
    pub thinking_level: ThinkingLevel,
    pub context_tokens: TokenCount,
    pub context_len: usize,
    pub tree_len: usize,
    pub rlm_depth: u32,
    pub rlm_max_depth: u32,
    #[serde(default)]
    pub rlm_max_depth_source: crate::session::RlmMaxDepthSource,
    pub turn_active: bool,
    pub turn_model: Option<mush_ai::types::Model>,
    pub active_turns: usize,
    pub compacting: bool,
    pub pending_prompt: bool,
    pub controlled: bool,
    #[serde(default)]
    pub last_request: Option<mush_ai::request_snapshot::RequestDigest>,
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
    #[serde(default)]
    pub sync: mush_session::SyncState,
    #[serde(default = "auto_compact_by_default")]
    pub auto_compact: bool,
}

fn auto_compact_by_default() -> bool {
    true
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
    #[error("the worker did not answer in time")]
    TimedOut,
    #[error("reattached to the worker, the command was not applied")]
    Reconnected,
    #[error("{0}")]
    Refused(String),
    #[error("transport: {0}")]
    Transport(String),
}

#[async_trait::async_trait]
pub trait AgentConnection: Send + std::any::Any {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
    async fn send(&mut self, command: ClientCommand) -> Result<Vec<ClientEvent>, ConnectionError>;
    async fn next_event(&mut self) -> Option<ClientEvent>;
    fn try_next_event(&mut self) -> Option<ClientEvent>;
    async fn tick(&mut self) {}
    fn state(&self) -> ConnectionState;
    fn closed(&self) -> bool;
    async fn transcript(&mut self) -> Vec<Message>;
    async fn history(&mut self) -> Vec<Message>;
    async fn recent_history(&mut self, limit: usize) -> mush_session::tree::RecentHistory;
    async fn tree_summary(&mut self) -> Vec<mush_session::tree::TreeSummaryRow>;
    async fn navigation_impact(&mut self, entry_id: &mush_session::tree::EntryId) -> usize;
    async fn system_prompt(&mut self) -> Option<String>;
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
    fork_compaction_due: bool,
    kernel_compaction_notice_due: bool,
    failed_compaction: Option<FailedCompaction>,
    branch_summary: Option<(
        mush_session::tree::EntryId,
        tokio::task::JoinHandle<Result<String, String>>,
    )>,
    refine: Option<tokio::task::JoinHandle<Result<mush_session::refine::Plan, String>>>,
    auto_review: Option<(
        mush_session::refine::AutoRefineReason,
        tokio::task::JoinHandle<Result<mush_session::refine::AutoRefineReview, String>>,
    )>,
    assistant_turns_since_review: u32,
    last_auto_review: Option<std::time::Instant>,
    compact_refine_pending: bool,
    title: Option<tokio::task::JoinHandle<Option<String>>>,
    inbox: Option<OrchestratorInbox>,
    kernel: Option<KernelSignals>,
    busy_reply: Option<tokio::sync::oneshot::Sender<mush_tools::ipython::BusyKernelChoice>>,
    side_questions: Vec<(String, tokio::task::JoinHandle<Result<String, String>>)>,
    system_prompt_builder: Option<SystemPromptBuilder>,
    agent_inbox: Option<AgentInbox>,
    observe_inbox: Option<ObserveInbox>,
    deferred_deliveries: VecDeque<crate::family::Delivery>,
    family: Option<Arc<Family>>,
    family_member: Option<(Arc<Family>, ChildId)>,
    turn_error: Option<String>,
    known_children: HashMap<ChildId, ChildSummary>,
    known_family_revision: Option<u64>,
    goal: Option<Arc<GoalStore>>,
    known_goal: Option<Goal>,
    scheduler: Option<Arc<Scheduler>>,
    known_jobs: Vec<Job>,
    deferred_jobs: VecDeque<String>,
    notes: Option<Arc<Notes>>,
    autonomous: Option<Arc<Autonomous>>,
    gate_task: Option<tokio::task::JoinHandle<Vec<crate::autonomous::GateRun>>>,
    host_commands: Option<Arc<HostCommands>>,
    sync: Option<tokio::sync::watch::Receiver<mush_session::SyncState>>,
    sync_state: mush_session::SyncState,
    relinquish: bool,
    closed: bool,
}

pub type SystemPromptBuilder = Arc<dyn Fn() -> String + Send + Sync>;

fn status_event(text: impl Into<String>) -> ClientEvent {
    ClientEvent::Session(SessionEvent::Status(text.into()))
}

fn failed_event(text: impl Into<String>) -> ClientEvent {
    ClientEvent::Session(SessionEvent::Failed(text.into()))
}

fn take_finished<T>(slot: &mut Option<JoinHandle<T>>) -> Option<Result<T, JoinError>> {
    let result = slot
        .as_mut()
        .filter(|task| task.is_finished())?
        .now_or_never()?;
    *slot = None;
    Some(result)
}

fn take_finished_keyed<K, T>(
    slot: &mut Option<(K, JoinHandle<T>)>,
) -> Option<(K, Result<T, JoinError>)> {
    let (_, task) = slot.as_mut().filter(|(_, task)| task.is_finished())?;
    let result = task.now_or_never()?;
    slot.take().map(|(key, _)| (key, result))
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
        runtime
            .auto_compact
            .store(config.auto_compact, std::sync::atomic::Ordering::Relaxed);
        Self {
            runtime,
            tools,
            registry,
            config,
            injections,
            queued: VecDeque::new(),
            compaction: None,
            compaction_auto: false,
            fork_compaction_due: false,
            kernel_compaction_notice_due: false,
            failed_compaction: None,
            branch_summary: None,
            refine: None,
            auto_review: None,
            assistant_turns_since_review: 0,
            last_auto_review: None,
            compact_refine_pending: false,
            title: None,
            inbox: None,
            kernel: None,
            busy_reply: None,
            side_questions: Vec::new(),
            system_prompt_builder: None,
            agent_inbox: None,
            observe_inbox: None,
            deferred_deliveries: VecDeque::new(),
            family: None,
            family_member: None,
            turn_error: None,
            known_children: HashMap::new(),
            known_family_revision: None,
            goal: None,
            known_goal: None,
            scheduler: None,
            known_jobs: Vec::new(),
            deferred_jobs: VecDeque::new(),
            notes: None,
            autonomous: None,
            gate_task: None,
            host_commands: None,
            sync: None,
            sync_state: mush_session::SyncState::default(),
            relinquish: false,
            closed: false,
        }
    }

    pub fn set_host_commands(&mut self, commands: Option<Arc<HostCommands>>) {
        self.host_commands = commands;
    }

    pub fn follow_sync(&mut self, receiver: tokio::sync::watch::Receiver<mush_session::SyncState>) {
        self.sync = Some(receiver);
    }

    fn try_poll_sync(&mut self) -> Option<ClientEvent> {
        let receiver = self.sync.as_mut()?;
        if !receiver.has_changed().unwrap_or(false) {
            return None;
        }
        let next = receiver.borrow_and_update().clone();
        if next == self.sync_state {
            return None;
        }
        if next.read_only() && !self.sync_state.read_only() {
            self.relinquish = true;
        }
        self.sync_state = next.clone();
        Some(ClientEvent::SyncChanged(next))
    }

    async fn run_host_commands(&mut self) {
        let Some(commands) = self.host_commands.clone() else {
            return;
        };
        if self.turn_active() || self.compaction.is_some() {
            return;
        }
        for command in commands.drain() {
            let requested_compaction = matches!(command, ClientCommand::Compact { .. });
            match self.send(command).await {
                Ok(events) => self.queued.extend(events),
                Err(error) => {
                    if requested_compaction {
                        self.runtime.conversation.append_message(
                            mush_session::compact::outcome_message("skipped", &error.to_string()),
                        );
                        self.runtime.save_now_and_settle();
                    }
                    self.queued.push_back(status_event(error.to_string()));
                }
            }
        }
    }

    pub fn set_autonomous(&mut self, autonomous: Option<Arc<Autonomous>>) {
        self.autonomous = autonomous;
    }

    async fn continue_autonomous(&mut self) {
        let Some(autonomous) = self.autonomous.clone() else {
            return;
        };
        if !autonomous.due()
            || self.gate_task.is_some()
            || self.turn_active()
            || self.compaction.is_some()
            || self.runtime.pending_prompt.is_some()
        {
            return;
        }
        if self.children_running() {
            if autonomous.keep_alive_due() {
                self.apply_autonomous_decision(autonomous.keep_alive());
            }
            return;
        }
        if self.background_bash_running() {
            return;
        }
        let commands = autonomous.gate_commands();
        if commands.is_empty() {
            self.apply_autonomous_decision(autonomous.decide(Vec::new()));
            return;
        }
        let timeout = autonomous.gate_timeout();
        self.gate_task = Some(tokio::spawn(async move {
            autonomous.run_gates(commands, timeout).await
        }));
    }

    fn try_poll_autonomous(&mut self) -> Option<ClientEvent> {
        let runs = take_finished(&mut self.gate_task)?.unwrap_or_default();
        let autonomous = self.autonomous.clone()?;
        self.apply_autonomous_decision(autonomous.decide(runs));
        self.queued.pop_front()
    }

    fn apply_autonomous_decision(&mut self, decision: crate::autonomous::Decision) {
        let changed = !matches!(decision, crate::autonomous::Decision::Idle);
        if let crate::autonomous::Decision::Continue(prompt) = decision {
            self.queue_prompt(&prompt, Vec::new());
        }
        if changed && let Some(autonomous) = &self.autonomous {
            self.queued
                .push_back(ClientEvent::AutonomousChanged(Box::new(
                    autonomous.status(),
                )));
        }
    }

    pub fn set_notes(&mut self, notes: Option<Arc<Notes>>) {
        self.runtime.notes = notes.clone();
        self.notes = notes;
    }

    fn follow_rlm_max_depth(&mut self, could_spawn: bool) {
        let depth = self.runtime.rlm_max_depth;
        if let Some(family) = self.root_family() {
            family.set_max_depth(depth);
        }
        if could_spawn != (self.runtime.rlm_depth < depth) {
            self.rebuild_system_prompt();
        }
    }

    fn rebuild_system_prompt(&mut self) {
        let Some(build) = &self.system_prompt_builder else {
            return;
        };
        let prompt = build();
        self.runtime.system_prompt = Some(prompt.clone());
        self.queued
            .push_back(ClientEvent::Session(SessionEvent::SystemPromptChanged(
                Some(prompt),
            )));
    }

    fn rebind_artifacts(&mut self) {
        let persisted = self.runtime.saver.is_some();
        let dir = persisted
            .then(|| mush_session::SessionStore::default_artifact_dir(&self.runtime.session_id));
        if let Some(notes) = &self.notes {
            notes.rebind(dir.clone());
        }
        if let Some(family) = &self.family {
            family.rebind(
                self.runtime.session_id.to_string(),
                dir.as_ref().map(|dir| dir.join("subagents.json")),
            );
        }
        if let Some(goal) = &self.goal {
            goal.rebind(dir.as_ref().map(|dir| dir.join("goal.json")));
            self.known_goal = goal.get();
        }
        if let Some(scheduler) = &self.scheduler {
            scheduler.rebind(dir.as_ref().map(|dir| dir.join("scheduled-jobs.json")));
            self.known_jobs = scheduler.jobs();
        }
        self.known_children.clear();
        self.known_family_revision = None;
    }

    pub fn set_scheduler(&mut self, scheduler: Option<Arc<Scheduler>>) {
        self.known_jobs = scheduler
            .as_ref()
            .map(|scheduler| scheduler.jobs())
            .unwrap_or_default();
        self.scheduler = scheduler;
    }

    #[must_use]
    pub fn has_scheduled_jobs(&self) -> bool {
        self.scheduler
            .as_ref()
            .is_some_and(|scheduler| !scheduler.jobs().is_empty())
    }

    fn try_poll_schedule(&mut self) -> Option<ClientEvent> {
        let jobs = self.scheduler.as_ref()?.jobs();
        if jobs == self.known_jobs {
            return None;
        }
        self.known_jobs = jobs.clone();
        Some(ClientEvent::ScheduleChanged(jobs))
    }

    async fn poll_schedule(&mut self) {
        let Some(scheduler) = self.scheduler.clone() else {
            return;
        };
        let now = mush_ai::types::Timestamp::now().as_ms();
        for job in scheduler.take_due(now) {
            let text = job.prompt_text();
            if self.turn_active() {
                match job.delivery {
                    crate::schedule::Delivery::Steer => self.steer(text, Vec::new()).await,
                    crate::schedule::Delivery::FollowUp => self.deferred_jobs.push_back(text),
                }
            } else {
                self.deferred_jobs.push_back(text);
            }
        }
        if self.turn_active() || self.compaction.is_some() {
            return;
        }
        while let Some(text) = self.deferred_jobs.pop_front() {
            self.queue_prompt(&text, Vec::new());
        }
    }

    pub fn set_goal_store(&mut self, goal: Option<Arc<GoalStore>>) {
        self.known_goal = goal.as_ref().and_then(|goal| goal.get());
        self.goal = goal;
    }

    fn try_poll_goal(&mut self) -> Option<ClientEvent> {
        let goal = self.goal.as_ref()?.get();
        if goal == self.known_goal {
            return None;
        }
        self.known_goal = goal.clone();
        Some(ClientEvent::GoalChanged(goal.map(Box::new)))
    }

    fn children_running(&self) -> bool {
        match (&self.family_member, &self.family) {
            (Some((family, id)), _) => family.descendants_running(Some(id)),
            (None, Some(family)) => family.descendants_running(None),
            (None, None) => false,
        }
    }

    fn background_bash_running(&self) -> bool {
        self.runtime
            .kernel_state
            .as_ref()
            .is_some_and(|kernel| kernel.background_work())
    }

    fn wake_kernel_waits(&self) {
        if let Some(kernel) = &self.runtime.kernel_state {
            kernel.wake_waits();
        }
    }

    fn schedule_live(&self) -> bool {
        self.scheduler.as_ref().is_some_and(|scheduler| {
            scheduler.jobs().iter().any(|job| {
                matches!(
                    job.status,
                    crate::schedule::JobStatus::Active | crate::schedule::JobStatus::Paused
                )
            })
        })
    }

    fn goal_live(&self) -> bool {
        self.goal
            .as_ref()
            .and_then(|goal| goal.get())
            .is_some_and(|goal| goal.status != crate::goal::GoalStatus::Complete)
    }

    async fn parked(&self) -> bool {
        !self.closed
            && self.idle().await
            && self.deferred_deliveries.is_empty()
            && self.deferred_jobs.is_empty()
            && !self.runtime.kernel_restore_pending()
            && self.runtime.kernel_release.is_none()
            && self.side_questions.is_empty()
            && self.branch_summary.is_none()
            && self.refine.is_none()
            && self.auto_review.is_none()
            && self.gate_task.is_none()
            && self.title.is_none()
            && self.busy_reply.is_none()
            && !self.schedule_live()
            && !self.goal_live()
            && !self.children_running()
    }

    pub async fn release_idle_kernel(&mut self) {
        let Some(kernel) = self.runtime.kernel_state.clone() else {
            return;
        };
        if self.runtime.kernel_released
            || self.runtime.kernel_release.is_some()
            || kernel.pid().is_none()
            || !self.parked().await
        {
            return;
        }
        self.runtime.kernel_release = Some(tokio::spawn(async move { kernel.release().await }));
    }

    fn settle_kernel_release(&mut self) {
        let Some(outcome) = take_finished(&mut self.runtime.kernel_release) else {
            return;
        };
        match outcome {
            Ok(mush_kernel::Release::Released(report)) => {
                tracing::info!(
                    session = %self.runtime.session_id,
                    saved = report.saved.len(),
                    skipped = report.skipped.len(),
                    "released an idle kernel"
                );
                self.runtime.kernel_released = true;
            }
            Ok(mush_kernel::Release::Failed(error)) => {
                tracing::warn!(%error, "kept an idle kernel whose state could not be saved");
            }
            Ok(mush_kernel::Release::NotRunning | mush_kernel::Release::Busy) => {}
            Err(error) => tracing::warn!(%error, "the kernel release task failed"),
        }
    }

    async fn continue_goal(&mut self) {
        let Some(goal) = self.goal.clone() else {
            return;
        };
        if self.turn_active() {
            if let Some(prompt) = goal.take_turn_notice() {
                self.steer(prompt, Vec::new()).await;
            }
            return;
        }
        if self.compaction.is_some()
            || self.runtime.pending_prompt.is_some()
            || self.children_running()
            || self.background_bash_running()
        {
            return;
        }
        if let Some(prompt) = goal.take_due_prompt() {
            self.queue_prompt(&prompt, Vec::new());
        }
    }

    fn try_poll_family(&mut self) -> Option<ClientEvent> {
        let family = self.family.clone()?;
        if let Some(note) = family.take_root_note() {
            return Some(ClientEvent::ProgressNote(note));
        }
        let revision = family.revision();
        if self.known_family_revision == Some(revision) {
            return None;
        }
        for child in family.all() {
            let changed = self
                .known_children
                .get(&child.id)
                .is_none_or(|known| *known != child);
            if changed {
                self.known_children.insert(child.id.clone(), child.clone());
                self.queued
                    .push_back(ClientEvent::ChildUpdated(Box::new(child)));
            }
        }
        self.known_family_revision = Some(revision);
        self.queued.pop_front()
    }

    pub fn set_family(&mut self, family: Option<Arc<Family>>) {
        self.family = family;
        self.known_family_revision = None;
    }

    fn root_family(&self) -> Option<&Arc<Family>> {
        self.family
            .as_ref()
            .filter(|_| self.family_member.is_none())
    }

    fn mark_root_running(&self, running: bool) {
        if let Some(family) = self.root_family() {
            family.set_root_running(running);
        }
    }

    pub fn set_family_member(&mut self, member: Option<(Arc<Family>, ChildId)>) {
        self.family_member = member;
    }

    #[must_use]
    pub fn family(&self) -> Option<&Arc<Family>> {
        self.family.as_ref()
    }

    pub fn attach_agent_inbox(&mut self, inbox: AgentInbox) {
        self.agent_inbox = Some(inbox);
    }

    pub fn attach_observe_inbox(&mut self, inbox: ObserveInbox) {
        self.observe_inbox = Some(inbox);
    }

    fn answer_observers(&mut self) {
        let Some(inbox) = self.observe_inbox.as_mut() else {
            return;
        };
        while let Ok(request) = inbox.try_recv() {
            let messages = self.runtime.conversation.display_history();
            let _ = request.reply.send(crate::family::preview_messages(
                &messages,
                request.limit,
                request.max_chars,
            ));
        }
    }

    #[must_use]
    pub fn default_model(&self) -> mush_ai::types::Model {
        self.config.default_model.clone()
    }

    async fn poll_agent_inbox(&mut self) {
        if let Some(inbox) = self.agent_inbox.as_mut() {
            while let Ok(delivery) = inbox.try_recv() {
                self.deferred_deliveries.push_back(delivery);
            }
        }
        if self.deferred_deliveries.is_empty() {
            return;
        }
        let busy = self.turn_active();
        let mut held = VecDeque::new();
        while let Some(mut delivery) = self.deferred_deliveries.pop_front() {
            use crate::family::Receipt;
            if held.len() >= crate::family::MAX_PENDING_MESSAGES {
                delivery.ack(Receipt::Rejected);
                continue;
            }
            if busy {
                self.steer(delivery.event.text(), Vec::new()).await;
                self.wake_kernel_waits();
                delivery.ack(Receipt::Queued);
            } else if self.runtime.pending_prompt.is_some() {
                delivery.ack(Receipt::Queued);
                held.push_back(delivery);
            } else {
                self.runtime.accept_event(delivery.event.clone());
                self.queued
                    .push_back(ClientEvent::MessageInjected(delivery.event.clone()));
                delivery.ack(Receipt::Delivered);
            }
        }
        self.deferred_deliveries = held;
    }

    pub fn set_system_prompt_builder(&mut self, builder: Option<SystemPromptBuilder>) {
        self.system_prompt_builder = builder;
    }

    pub fn attach_kernel(&mut self, signals: Option<KernelSignals>) {
        self.kernel = signals;
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
                provider_api_keys: HashMap::new(),
                confirm_tools: false,
                auto_compact: false,
                compaction: Default::default(),
                branch_summary: Default::default(),
                auto_refine: Default::default(),
                auto_fork_compact: false,
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
        self.runtime.turns.meta(PRIMARY_PANE).is_some()
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

    pub fn attach_inbox(&mut self, inbox: OrchestratorInbox) {
        self.inbox = Some(inbox);
    }

    pub fn attach_inbox_from_env(&mut self) {
        let cwd = self
            .injections
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        match OrchestratorInbox::from_env(&self.runtime, &cwd) {
            Ok(Some(inbox)) => self.inbox = Some(inbox),
            Ok(None) => {}
            Err(error) => self.report_inbox_disabled(&error),
        }
    }

    #[must_use]
    pub fn inbox(&self) -> Option<&OrchestratorInbox> {
        self.inbox.as_ref()
    }

    pub fn inbox_mut(&mut self) -> Option<&mut OrchestratorInbox> {
        self.inbox.as_mut()
    }

    fn report_inbox_disabled(&mut self, error: &std::io::Error) {
        tracing::warn!(%error, "orchestrator inbox disabled");
        self.queued.push_back(status_event(format!(
            "orchestrator inbox disabled: {error}. restart to reconnect"
        )));
    }

    pub async fn poll_inbox(&mut self) -> bool {
        if self.inbox.is_none() {
            return true;
        }
        let idle = self.idle().await;
        let Some(inbox) = self.inbox.as_mut() else {
            return true;
        };
        match inbox.poll(&mut self.runtime, idle).await {
            Ok(crate::inbox::Delivered::Ready) => true,
            Ok(crate::inbox::Delivered::Staged) => false,
            Ok(crate::inbox::Delivered::Injected(message)) => {
                self.queued.push_back(ClientEvent::MessageInjected(message));
                false
            }
            Err(error) => {
                if let Some(inbox) = self.inbox.take() {
                    inbox.disconnect(&mut self.runtime);
                }
                self.report_inbox_disabled(&error);
                true
            }
        }
    }

    pub async fn start_pending(&mut self) -> bool {
        if std::mem::take(&mut self.relinquish) && self.turn_active() {
            let events = self.abort().await;
            self.queued.extend(events);
        }
        if self.sync_state.read_only() {
            self.answer_observers();
            return false;
        }
        if self.runtime.paused.is_some() {
            self.answer_observers();
            self.runtime.save_if_dirty();
            return false;
        }
        if !self.poll_inbox().await {
            self.runtime.save_if_dirty();
            return false;
        }
        self.poll_control().await;
        self.answer_observers();
        self.poll_agent_inbox().await;
        self.submit_kernel_notices().await;
        self.poll_schedule().await;
        self.maybe_auto_refine().await;
        self.run_host_commands().await;
        self.continue_goal().await;
        self.continue_autonomous().await;
        if std::mem::take(&mut self.fork_compaction_due) {
            let _ = self
                .start_compaction(CompactionFocus::default(), true)
                .await;
        }
        self.deliver_kernel_compaction_notice().await;
        let started = self.start_pending_prompt().await;
        self.runtime.save_if_dirty();
        started
    }

    pub async fn idle(&self) -> bool {
        !self.turn_active()
            && self.compaction.is_none()
            && self.runtime.pending_prompt.is_none()
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
        self.queued
            .pop_front()
            .or_else(|| self.try_poll_sync())
            .or_else(|| self.try_poll_confirm())
            .or_else(|| self.try_poll_title())
            .or_else(|| self.try_poll_kernel())
            .or_else(|| self.try_poll_kernel_restore())
            .or_else(|| self.try_poll_side_questions())
            .or_else(|| self.try_poll_branch_summary())
            .or_else(|| self.try_poll_refine())
            .or_else(|| self.try_poll_auto_review())
            .or_else(|| self.try_poll_family())
            .or_else(|| self.try_poll_goal())
            .or_else(|| self.try_poll_schedule())
            .or_else(|| self.try_poll_autonomous())
            .or_else(|| self.try_poll_compaction())
    }

    async fn summarise_and_navigate(&mut self, entry_id: mush_session::tree::EntryId) {
        let abandoned = {
            let tree = self.runtime.conversation.tree();
            let Some(target) = tree.by_id(&entry_id) else {
                self.queued
                    .push_back(failed_event("that message is no longer in the tree"));
                return;
            };
            tree.abandoned_by_moving_to(crate::commands::navigation_leaf(tree, target).as_ref())
        };
        if abandoned.is_empty() || self.branch_summary.is_some() {
            let events = self.runtime.navigate_tree(&entry_id, None);
            self.runtime.save_now_and_settle();
            self.queued
                .extend(events.into_iter().map(ClientEvent::Session));
            return;
        }
        let settings = self.config.branch_summary;
        let target = self.runtime.compaction_target(&self.config).await;
        let window = target.window;
        let target = target.fitting(|| {
            mush_session::compact::branch_summary_request_tokens(&abandoned, window, &settings)
        });
        let registry = self.registry.clone();
        let task = tokio::spawn(async move {
            mush_session::compact::summarise_branch(
                &abandoned,
                &registry,
                &target.model,
                &target.options,
                &settings,
            )
            .await
        });
        self.branch_summary = Some((entry_id, task));
        self.queued
            .push_back(status_event("summarising the branch you're leaving…"));
    }

    fn try_poll_branch_summary(&mut self) -> Option<ClientEvent> {
        let (entry_id, outcome) = take_finished_keyed(&mut self.branch_summary)?;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => Err(format!("branch summary task failed: {error}")),
        };
        let mut events = match outcome {
            Ok(summary) => self.runtime.navigate_tree(&entry_id, Some(summary)),
            Err(error) => {
                let mut events = self.runtime.navigate_tree(&entry_id, None);
                events.push(SessionEvent::Status(format!(
                    "branch summary failed ({error}), branched without it"
                )));
                events
            }
        };
        self.runtime.save_now_and_settle();
        let first = events.remove(0);
        self.queued
            .extend(events.into_iter().map(ClientEvent::Session));
        Some(ClientEvent::Session(first))
    }

    fn try_poll_side_questions(&mut self) -> Option<ClientEvent> {
        let index = self
            .side_questions
            .iter()
            .position(|(_, task)| task.is_finished())?;
        let answer = (&mut self.side_questions[index].1).now_or_never()?;
        let (question, _) = self.side_questions.remove(index);
        let answer = match answer {
            Ok(answer) => answer,
            Err(error) => Err(format!("side question task failed: {error}")),
        };
        Some(ClientEvent::SideAnswer { question, answer })
    }

    fn ask_side_question(&mut self, question: String) {
        let model = self.runtime.model(&self.config.default_model);
        let cwd = self.injections.cwd.clone().unwrap_or_default();
        let tools = aligned_tools(&self.tools, &model, &self.config, cwd);
        let context = crate::side_question::side_question_context(
            self.runtime.live_context(&tools, &model),
            self.runtime.conversation.context(),
            &question,
        );
        let registry = self.registry.clone();
        let options = self.runtime.call_options(&self.config);
        let keys = self.config.provider_api_keys.clone();
        let account = self.runtime.account_for(&model);
        let task = tokio::spawn(async move {
            let options = resolve_auth_into_options(&model, options, &keys, account).await;
            crate::side_question::answer(&registry, &model, context, &options).await
        });
        self.side_questions.push((question, task));
    }

    fn try_poll_kernel(&mut self) -> Option<ClientEvent> {
        if self
            .busy_reply
            .as_ref()
            .is_some_and(tokio::sync::oneshot::Sender::is_closed)
        {
            self.busy_reply = None;
            return Some(ClientEvent::KernelBusyWithdrawn);
        }
        let kernel = self.kernel.as_mut()?;
        while let Ok((exec_id, line)) = kernel.output.try_recv() {
            self.queued
                .push_back(ClientEvent::ToolOutput { exec_id, line });
        }
        let mut latest = None;
        while let Ok(reply) = kernel.busy.try_recv() {
            latest = Some(reply);
        }
        if let Some(reply) = latest.filter(|reply| !reply.is_closed()) {
            self.busy_reply = Some(reply);
            self.queued.push_back(ClientEvent::KernelBusy);
        }
        self.queued.pop_front()
    }

    fn try_poll_kernel_restore(&mut self) -> Option<ClientEvent> {
        let status = self.runtime.settle_kernel_restore()?;
        self.runtime.save_now_and_settle();
        Some(status_event(status))
    }

    async fn submit_kernel_notices(&mut self) {
        let mut notices = Vec::new();
        if let Some(kernel) = self.kernel.as_mut() {
            while let Ok(notice) = kernel.notices.try_recv() {
                notices.push(notice);
            }
        }
        for notice in notices {
            let text = notice.done.text();
            self.queued.push_back(ClientEvent::BashDone(notice));
            if self.turn_active() {
                self.steer(text, Vec::new()).await;
            } else {
                self.queue_prompt(&text, Vec::new());
            }
        }
    }

    fn try_poll_title(&mut self) -> Option<ClientEvent> {
        let title = take_finished(&mut self.title)?.ok().flatten()?;
        self.runtime.title = Some(title.clone());
        self.runtime.save_debounced();
        Some(ClientEvent::Session(SessionEvent::Title(Some(title))))
    }

    fn select_account(
        &mut self,
        model: &mush_ai::types::Model,
        label: Option<mush_ai::types::AccountLabel>,
    ) -> Result<String, ConnectionError> {
        let oauth_provider =
            mush_ai::auth::oauth_provider_id(&model.provider).ok_or_else(|| {
                ConnectionError::Refused(format!("{} has no account logins", model.provider))
            })?;
        let store = mush_ai::oauth::load_credentials()
            .map_err(|error| ConnectionError::Refused(error.to_string()))?;
        let accounts = store.accounts(oauth_provider).ok_or_else(|| {
            ConnectionError::Refused(format!("no {oauth_provider} accounts, run /login first"))
        })?;
        let chosen = match &label {
            Some(label) => accounts
                .get(Some(label))
                .ok_or_else(|| {
                    let known: Vec<&str> = accounts.labels().map(|label| label.as_str()).collect();
                    ConnectionError::Refused(format!(
                        "no {oauth_provider} account called {label}, known accounts: {}",
                        known.join(", ")
                    ))
                })?
                .0
                .clone(),
            None => accounts
                .default_label()
                .cloned()
                .unwrap_or_else(mush_ai::types::AccountLabel::default_label),
        };
        let mut session_accounts = self.runtime.accounts.clone();
        session_accounts.set(oauth_provider, label);
        self.runtime.set_accounts(session_accounts);
        self.runtime.save_now_and_settle();
        Ok(format!("{oauth_provider} account: {chosen}"))
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
        let oauth_account = self.runtime.account_for(&model);
        self.title = Some(tokio::spawn(async move {
            let (api_key, account_id) =
                resolve_auth_for_model(&model, &keys, oauth_account.as_ref()).await;
            let options = mush_ai::types::StreamOptions {
                api_key,
                account_id,
                oauth_account,
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
        let was_active = self.turn_active();
        let cancelled = self.runtime.record_cancelled_tools();
        self.runtime.record_aborted_partial();
        self.runtime.turns.abort(PRIMARY_PANE);
        self.runtime.steering_queue.lock().await.clear();
        self.runtime.save_debounced();
        let mut events = Vec::new();
        if !cancelled.is_empty() {
            events.push(ClientEvent::ToolsCancelled(cancelled));
        }
        if was_active {
            events.push(ClientEvent::TurnEnded { cancelled: true });
        }
        if let Some(compaction) = self.compaction.take() {
            compaction.abort();
            self.compaction_auto = false;
            events.push(ClientEvent::CompactionCancelled);
        }
        events
    }

    async fn take_waiting_messages(&mut self) -> Vec<Message> {
        if let Some(inbox) = self.agent_inbox.as_mut() {
            while let Ok(delivery) = inbox.try_recv() {
                self.deferred_deliveries.push_back(delivery);
            }
        }
        let mut waiting: Vec<Message> = self
            .runtime
            .steering_queue
            .lock()
            .await
            .drain(..)
            .filter(|message| matches!(message, Message::User(_) | Message::Event(_)))
            .collect();
        waiting.extend(self.deferred_deliveries.drain(..).map(|mut delivery| {
            delivery.ack(crate::family::Receipt::Queued);
            Message::Event(delivery.event)
        }));
        waiting
    }

    async fn take_user_steers(&mut self) -> Vec<Message> {
        self.runtime
            .steering_queue
            .lock()
            .await
            .drain(..)
            .filter(|message| matches!(message, Message::User(_)))
            .collect()
    }

    async fn send_steers(&mut self, steers: Vec<Message>) {
        if steers.is_empty() || self.compaction.is_some() {
            return;
        }
        for steer in steers {
            self.runtime.conversation.append_message(steer);
        }
        if self.runtime.pending_prompt.is_none() {
            self.runtime.pending_prompt = Some(PendingPrompt::persisted(""));
        }
        self.start_pending_prompt().await;
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

    async fn start_pending_prompt(&mut self) -> bool {
        self.settle_kernel_release();
        if self.runtime.kernel_released
            && self.runtime.pending_prompt.is_some()
            && !self.turn_active()
        {
            self.runtime.begin_kernel_restore();
        }
        if self.turn_active()
            || self.compaction.is_some()
            || self.runtime.kernel_release.is_some()
            || self.runtime.kernel_restore_pending()
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
            goal: self
                .goal
                .as_ref()
                .and_then(|goal| goal.get())
                .map(|goal| goal.objective),
        };
        self.runtime.start_prepared_turn(request, prep, &deps);
        self.runtime.save_now_and_settle();
        self.queued.push_back(ClientEvent::TurnStarted);
        self.mark_root_running(true);
        true
    }

    async fn prompt(&mut self, text: String, images: Vec<PendingImage>) {
        if self.runtime.paused.take().is_some() {
            self.runtime.save_now_and_settle();
            self.queued.push_back(ClientEvent::PausedChanged(None));
        }
        if self.turn_active() {
            self.steer(text, images).await;
            self.wake_kernel_waits();
        } else {
            self.queue_prompt(&text, images);
            self.start_pending_prompt().await;
        }
    }

    async fn handle_session(&mut self, command: SessionCommand) {
        let before = self.runtime.session_id.clone();
        let could_spawn = self.runtime.rlm_depth < self.runtime.rlm_max_depth;
        let events = self.runtime.handle(command).await;
        self.sync_control_target();
        if self.runtime.session_id != before {
            self.rebind_artifacts();
        }
        self.runtime.save_now_and_settle();
        self.queued
            .extend(events.into_iter().map(ClientEvent::Session));
        self.follow_rlm_max_depth(could_spawn);
    }

    async fn shutdown(&mut self) {
        if let Some(family) = self.family.clone() {
            family.shutdown().await;
        }
        let waiting = self.take_waiting_messages().await;
        self.abort().await;
        for message in waiting {
            self.runtime.conversation.append_message(message);
        }
        self.runtime.snapshot_kernel(&self.runtime.session_id).await;
        self.runtime.save_now_and_settle();
        self.closed = true;
    }

    fn queue_prompt(&mut self, text: &str, images: Vec<PendingImage>) {
        match &mut self.runtime.pending_prompt {
            Some(pending) => pending.merge(text, images),
            None => self.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images)),
        }
    }

    async fn steer(&mut self, text: String, images: Vec<PendingImage>) {
        let Some(meta) = self.runtime.turns.meta(PRIMARY_PANE) else {
            tracing::info!(
                turn_active = self.runtime.turn_active(),
                "steer arrived with no active turn, queued as the next prompt"
            );
            self.queue_prompt(&text, images);
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
            AgentEvent::Error { error } => {
                self.turn_error = Some(error.clone());
            }
            AgentEvent::MessageEnd { message } => {
                self.turn_error =
                    (message.stop_reason == mush_ai::types::StopReason::Error).then(|| {
                        message
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "assistant response failed".into())
                    });
                if let Some(meta) = self.runtime.turns.meta(PRIMARY_PANE) {
                    meta.context_tokens.store(
                        message.usage.total_input_tokens().get(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                if let Some(commands) = &self.host_commands {
                    commands.record_context(ContextUsage {
                        tokens: Some(message.usage.total_input_tokens().get()),
                        context_window: Some(
                            self.runtime
                                .model(&self.config.default_model)
                                .compaction_window()
                                .get(),
                        ),
                    });
                }
                if let Some(goal) = &self.goal {
                    goal.record_usage(crate::goal::usage_tokens(&message.usage));
                }
                if let Some(autonomous) = &self.autonomous {
                    autonomous.record_usage(crate::autonomous::usage_tokens(&message.usage));
                    autonomous.record_assistant_turn();
                }
                self.assistant_turns_since_review += 1;
                self.runtime.save_debounced();
            }
            AgentEvent::ContextCompacted { .. } => {
                self.compact_refine_pending = true;
                self.runtime.save_debounced();
            }
            AgentEvent::ToolExecEnd { .. } => {
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
                self.mark_root_running(false);
                self.fork_compaction_due = !cancelled && self.fork_compaction_due();
                let failed = self.turn_error.take();
                if !cancelled && let Some(goal) = &self.goal {
                    match &failed {
                        Some(error) => goal.turn_failed(error),
                        None => goal.turn_ended(),
                    }
                }
                if !cancelled
                    && failed.is_none()
                    && let Some(autonomous) = &self.autonomous
                {
                    autonomous.turn_ended();
                }
            }
            _ => {}
        }
    }
}

fn changes_the_session(command: &ClientCommand) -> bool {
    match command {
        ClientCommand::Prompt { .. }
        | ClientCommand::QueuePrompt { .. }
        | ClientCommand::Steer { .. }
        | ClientCommand::AbortAndSendQueued
        | ClientCommand::SetModel(_)
        | ClientCommand::SetSubagentModel(_)
        | ClientCommand::SetAccount(_)
        | ClientCommand::SetPaused(_)
        | ClientCommand::SetThinking(_)
        | ClientCommand::SetRlmMaxDepth(_)
        | ClientCommand::SetSystemPrompt(_)
        | ClientCommand::ReloadSystemPrompt
        | ClientCommand::Session(_)
        | ClientCommand::SummariseAndNavigate { .. }
        | ClientCommand::Refine(_)
        | ClientCommand::Compact { .. }
        | ClientCommand::CompactionRetry
        | ClientCommand::CompactionFallback
        | ClientCommand::Goal(_)
        | ClientCommand::Schedule(_)
        | ClientCommand::Autonomous(_) => true,
        ClientCommand::RetractSteer { .. }
        | ClientCommand::Abort
        | ClientCommand::ConfirmTool { .. }
        | ClientCommand::SetStreamOptions(_)
        | ClientCommand::SetAutoCompact(_)
        | ClientCommand::SetCompactionModel(_)
        | ClientCommand::AnswerKernelBusy(_)
        | ClientCommand::SideQuestion(_)
        | ClientCommand::StopChild { .. }
        | ClientCommand::Shutdown => false,
    }
}

#[async_trait::async_trait]
impl AgentConnection for InProcessConnection {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    async fn send(&mut self, command: ClientCommand) -> Result<Vec<ClientEvent>, ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        if let Some(holder) = &self.sync_state.taken_by
            && changes_the_session(&command)
        {
            return Err(ConnectionError::Refused(format!(
                "this session is open on {holder} now, so it is read-only here. close this window and carry on there, or run `mush sync take {}` to bring it back",
                self.runtime.session_id
            )));
        }
        let before = self.queued.len();
        match command {
            ClientCommand::Prompt { text, images } => self.prompt(text, images).await,
            ClientCommand::QueuePrompt { text, images } => self.queue_prompt(&text, images),
            ClientCommand::Steer { text, images } => {
                self.steer(text, images).await;
                self.wake_kernel_waits();
            }
            ClientCommand::RetractSteer { text } => {
                if !self.retract_steer(&text).await {
                    tracing::warn!(%text, "no steering queue entry matched the retracted message");
                }
            }
            ClientCommand::Abort => {
                let events = self.abort().await;
                self.queued.extend(events);
            }
            ClientCommand::AbortAndSendQueued => {
                let steers = self.take_user_steers().await;
                let events = self.abort().await;
                self.queued.extend(events);
                self.send_steers(steers).await;
            }
            ClientCommand::ConfirmTool { allow } => self.confirm(allow).await,
            ClientCommand::AnswerKernelBusy(choice) => {
                if let Some(reply) = self.busy_reply.take() {
                    let _ = reply.send(choice);
                }
            }
            ClientCommand::SideQuestion(question) => self.ask_side_question(question),
            ClientCommand::Autonomous(command) => {
                let autonomous = self.autonomous.clone().ok_or_else(|| {
                    ConnectionError::Refused("autonomous mode is not available".into())
                })?;
                let status = autonomous.apply(command);
                self.queued
                    .push_back(ClientEvent::AutonomousChanged(Box::new(status)));
            }
            ClientCommand::Schedule(command) => {
                let scheduler = self.scheduler.clone().ok_or_else(|| {
                    ConnectionError::Refused("schedules are not available".into())
                })?;
                let jobs = scheduler.apply(command).map_err(ConnectionError::Refused)?;
                self.known_jobs = jobs.clone();
                self.queued.push_back(ClientEvent::ScheduleChanged(jobs));
            }
            ClientCommand::Goal(command) => {
                let goal = self
                    .goal
                    .clone()
                    .ok_or_else(|| ConnectionError::Refused("goals are not available".into()))?;
                let updated = goal.apply(command).map_err(ConnectionError::Refused)?;
                self.known_goal = updated.clone();
                self.queued
                    .push_back(ClientEvent::GoalChanged(updated.map(Box::new)));
            }
            ClientCommand::SetModel(id) => {
                self.runtime.models.set_current(id.clone());
                self.runtime.model_id = id;
            }
            ClientCommand::SetSubagentModel(id) => {
                self.runtime.models.set_subagent(id.clone());
                self.runtime.subagent_model = id;
                self.runtime.save_now_and_settle();
            }
            ClientCommand::SetAccount(label) => {
                let model = self.runtime.model(&self.config.default_model);
                let status = self.select_account(&model, label)?;
                self.queued
                    .push_back(ClientEvent::Session(SessionEvent::Accounts(
                        self.runtime.accounts.clone(),
                    )));
                self.queued.push_back(status_event(status));
            }
            ClientCommand::SetPaused(pause) => {
                if self.runtime.paused != pause {
                    self.runtime.paused = pause.clone();
                    self.runtime.save_now_and_settle();
                    self.queued.push_back(ClientEvent::PausedChanged(pause));
                }
            }
            ClientCommand::SetThinking(level) => {
                self.runtime.models.set_thinking(level.clone());
                self.runtime.thinking_level = level;
                if !self.turn_active()
                    && self.runtime.conversation.leaf_id().is_some()
                    && self.runtime.record_thinking_level()
                {
                    self.runtime.save_debounced();
                }
            }
            ClientCommand::SetRlmMaxDepth(depth) => {
                let could_spawn = self.runtime.rlm_depth < self.runtime.rlm_max_depth;
                self.runtime.set_rlm_max_depth(depth);
                self.follow_rlm_max_depth(could_spawn);
            }
            ClientCommand::SetSystemPrompt(prompt) => self.runtime.system_prompt = prompt,
            ClientCommand::ReloadSystemPrompt => self.rebuild_system_prompt(),
            ClientCommand::SetStreamOptions(options) => self.config.options = *options,
            ClientCommand::SetAutoCompact(on) => self
                .runtime
                .auto_compact
                .store(on, std::sync::atomic::Ordering::Relaxed),
            ClientCommand::SetCompactionModel(model) => {
                self.config.compaction_model = model.map(|boxed| *boxed);
            }
            ClientCommand::Session(command) => self.handle_session(command).await,
            ClientCommand::SummariseAndNavigate { entry_id } => {
                self.summarise_and_navigate(entry_id).await;
            }
            ClientCommand::Refine(request) => self.start_refine(request).await?,
            ClientCommand::Compact { focus, auto } => self.start_compaction(focus, auto).await?,
            ClientCommand::CompactionRetry => self.retry_compaction().await?,
            ClientCommand::CompactionFallback => {
                let event = self.fallback_compaction().await?;
                self.queued.push_back(event);
            }
            ClientCommand::StopChild { session_id } => {
                let family = self.family.as_ref().ok_or_else(|| {
                    ConnectionError::Refused("this agent has no subagents".into())
                })?;
                family
                    .stop_session(&session_id)
                    .map_err(ConnectionError::Refused)?;
            }
            ClientCommand::Shutdown => self.shutdown().await,
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

    async fn tick(&mut self) {
        self.start_pending().await;
    }

    fn closed(&self) -> bool {
        self.closed
    }

    fn state(&self) -> ConnectionState {
        ConnectionState {
            session_id: self.runtime.session_id.clone(),
            title: self.runtime.title.clone(),
            model_id: self.runtime.model_id.clone(),
            subagent_model: self.runtime.subagent_model.clone(),
            accounts: self.runtime.accounts.clone(),
            paused: self.runtime.paused.clone(),
            thinking_level: self.runtime.thinking_level.clone(),
            context_tokens: self.runtime.context_tokens,
            context_len: self.runtime.conversation.context_len(),
            tree_len: self.runtime.conversation.tree().len(),
            rlm_depth: self.runtime.rlm_depth,
            rlm_max_depth: self.runtime.rlm_max_depth,
            rlm_max_depth_source: self.runtime.rlm_max_depth_source,
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
            last_request: mush_ai::request_snapshot::last_request(self.runtime.session_id.as_str()),
            cwd: self.injections.cwd.clone(),
            sync: self.sync_state.clone(),
            auto_compact: self
                .runtime
                .auto_compact
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    async fn transcript(&mut self) -> Vec<Message> {
        self.runtime.conversation.context()
    }

    async fn history(&mut self) -> Vec<Message> {
        self.runtime.conversation.display_history()
    }

    async fn recent_history(&mut self, limit: usize) -> mush_session::tree::RecentHistory {
        self.runtime
            .conversation
            .tree()
            .recent_display_history(limit)
    }

    async fn tree_summary(&mut self) -> Vec<mush_session::tree::TreeSummaryRow> {
        self.runtime.conversation.tree().summary()
    }

    async fn navigation_impact(&mut self, entry_id: &mush_session::tree::EntryId) -> usize {
        let tree = self.runtime.conversation.tree();
        let Some(target) = tree.by_id(entry_id) else {
            return 0;
        };
        tree.abandoned_by_moving_to(crate::commands::navigation_leaf(tree, target).as_ref())
            .len()
    }

    async fn system_prompt(&mut self) -> Option<String> {
        self.runtime.system_prompt.clone()
    }
}

#[cfg(test)]
mod tests;
