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
    TurnConfig, TurnDeps, TurnRequest, build_user_content, resolve_auth_for_model,
    resolve_auth_into_options,
};

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
    goal: Option<Arc<GoalStore>>,
    known_goal: Option<Goal>,
    scheduler: Option<Arc<Scheduler>>,
    known_jobs: Vec<Job>,
    deferred_jobs: VecDeque<String>,
    notes: Option<Arc<Notes>>,
    autonomous: Option<Arc<Autonomous>>,
    gate_task: Option<tokio::task::JoinHandle<Vec<crate::autonomous::GateRun>>>,
    host_commands: Option<Arc<HostCommands>>,
    closed: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: Option<u64>,
}

impl ContextUsage {
    #[must_use]
    pub fn percent(&self) -> Option<f64> {
        let (tokens, window) = (self.tokens?, self.context_window?);
        (window > 0).then(|| tokens as f64 * 100.0 / window as f64)
    }
}

#[derive(Default)]
pub struct HostCommands {
    queue: std::sync::nonpoison::Mutex<VecDeque<ClientCommand>>,
    context: std::sync::nonpoison::Mutex<ContextUsage>,
}

impl HostCommands {
    pub fn record_context(&self, usage: ContextUsage) {
        *self.context.lock() = usage;
    }

    #[must_use]
    pub fn context_usage(&self) -> ContextUsage {
        *self.context.lock()
    }

    #[must_use]
    pub fn compaction_queued(&self) -> bool {
        self.queue
            .lock()
            .iter()
            .any(|command| matches!(command, ClientCommand::Compact { .. }))
    }

    pub fn push(&self, command: ClientCommand) {
        self.queue.lock().push_back(command);
    }

    fn drain(&self) -> Vec<ClientCommand> {
        self.queue.lock().drain(..).collect()
    }

    pub fn replace_refine(&self, request: mush_session::refine::RefineRequest) {
        let mut queue = self.queue.lock();
        queue.retain(|command| !matches!(command, ClientCommand::Refine(_)));
        queue.push_back(ClientCommand::Refine(request));
    }

    #[must_use]
    pub fn pending_refine(&self) -> bool {
        self.queue
            .lock()
            .iter()
            .any(|command| matches!(command, ClientCommand::Refine(_)))
    }

    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.lock().len()
    }
}

pub type SystemPromptBuilder = Arc<dyn Fn() -> String + Send + Sync>;

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
            goal: None,
            known_goal: None,
            scheduler: None,
            known_jobs: Vec::new(),
            deferred_jobs: VecDeque::new(),
            notes: None,
            autonomous: None,
            gate_task: None,
            host_commands: None,
            closed: false,
        }
    }

    pub fn set_host_commands(&mut self, commands: Option<Arc<HostCommands>>) {
        self.host_commands = commands;
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
                    self.queued
                        .push_back(ClientEvent::Session(SessionEvent::Status(
                            error.to_string(),
                        )));
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
        if self.family_member.is_none()
            && let Some(family) = &self.family
        {
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
            notes.rebind(dir.as_ref().map(|dir| dir.join("notes.md")));
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
        for child in family.all() {
            let changed = self
                .known_children
                .get(&child.id)
                .is_none_or(|known| *known != child);
            if changed {
                self.known_children.insert(child.id.clone(), child.clone());
                return Some(ClientEvent::ChildUpdated(Box::new(child)));
            }
        }
        None
    }

    pub fn set_family(&mut self, family: Option<Arc<Family>>) {
        self.family = family;
    }

    fn mark_root_running(&self, running: bool) {
        if self.family_member.is_none()
            && let Some(family) = &self.family
        {
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
        let mut requests = Vec::new();
        while let Ok(request) = inbox.try_recv() {
            requests.push(request);
        }
        for request in requests {
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
        self.queued
            .push_back(ClientEvent::Session(SessionEvent::Status(format!(
                "orchestrator inbox disabled: {error}. restart to reconnect"
            ))));
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

    async fn deliver_kernel_compaction_notice(&mut self) {
        if self.turn_active() || !std::mem::take(&mut self.kernel_compaction_notice_due) {
            return;
        }
        if let Some(notice) =
            crate::kernel_notice::after_compaction(self.runtime.kernel_state.as_deref()).await
        {
            self.runtime.conversation.append_message(notice);
            self.runtime.save_now_and_settle();
        }
    }

    fn fork_compaction_due(&self) -> bool {
        if !self.config.auto_fork_compact {
            return false;
        }
        let window = self
            .runtime
            .model(&self.config.default_model)
            .compaction_window();
        mush_session::compact::needs_compaction_for_messages(
            self.runtime.context_tokens,
            window,
            &self.runtime.conversation.context(),
            &self.config.compaction,
        )
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
                    .push_back(ClientEvent::Session(SessionEvent::Failed(
                        "that message is no longer in the tree".into(),
                    )));
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
        let required = mush_session::compact::branch_summary_request_tokens(
            &abandoned,
            target.window,
            &settings,
        );
        let target = target.fitting(required);
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
            .push_back(ClientEvent::Session(SessionEvent::Status(
                "summarising the branch you're leaving…".into(),
            )));
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

    async fn start_refine(
        &mut self,
        request: mush_session::refine::RefineRequest,
    ) -> Result<(), ConnectionError> {
        if !self.runtime.harness_enabled() {
            return Err(ConnectionError::Refused(
                "the continual harness needs a saved session (not --no-session)".into(),
            ));
        }
        if self.refine.is_some() {
            return Err(ConnectionError::Refused(
                "a refinement is already running".into(),
            ));
        }
        let target = self.runtime.compaction_target(&self.config).await;
        let messages = self.runtime.conversation.context();
        let history = self.runtime.refinement_history();
        let session_id = self.runtime.session_id.clone();
        let registry = self.registry.clone();
        let rollback = request.rollback.is_some();
        let thinking = self.runtime.thinking_level.clone();
        self.refine = Some(tokio::spawn(crate::refine::plan_refinement(
            messages,
            session_id,
            history,
            registry,
            target.model,
            target.options,
            request,
            thinking,
        )));
        self.queued
            .push_back(ClientEvent::Session(SessionEvent::Status(if rollback {
                "rolling the harness back…".into()
            } else {
                "refining the harness…".into()
            })));
        Ok(())
    }

    fn auto_refine_due(&self) -> Option<mush_session::refine::AutoRefineReason> {
        use mush_session::refine::AutoRefineReason;
        let settings = self.config.auto_refine;
        if !settings.enabled
            || self.runtime.rlm_depth > 0
            || !self.runtime.harness_enabled()
            || self.turn_active()
            || self.refine.is_some()
            || self.auto_review.is_some()
        {
            return None;
        }
        let cooling = self
            .last_auto_review
            .is_some_and(|at| at.elapsed() < settings.cooldown());
        if cooling {
            return None;
        }
        if self.compact_refine_pending && settings.compact {
            return Some(AutoRefineReason::Compact);
        }
        (self.assistant_turns_since_review >= settings.turn_interval)
            .then_some(AutoRefineReason::TurnInterval)
    }

    async fn maybe_auto_refine(&mut self) {
        let Some(reason) = self.auto_refine_due() else {
            return;
        };
        self.compact_refine_pending = false;
        self.last_auto_review = Some(std::time::Instant::now());
        let turns = std::mem::take(&mut self.assistant_turns_since_review);
        let target = self.runtime.compaction_target(&self.config).await;
        let messages = self.runtime.conversation.context();
        let history = self.runtime.refinement_history();
        let merged = mush_session::harness::load_merged(&self.runtime.session_id);
        let registry = self.registry.clone();
        let thinking = self.runtime.thinking_level.clone();
        self.auto_review = Some((
            reason,
            tokio::spawn(async move {
                mush_session::refine::review(
                    &messages,
                    &merged,
                    &history,
                    &registry,
                    &target.model,
                    &target.options,
                    reason,
                    turns,
                    &thinking,
                )
                .await
            }),
        ));
    }

    fn try_poll_auto_review(&mut self) -> Option<ClientEvent> {
        let (reason, review) = take_finished_keyed(&mut self.auto_review)?;
        let review = match review {
            Ok(Ok(review)) => review,
            Ok(Err(error)) => {
                tracing::warn!(%error, "auto-refine review failed");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, "auto-refine review task failed");
                return None;
            }
        };
        if !review.should_refine {
            tracing::info!(rationale = %review.rationale, "auto-refine review declined");
            return None;
        }
        let request = review.refine_request(reason);
        self.host_commands
            .get_or_insert_with(Default::default)
            .replace_refine(request);
        Some(ClientEvent::Session(SessionEvent::Status(
            "auto-refine review approved, refining after this turn…".into(),
        )))
    }

    fn try_poll_refine(&mut self) -> Option<ClientEvent> {
        if self.turn_active() {
            return None;
        }
        let outcome = match take_finished(&mut self.refine)? {
            Ok(outcome) => outcome,
            Err(error) => Err(format!("refine task failed: {error}")),
        };
        let result = outcome.and_then(|plan| {
            crate::refine::apply_plan(&self.runtime.session_id, &plan)
                .map(|result| (plan.source, result))
        });
        Some(match result {
            Ok((source, result)) => {
                self.runtime
                    .conversation
                    .append_refinement(source, result.clone());
                self.runtime.save_now_and_settle();
                ClientEvent::Refined(Box::new(result))
            }
            Err(error) => ClientEvent::Session(SessionEvent::Failed(format!("refine: {error}"))),
        })
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
        let context = crate::side_question::side_question_context(
            self.runtime.system_prompt.as_deref(),
            self.runtime.conversation.context(),
            &question,
        );
        let registry = self.registry.clone();
        let model = self.runtime.model(&self.config.default_model);
        let options = self.config.options.clone();
        let keys = self.config.provider_api_keys.clone();
        let account = self.runtime.account_for(&model);
        let task = tokio::spawn(async move {
            let options = resolve_auth_into_options(&model, options, &keys, account).await;
            mush_ai::oneshot::collect_text(&registry, &model, &context, &options)
                .await
                .map_err(|error| error.to_string())
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
        Some(ClientEvent::Session(SessionEvent::Status(status)))
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
        events
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

    fn try_poll_compaction(&mut self) -> Option<ClientEvent> {
        let result = take_finished(&mut self.compaction)?.ok()?;
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
                if self
                    .runtime
                    .apply_compaction(
                        applied.summary.clone(),
                        applied.summarised_count,
                        &applied.messages,
                    )
                    .is_none()
                {
                    return ClientEvent::Session(SessionEvent::Failed(String::from(
                        "compaction discarded, the conversation changed while it ran",
                    )));
                }
                if let Some(commands) = &self.host_commands {
                    commands.record_context(ContextUsage {
                        tokens: None,
                        ..commands.context_usage()
                    });
                }
                self.runtime.save_now_and_settle();
                self.compact_refine_pending = true;
                self.kernel_compaction_notice_due = true;
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
        if self.turn_active()
            || self.compaction.is_some()
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
        self.abort().await;
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
        let target = self.runtime.compaction_target(&self.config).await.fitting(
            mush_session::compact::summary_request_tokens(
                &messages,
                self.config.compaction.keep_recent(&messages),
                &focus,
                &self.config.compaction,
            ),
        );
        let pane_model = self.runtime.model(&self.config.default_model);
        let live = target.uses_session_model().then(|| {
            let tools = crate::turn::aligned_tools(
                &self.tools,
                &pane_model,
                &self.config,
                self.injections.cwd.clone().unwrap_or_default(),
            );
            mush_session::compact::LiveContext {
                system_prompt: self.runtime.system_prompt.clone(),
                tools: mush_agent::tool_definitions(&tools, pane_model.api),
            }
        });
        let task = crate::compaction::start_compaction(
            messages,
            target.model,
            target.options,
            self.registry.clone(),
            Some(self.injections.lifecycle_hooks.clone()),
            self.injections.cwd.clone(),
            focus,
            live,
            self.config.compaction,
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
                                .context_window
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

#[async_trait::async_trait]
impl AgentConnection for InProcessConnection {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    async fn send(&mut self, command: ClientCommand) -> Result<Vec<ClientEvent>, ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        let before = self.queued.len();
        match command {
            ClientCommand::Prompt { text, images } => self.prompt(text, images).await,
            ClientCommand::QueuePrompt { text, images } => self.queue_prompt(&text, images),
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
                self.queued
                    .push_back(ClientEvent::Session(SessionEvent::Status(status)));
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
mod tests {
    use super::*;
    use mush_ai::types::{Api, AssistantContentPart, AssistantMessage, StopReason, TextContent};
    use mush_session::ConversationState;
    use std::sync::{Arc, Mutex};

    fn connection() -> (InProcessConnection, crate::testing::Requests) {
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

    fn connection_replying(reply: &str) -> (InProcessConnection, crate::testing::Requests) {
        let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(crate::testing::Echo {
            requests: requests.clone(),
            reply: reply.into(),
        }));
        let model = crate::testing::echo_model();
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.model_id = model.id.to_string();
        let connection = InProcessConnection::new(
            runtime,
            ToolRegistry::new(),
            registry,
            crate::testing::echo_config(model),
            mush_agent::AgentInjections::default(),
        );
        (connection, requests)
    }

    async fn wait_for(
        connection: &mut InProcessConnection,
        accept: impl Fn(&ClientEvent) -> bool,
    ) -> ClientEvent {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(event) = connection.next_event().await
                    && accept(&event)
                {
                    break event;
                }
            }
        })
        .await
        .unwrap()
    }

    async fn pump_until(
        connection: &mut InProcessConnection,
        accept: impl Fn(&ClientEvent) -> bool,
    ) -> ClientEvent {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                connection.start_pending().await;
                if let Some(event) = connection.try_next_event() {
                    if accept(&event) {
                        break event;
                    }
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn refine_applies_the_planned_edits_and_the_next_turn_carries_the_digest() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let (mut connection, requests) = connection_replying(
            "{\"summary\":\"remember the runner\",\"rationale\":\"seen twice\",\"expectedOutcome\":\"fewer misses\",\"edits\":[{\"action\":\"create\",\"kind\":\"memory\",\"title\":\"test runner\",\"content\":\"cargo nextest run\"}]}",
        );
        connection.runtime.saver = Some(Arc::new(|_| {}));
        connection.runtime.conversation = ConversationState::from_messages(vec![user("hi")]);

        let started = connection
            .send(ClientCommand::Refine(
                mush_session::refine::RefineRequest::default(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            started[..],
            [ClientEvent::Session(SessionEvent::Status(_))]
        ));
        let refined = wait_for(&mut connection, |event| {
            matches!(event, ClientEvent::Refined(_))
        })
        .await;
        let ClientEvent::Refined(result) = refined else {
            unreachable!()
        };
        assert_eq!(result.scope, mush_session::harness::Scope::Local);
        assert_eq!(result.applied_edits[0].id, "test_runner");
        assert!(result.applied_edits[0].applied);
        let local = mush_session::harness::load_merged(&connection.runtime.session_id);
        assert_eq!(
            local.entries[&mush_session::harness::Kind::Memory]["test_runner"].content,
            "cargo nextest run"
        );
        assert_eq!(connection.runtime.refinement_history().len(), 1);

        connection
            .send(ClientCommand::Prompt {
                text: "next".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        let turn = requests.lock().unwrap()[1].messages.clone();
        assert_eq!(turn.len(), 4);
        assert!(
            matches!(&turn[1], Message::User(u) if u.text() == "[user-refinement]\n\nthese harness edits are already applied, nothing to do\nsummary: remember the runner\n- created memory [local:test_runner] test runner: cargo nextest run"),
            "{turn:?}"
        );
        let Message::User(digest) = &turn[2] else {
            panic!("{turn:?}")
        };
        assert!(
            digest
                .text()
                .starts_with(mush_session::harness::DIGEST_PREFIX)
        );
        assert!(digest.text().contains("[local:test_runner] test runner"));
        assert!(matches!(&turn[3], Message::User(u) if u.text() == "next"));

        connection
            .send(ClientCommand::Prompt {
                text: "again".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        let turn = requests.lock().unwrap()[2].messages.clone();
        assert!(
            !turn
                .iter()
                .skip(3)
                .any(|message| matches!(message, Message::User(u) if u.text().starts_with(mush_session::harness::DIGEST_PREFIX))),
            "an unchanged harness is not re-delivered"
        );
    }

    fn is_digest(message: &Message) -> bool {
        matches!(message, Message::User(u) if u.text().starts_with(mush_session::harness::DIGEST_PREFIX))
    }

    async fn prompt(connection: &mut InProcessConnection, text: &str) {
        connection
            .send(ClientCommand::Prompt {
                text: text.into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(connection).await;
    }

    #[tokio::test]
    async fn a_mid_session_refine_reaches_the_model_as_a_notice_and_the_digest_waits_for_a_cold_boundary()
     {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let (mut connection, requests) = connection_replying(
            "{\"summary\":\"remember the runner\",\"edits\":[{\"action\":\"create\",\"kind\":\"memory\",\"title\":\"test runner\",\"content\":\"cargo nextest run\"}]}",
        );
        connection.runtime.saver = Some(Arc::new(|_| {}));

        prompt(&mut connection, "hello").await;
        connection
            .send(ClientCommand::Refine(
                mush_session::refine::RefineRequest::default(),
            ))
            .await
            .unwrap();
        wait_for(&mut connection, |event| {
            matches!(event, ClientEvent::Refined(_))
        })
        .await;
        prompt(&mut connection, "next").await;

        let turn = requests.lock().unwrap()[2].messages.clone();
        assert!(!turn.iter().any(is_digest), "{turn:?}");
        assert!(
            matches!(&turn[turn.len() - 2], Message::User(u) if u.text().starts_with("[user-refinement]\n\nthese harness edits are already applied")),
            "{turn:?}"
        );

        let leaf = connection.runtime.conversation.leaf_id().unwrap().clone();
        connection.runtime.navigate_tree(&leaf, None);
        prompt(&mut connection, "again").await;

        let turn = requests.lock().unwrap()[3].messages.clone();
        assert!(is_digest(&turn[turn.len() - 2]), "{turn:?}");
        assert_eq!(turn.iter().filter(|message| is_digest(message)).count(), 1);
    }

    #[tokio::test]
    async fn the_turn_after_a_compaction_carries_the_digest_again() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let (mut connection, requests) = connection();
        connection.runtime.saver = Some(Arc::new(|_| {}));
        let mut state = mush_session::harness::State::default();
        state
            .upsert(
                &mush_session::harness::Edit {
                    action: mush_session::harness::Action::Create,
                    kind: mush_session::harness::Kind::Memory,
                    id: None,
                    title: Some("test runner".into()),
                    content: Some("cargo nextest run".into()),
                    path: None,
                    reference: None,
                    arguments: None,
                    metadata: None,
                    reason: None,
                },
                mush_session::harness::Scope::Local,
                "test",
            )
            .unwrap();
        state
            .save(&mush_session::harness::local_dir(
                &connection.runtime.session_id,
            ))
            .unwrap();

        prompt(&mut connection, "one").await;
        prompt(&mut connection, "two").await;
        let context = connection.runtime.conversation.context();
        connection.runtime.apply_compaction(
            "earlier work".into(),
            3,
            &[vec![user("earlier work")], context[3..].to_vec()].concat(),
        );
        prompt(&mut connection, "three").await;

        let asked = requests.lock().unwrap();
        assert!(is_digest(&asked[0].messages[0]), "{:?}", asked[0].messages);
        assert!(!asked[1].messages.iter().skip(1).any(is_digest));
        let after = &asked[2].messages;
        assert!(is_digest(&after[after.len() - 2]), "{after:?}");
    }

    #[tokio::test]
    async fn a_refine_that_returns_no_json_fails_without_touching_the_harness() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let (mut connection, _) = connection_replying("i have nothing to add");
        connection.runtime.saver = Some(Arc::new(|_| {}));

        connection
            .send(ClientCommand::Refine(
                mush_session::refine::RefineRequest::default(),
            ))
            .await
            .unwrap();
        let failed = wait_for(&mut connection, |event| {
            matches!(event, ClientEvent::Session(SessionEvent::Failed(_)))
        })
        .await;
        let ClientEvent::Session(SessionEvent::Failed(text)) = failed else {
            unreachable!()
        };
        assert!(text.starts_with("refine:"), "{text}");
        assert!(mush_session::harness::load_merged(&connection.runtime.session_id).is_empty());
    }

    #[tokio::test]
    async fn auto_refine_reviews_after_the_interval_and_refines_when_approved() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let (mut connection, requests) =
            connection_replying("{\"shouldRefine\": true, \"rationale\": \"worth keeping\"}");
        connection.runtime.saver = Some(Arc::new(|_| {}));
        connection.config.auto_refine = mush_session::refine::AutoRefineSettings {
            enabled: true,
            turn_interval: 1,
            compact: true,
            cooldown_ms: 0,
        };

        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;

        let approved = pump_until(&mut connection, |event| {
            matches!(event, ClientEvent::Session(SessionEvent::Status(text)) if text.starts_with("auto-refine review approved"))
        })
        .await;
        assert!(matches!(approved, ClientEvent::Session(_)));
        let refined = pump_until(&mut connection, |event| {
            matches!(event, ClientEvent::Refined(_))
        })
        .await;
        let ClientEvent::Refined(result) = refined else {
            unreachable!()
        };
        assert!(result.applied_edits.is_empty());
        assert!(matches!(
            connection
                .runtime
                .conversation
                .entries()
                .last()
                .map(|entry| &entry.kind),
            Some(mush_session::tree::EntryKind::Refinement {
                source: mush_session::harness::RefinementSource::Auto,
                ..
            })
        ));
        assert!(!connection.runtime.conversation.context().iter().any(|message| {
            matches!(message, Message::User(u) if mush_session::harness::is_refinement_notice(&u.text()))
        }));
        assert_eq!(connection.assistant_turns_since_review, 0);
        let asked = requests.lock().unwrap();
        assert_eq!(asked.len(), 3, "turn, review, plan");
        let Message::User(review) = &asked[1].messages[0] else {
            unreachable!()
        };
        assert!(
            review
                .text()
                .contains("<trigger>\nturn_interval; 1 assistant turns"),
            "{}",
            review.text()
        );
    }

    #[tokio::test]
    async fn a_subagent_never_runs_the_auto_refine_review() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let (mut connection, requests) =
            connection_replying("{\"shouldRefine\": true, \"rationale\": \"worth keeping\"}");
        connection.runtime.saver = Some(Arc::new(|_| {}));
        connection.runtime.rlm_depth = 1;
        connection.config.auto_refine = mush_session::refine::AutoRefineSettings {
            enabled: true,
            turn_interval: 1,
            compact: true,
            cooldown_ms: 0,
        };

        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        connection.start_pending().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while connection.try_next_event().is_some() {}

        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(connection.auto_review.is_none());
    }

    #[tokio::test]
    async fn auto_refine_stays_quiet_when_disabled() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let (mut connection, requests) =
            connection_replying("{\"shouldRefine\": true, \"rationale\": \"worth keeping\"}");
        connection.runtime.saver = Some(Arc::new(|_| {}));
        connection.config.auto_refine = mush_session::refine::AutoRefineSettings {
            enabled: false,
            turn_interval: 1,
            compact: true,
            cooldown_ms: 0,
        };

        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        connection.start_pending().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        while connection.try_next_event().is_some() {}

        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(connection.auto_review.is_none());
    }

    #[tokio::test]
    async fn refine_is_refused_without_a_saved_session() {
        let (mut connection, _) = connection();
        let refused = connection
            .send(ClientCommand::Refine(
                mush_session::refine::RefineRequest::default(),
            ))
            .await;
        assert!(matches!(refused, Err(ConnectionError::Refused(_))));
    }

    #[tokio::test]
    async fn a_compaction_runs_in_the_background_applies_and_saves() {
        let (mut connection, _) = connection();
        let messages: Vec<Message> = (0..30)
            .map(|i| user(&format!("turn {i}: {}", "x".repeat(4_000))))
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

    struct LiveKernel(Option<Vec<String>>);

    #[async_trait::async_trait]
    impl mush_kernel::KernelState for LiveKernel {
        async fn snapshot(
            &self,
            _path: &std::path::Path,
        ) -> Result<Option<mush_kernel::SnapshotReport>, String> {
            Ok(None)
        }
        async fn restore_from(
            &self,
            _path: &std::path::Path,
        ) -> Option<Result<mush_kernel::RestoreReport, String>> {
            None
        }
        async fn live_names(&self) -> Option<Result<Vec<String>, String>> {
            self.0.clone().map(Ok)
        }
        async fn reset(&self) {}
        fn attach_session(&self, _session: mush_kernel::KernelSession) {}
    }

    async fn compacted_with(kernel: LiveKernel) -> Vec<Message> {
        let (mut connection, _) = connection();
        connection.runtime.conversation = ConversationState::from_messages(
            (0..30)
                .map(|i| user(&format!("turn {i}: {}", "x".repeat(400))))
                .collect(),
        );
        connection.runtime.set_kernel_state(Some(Arc::new(kernel)));
        connection
            .send(ClientCommand::Compact {
                focus: CompactionFocus::default(),
                auto: false,
            })
            .await
            .unwrap();
        wait_for(&mut connection, |event| {
            matches!(event, ClientEvent::Compacted(_))
        })
        .await;
        connection.start_pending().await;
        connection.start_pending().await;
        connection.transcript().await
    }

    #[tokio::test]
    async fn a_manual_compaction_tells_the_model_which_python_names_are_still_live() {
        let transcript = compacted_with(LiveKernel(Some(vec!["df".into(), "load".into()]))).await;

        let notices: Vec<String> = transcript
            .iter()
            .filter_map(|message| match message {
                Message::User(user) if crate::kernel_notice::is_notice(&user.text()) => {
                    Some(user.text())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            [
                "[python-state]\n\nYour Python kernel persisted through compaction; its remaining \
              variables, imports, and helpers are still available. These names are still \
              defined: df, load."
            ]
        );
        assert!(
            matches!(transcript.last(), Some(Message::User(user)) if user.text() == notices[0])
        );
    }

    #[tokio::test]
    async fn a_compaction_with_no_running_kernel_adds_no_python_notice() {
        let transcript = compacted_with(LiveKernel(None)).await;

        assert!(!transcript.iter().any(|message| matches!(
            message,
            Message::User(user) if crate::kernel_notice::is_notice(&user.text())
        )));
    }

    #[tokio::test]
    async fn summarise_and_navigate_puts_the_model_summary_on_the_new_branch() {
        let (mut connection, requests) = connection();
        connection.runtime.conversation = ConversationState::from_messages(vec![
            user("apples"),
            user("bananas"),
            user("cherries"),
        ]);
        let target = connection.runtime.conversation.tree().entries()[1]
            .id
            .clone();

        let started = connection
            .send(ClientCommand::SummariseAndNavigate { entry_id: target })
            .await
            .unwrap();
        assert!(matches!(
            started[..],
            [ClientEvent::Session(SessionEvent::Status(_))]
        ));

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(event @ ClientEvent::Session(SessionEvent::TranscriptReplaced { .. })) =
                    connection.next_event().await
                {
                    break event;
                }
            }
        })
        .await
        .unwrap();

        let ClientEvent::Session(SessionEvent::TranscriptReplaced {
            messages,
            editor_text,
            ..
        }) = event
        else {
            unreachable!()
        };
        assert_eq!(editor_text.as_deref(), Some("bananas"));
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::User(u) if u.text() == "apples"));
        assert!(matches!(&messages[1], Message::User(u) if u.text().contains("hi back")));
        let asked = requests.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let Message::User(request) = &asked[0].messages[0] else {
            unreachable!()
        };
        let summarised = request.text();
        assert!(summarised.contains("bananas"), "{summarised}");
        assert!(summarised.contains("cherries"), "{summarised}");
        assert!(!summarised.contains("apples"), "{summarised}");
    }

    #[tokio::test]
    async fn summarise_and_navigate_skips_the_model_when_nothing_is_left_behind() {
        let (mut connection, requests) = connection();
        let reply = Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: "reply".into(),
                })])
                .model("m".into())
                .provider(mush_ai::types::Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .stop_reason(StopReason::Stop)
                .timestamp_ms(Timestamp::zero())
                .build(),
        );
        connection.runtime.conversation = ConversationState::from_messages(vec![user("a"), reply]);
        let assistant = connection.runtime.conversation.tree().entries()[1]
            .id
            .clone();

        let events = connection
            .send(ClientCommand::SummariseAndNavigate {
                entry_id: assistant,
            })
            .await
            .unwrap();

        assert!(matches!(
            events[0],
            ClientEvent::Session(SessionEvent::TranscriptReplaced { .. })
        ));
        assert!(requests.lock().unwrap().is_empty());
    }

    struct NoteTool;

    #[async_trait::async_trait]
    impl mush_agent::AgentTool for NoteTool {
        fn name(&self) -> &str {
            "note"
        }
        fn label(&self) -> &str {
            "Note"
        }
        fn description(&self) -> &str {
            "keeps a note"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _args: serde_json::Value) -> mush_agent::ToolResult {
            mush_agent::ToolResult::text("noted")
        }
        fn output_limit(&self) -> mush_agent::OutputLimit {
            mush_agent::OutputLimit::Head
        }
    }

    #[tokio::test]
    async fn a_prompt_after_aborting_a_hung_tool_starts_a_new_turn() {
        let (mut connection, requests) = crate::testing::hanging_tool_connection();
        connection
            .send(ClientCommand::Prompt {
                text: "start".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        wait_for(&mut connection, |event| {
            matches!(
                event,
                ClientEvent::Agent {
                    event: AgentEvent::ToolExecStart { .. },
                    ..
                }
            )
        })
        .await;

        let aborted = connection.send(ClientCommand::Abort).await.unwrap();
        assert!(
            aborted
                .iter()
                .any(|event| matches!(event, ClientEvent::TurnEnded { cancelled: true })),
            "{aborted:?}"
        );
        assert!(!connection.state().turn_active, "{:?}", connection.state());

        let started = connection
            .send(ClientCommand::Prompt {
                text: "again".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        assert!(
            started
                .iter()
                .any(|event| matches!(event, ClientEvent::TurnStarted)),
            "{started:?}"
        );
        let events = drain_turn(&mut connection).await;
        assert!(events.iter().any(|event| matches!(
            event,
            ClientEvent::Agent {
                event: AgentEvent::MessageEnd { .. },
                ..
            }
        )));
        let asked = requests.lock().unwrap();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(matches!(asked[1].messages.last(), Some(Message::User(u)) if u.text() == "again"));
    }

    #[tokio::test]
    async fn aborting_with_queued_steers_sends_them_together_in_a_new_turn() {
        let (mut connection, requests) = crate::testing::hanging_tool_connection();
        connection
            .send(ClientCommand::Prompt {
                text: "start".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        wait_for(&mut connection, |event| {
            matches!(
                event,
                ClientEvent::Agent {
                    event: AgentEvent::ToolExecStart { .. },
                    ..
                }
            )
        })
        .await;
        for text in ["first", "second"] {
            connection
                .send(ClientCommand::Steer {
                    text: text.into(),
                    images: Vec::new(),
                })
                .await
                .unwrap();
        }

        let events = connection
            .send(ClientCommand::AbortAndSendQueued)
            .await
            .unwrap();
        let ended = events
            .iter()
            .position(|event| matches!(event, ClientEvent::TurnEnded { cancelled: true }));
        let started = events
            .iter()
            .position(|event| matches!(event, ClientEvent::TurnStarted));
        assert!(
            matches!((ended, started), (Some(ended), Some(started)) if ended < started),
            "{events:?}"
        );

        drain_turn(&mut connection).await;
        assert!(connection.runtime.steering_queue.lock().await.is_empty());
        let asked = requests.lock().unwrap();
        assert_eq!(asked.len(), 2, "{asked:?}");
        let texts: Vec<String> = asked[1]
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(user.text()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["start", "first", "second"]);
    }

    #[tokio::test]
    async fn aborting_with_no_steers_only_aborts() {
        let (mut connection, requests) = crate::testing::hanging_tool_connection();
        connection
            .send(ClientCommand::Prompt {
                text: "start".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        wait_for(&mut connection, |event| {
            matches!(
                event,
                ClientEvent::Agent {
                    event: AgentEvent::ToolExecStart { .. },
                    ..
                }
            )
        })
        .await;

        let events = connection
            .send(ClientCommand::AbortAndSendQueued)
            .await
            .unwrap();

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ClientEvent::TurnStarted)),
            "{events:?}"
        );
        assert!(!connection.state().turn_active, "{:?}", connection.state());
        assert!(
            !connection.state().pending_prompt,
            "{:?}",
            connection.state()
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_steer_with_no_turn_joins_the_pending_prompt() {
        let (mut connection, _) = connection();
        connection.runtime.pending_prompt = Some(PendingPrompt::text("first"));
        connection.compaction_auto = false;

        connection.steer("second".into(), Vec::new()).await;

        let pending = connection.runtime.pending_prompt.as_ref().unwrap();
        assert!(pending.text.contains("first"), "{}", pending.text);
        assert!(pending.text.contains("second"), "{}", pending.text);
    }

    #[tokio::test]
    async fn a_steer_sent_during_a_tool_call_reaches_the_next_request() {
        let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(crate::testing::ToolThenText {
            requests: requests.clone(),
            gate: gate.clone(),
        }));
        let model = crate::testing::echo_model();
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.model_id = model.id.to_string();
        let mut tools = ToolRegistry::new();
        tools.register_shared(Arc::new(NoteTool));
        let mut connection = InProcessConnection::new(
            runtime,
            tools,
            registry,
            crate::testing::echo_config(model),
            mush_agent::AgentInjections::default(),
        );

        connection
            .send(ClientCommand::Prompt {
                text: "start".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(connection.state().turn_active);
        connection
            .send(ClientCommand::Steer {
                text: "actually, stop and do y".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        gate.notify_one();

        let events = drain_turn(&mut connection).await;
        assert!(
            events.iter().any(|event| matches!(
                event,
                ClientEvent::Agent {
                    event: AgentEvent::SteeringInjected { .. },
                    ..
                }
            )),
            "{events:?}"
        );
        let asked = requests.lock().unwrap();
        assert_eq!(asked.len(), 2);
        let second = &asked[1].messages;
        assert!(
            matches!(second.last(), Some(Message::User(u)) if u.text() == "actually, stop and do y"),
            "{second:?}"
        );
        assert!(matches!(&second[second.len() - 2], Message::ToolResult(_)));
    }

    #[tokio::test]
    async fn a_compaction_request_shares_the_prefix_of_the_live_turn() {
        let (mut connection, requests) = connection();
        connection.runtime.system_prompt = Some("you are terse".into());
        let mut tools = ToolRegistry::new();
        tools.register_shared(Arc::new(NoteTool));
        let registry = std::mem::replace(&mut connection.registry, ApiRegistry::new());
        connection.set_deps(tools, registry);
        connection.runtime.conversation = ConversationState::from_messages(
            (0..30)
                .map(|i| user(&format!("turn {i}: {}", "x".repeat(4_000))))
                .collect(),
        );

        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        connection
            .send(ClientCommand::Compact {
                focus: CompactionFocus::default(),
                auto: false,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(ClientEvent::Compacted(_)) = connection.next_event().await {
                    break;
                }
            }
        })
        .await
        .unwrap();

        let asked = requests.lock().unwrap();
        let [turn, compaction] = &asked[..] else {
            panic!("{}", asked.len());
        };
        assert_eq!(compaction.system_prompt, turn.system_prompt);
        assert_eq!(
            compaction.tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
            turn.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert_eq!(compaction.tools[0].name, "note");
        assert_eq!(compaction.messages.len(), turn.messages.len() + 2);
        assert_eq!(
            &compaction.messages[..turn.messages.len()],
            &turn.messages[..]
        );
        assert!(matches!(
            compaction.messages.last(),
            Some(Message::User(user)) if user.text().starts_with("[context checkpoint]")
        ));
    }

    async fn compaction_request_with_a_summariser_window(
        window: u64,
    ) -> mush_ai::registry::LlmContext {
        let (mut connection, requests) = connection();
        connection.runtime.system_prompt = Some("you are terse".into());
        let mut summariser = crate::testing::echo_model();
        summariser.id = "summariser".into();
        summariser.context_window = TokenCount::new(window);
        summariser.effective_context_window = None;
        connection.config.compaction_model =
            Some((summariser, mush_ai::types::StreamOptions::default()));
        connection.runtime.conversation = ConversationState::from_messages(
            (0..30)
                .map(|i| user(&format!("turn {i}: {}", "x".repeat(4_000))))
                .collect(),
        );

        connection
            .send(ClientCommand::Compact {
                focus: CompactionFocus::default(),
                auto: false,
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(ClientEvent::Compacted(_)) = connection.next_event().await {
                    break;
                }
            }
        })
        .await
        .unwrap();

        let asked = requests.lock().unwrap();
        let [compaction] = &asked[..] else {
            panic!("{}", asked.len());
        };
        compaction.clone()
    }

    #[tokio::test]
    async fn a_summariser_with_room_for_the_summary_request_gets_it() {
        let compaction = compaction_request_with_a_summariser_window(1_000_000).await;

        assert_ne!(compaction.system_prompt.as_deref(), Some("you are terse"));
        assert_eq!(compaction.messages.len(), 1);
    }

    #[tokio::test]
    async fn a_summariser_too_small_for_the_summary_request_hands_it_to_the_session_model() {
        let compaction = compaction_request_with_a_summariser_window(10_000).await;

        assert_eq!(compaction.system_prompt.as_deref(), Some("you are terse"));
        assert!(matches!(
            compaction.messages.last(),
            Some(Message::User(user)) if user.text().starts_with("[context checkpoint]")
        ));
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
    async fn turns_and_idle_changes_record_the_thinking_level_in_the_tree() {
        use mush_ai::types::ThinkingLevel;
        use mush_session::tree::EntryKind;
        let (mut connection, _) = connection();
        connection
            .send(ClientCommand::SetThinking(ThinkingLevel::Medium))
            .await
            .unwrap();
        assert!(connection.runtime.conversation.is_empty());

        connection
            .send(ClientCommand::Prompt {
                text: "hi".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;

        let entries = connection.runtime.conversation.entries();
        assert!(matches!(
            &entries[0].kind,
            EntryKind::ThinkingLevelChange { thinking_level } if *thinking_level == ThinkingLevel::Medium
        ));
        assert!(matches!(
            &entries[1].kind,
            EntryKind::Message {
                message: Message::User(_)
            }
        ));

        connection
            .send(ClientCommand::SetThinking(ThinkingLevel::High))
            .await
            .unwrap();
        assert_eq!(
            connection.runtime.conversation.thinking_level(),
            Some(ThinkingLevel::High)
        );
    }

    #[tokio::test]
    async fn abort_mid_stream_keeps_the_partial_as_an_aborted_message() {
        let (mut connection, _) = connection();
        connection.runtime.conversation = ConversationState::from_messages(vec![user("go")]);
        let start = AssistantMessage::builder()
            .model("m".into())
            .provider(mush_ai::types::Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();
        for event in [
            AgentEvent::MessageStart { message: start },
            AgentEvent::StreamEvent {
                event: mush_ai::stream::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "partly".into(),
                },
            },
        ] {
            connection.runtime.apply_event(&event);
        }

        connection.send(ClientCommand::Abort).await.unwrap();

        let transcript = connection.transcript().await;
        assert!(matches!(
            transcript.last(),
            Some(Message::Assistant(message))
                if message.stop_reason == StopReason::Aborted && message.text() == "partly"
        ));
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
    async fn stopping_a_child_reaches_the_family_and_names_what_is_missing() {
        let (mut connection, _) = connection();
        assert!(matches!(
            connection
                .send(ClientCommand::StopChild {
                    session_id: "nobody".into(),
                })
                .await,
            Err(ConnectionError::Refused(_))
        ));

        connection.set_family(Some(crate::family::Family::new(
            crate::family::Identity::root("root"),
            "root".into(),
            2,
            None,
            None,
        )));
        let Err(ConnectionError::Refused(error)) = connection
            .send(ClientCommand::StopChild {
                session_id: "nobody".into(),
            })
            .await
        else {
            panic!("an unknown child is refused")
        };
        assert!(error.contains("nobody"), "{error}");
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

    fn bash_done(command: &str, exit_code: i32) -> mush_tools::kernel_jobs::JobNotice {
        mush_tools::kernel_jobs::JobNotice {
            done: mush_tools::kernel_jobs::BashDone {
                pid: Some(77),
                exit_code,
                command: command.into(),
            },
            origin: None,
        }
    }

    #[tokio::test]
    async fn a_kernel_notice_becomes_a_prompt_when_idle_and_steering_when_busy() {
        let (mut connection, _requests) = connection();
        let (sinks, signals) = crate::kernel_channels::kernel_channels();
        connection.attach_kernel(Some(signals));

        (sinks.notices)(bash_done("exit 5", 5));
        connection.tick().await;

        let events: Vec<_> = std::iter::from_fn(|| connection.try_next_event()).collect();
        assert!(
            matches!(&events[..], [ClientEvent::BashDone(notice), ClientEvent::TurnStarted] if notice.done.exit_code == 5),
            "{events:?}"
        );
        let transcript = connection.transcript().await;
        assert!(matches!(
            &transcript[0],
            Message::User(user) if user.text().starts_with("[bash-done pid:77 exit:5]")
        ));

        (sinks.notices)(bash_done("second", 0));
        connection.tick().await;

        let events: Vec<_> = std::iter::from_fn(|| connection.try_next_event()).collect();
        assert!(
            matches!(&events[..], [ClientEvent::BashDone(_)]),
            "{events:?}"
        );
        let queued = connection
            .runtime
            .turns
            .meta(PRIMARY_PANE)
            .expect("the turn is running")
            .steering_queue
            .lock()
            .await
            .len();
        assert_eq!(queued, 1);
    }

    #[tokio::test]
    async fn a_busy_kernel_question_reaches_the_client_and_the_answer_reaches_the_tool() {
        use mush_tools::ipython::BusyKernelChoice;
        let (mut connection, _requests) = connection();
        let (sinks, signals) = crate::kernel_channels::kernel_channels();
        connection.attach_kernel(Some(signals));

        let answer = (sinks.busy_prompt)();
        assert!(matches!(
            connection.try_next_event(),
            Some(ClientEvent::KernelBusy)
        ));
        assert!(connection.try_next_event().is_none());

        connection
            .send(ClientCommand::AnswerKernelBusy(BusyKernelChoice::Kill))
            .await
            .unwrap();

        assert_eq!(answer.await, BusyKernelChoice::Kill);
    }

    #[tokio::test]
    async fn an_aborted_cell_withdraws_its_busy_question() {
        let (mut connection, _requests) = connection();
        let (sinks, signals) = crate::kernel_channels::kernel_channels();
        connection.attach_kernel(Some(signals));

        let answer = (sinks.busy_prompt)();
        assert!(matches!(
            connection.try_next_event(),
            Some(ClientEvent::KernelBusy)
        ));
        drop(answer);

        assert!(matches!(
            connection.try_next_event(),
            Some(ClientEvent::KernelBusyWithdrawn)
        ));
        assert!(connection.try_next_event().is_none());
    }

    #[tokio::test]
    async fn tool_output_lines_arrive_as_events() {
        let (mut connection, _requests) = connection();
        let (sinks, signals) = crate::kernel_channels::kernel_channels();
        connection.attach_kernel(Some(signals));

        (sinks.output)("tc_1", "building");
        (sinks.output)("tc_2", "testing");

        let events: Vec<_> = std::iter::from_fn(|| connection.try_next_event()).collect();
        assert!(
            matches!(
                &events[..],
                [
                    ClientEvent::ToolOutput { exec_id: a, line: la },
                    ClientEvent::ToolOutput { exec_id: b, line: lb },
                ] if a == "tc_1" && la == "building" && b == "tc_2" && lb == "testing"
            ),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_side_question_is_answered_without_touching_the_conversation() {
        let (mut connection, requests) = connection();
        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        let before = connection.state().context_len;

        connection
            .send(ClientCommand::SideQuestion("why slow?".into()))
            .await
            .unwrap();
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match connection.next_event().await {
                    Some(ClientEvent::SideAnswer { question, answer }) => break (question, answer),
                    Some(_) => {}
                    None => panic!("closed"),
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(answer.0, "why slow?");
        assert_eq!(answer.1, Ok("hi back".into()));
        assert_eq!(connection.state().context_len, before);
        let asked = requests.lock().unwrap();
        let last = &asked.last().unwrap().messages;
        assert!(matches!(last.last(), Some(Message::User(user)) if user.text() == "why slow?"));
        assert_eq!(last.len(), before + 1);
    }

    #[tokio::test]
    async fn reload_rebuilds_the_system_prompt_through_the_worker_builder() {
        let (mut connection, _requests) = connection();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        connection.set_system_prompt_builder(Some(Arc::new({
            let calls = Arc::clone(&calls);
            move || {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                format!("prompt v{n}")
            }
        })));

        let events = connection
            .send(ClientCommand::ReloadSystemPrompt)
            .await
            .unwrap();

        assert_eq!(
            connection.runtime.system_prompt.as_deref(),
            Some("prompt v1")
        );
        assert!(matches!(
            &events[..],
            [ClientEvent::Session(SessionEvent::SystemPromptChanged(Some(prompt)))]
                if prompt == "prompt v1"
        ));
    }

    #[tokio::test]
    async fn a_goal_keeps_prompting_until_the_kernel_completes_it() {
        let (mut connection, requests) = connection();
        let goal = Arc::new(crate::goal::GoalStore::new(None));
        connection.set_goal_store(Some(Arc::clone(&goal)));

        let events = connection
            .send(ClientCommand::Goal(crate::goal::GoalCommand::Create {
                objective: "answer twice".into(),
                token_budget: None,
            }))
            .await
            .unwrap();
        assert!(matches!(
            &events[..],
            [ClientEvent::GoalChanged(Some(goal))] if goal.objective == "answer twice"
        ));

        connection.start_pending().await;
        assert!(connection.state().turn_active);
        drain_turn(&mut connection).await;
        assert!(matches!(
            &requests.lock().unwrap()[0].messages[0],
            Message::User(user) if user.text().starts_with("[goal: continuation 1]")
        ));

        connection.start_pending().await;
        assert!(
            connection.state().turn_active,
            "the goal continues after a turn"
        );
        drain_turn(&mut connection).await;
        assert_eq!(goal.get().unwrap().continuations, 2);

        goal.apply(crate::goal::GoalCommand::Complete {
            summary: Some("both answered".into()),
        })
        .unwrap();
        connection.start_pending().await;
        assert!(!connection.state().turn_active);
        assert!(matches!(
            connection.try_next_event(),
            Some(ClientEvent::GoalChanged(Some(goal))) if goal.status == crate::goal::GoalStatus::Complete
        ));
    }

    struct BusyKernel(std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl mush_kernel::KernelState for BusyKernel {
        async fn snapshot(
            &self,
            _path: &std::path::Path,
        ) -> Result<Option<mush_kernel::SnapshotReport>, String> {
            Ok(None)
        }
        async fn restore_from(
            &self,
            _path: &std::path::Path,
        ) -> Option<Result<mush_kernel::RestoreReport, String>> {
            None
        }
        async fn reset(&self) {}
        fn attach_session(&self, _session: mush_kernel::KernelSession) {}
        fn background_work(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    struct RestoringKernel {
        outcome: Option<Result<mush_kernel::RestoreReport, String>>,
        gate: tokio::sync::Notify,
        restores: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl mush_kernel::KernelState for RestoringKernel {
        async fn snapshot(
            &self,
            _path: &std::path::Path,
        ) -> Result<Option<mush_kernel::SnapshotReport>, String> {
            Ok(None)
        }
        async fn restore_from(
            &self,
            _path: &std::path::Path,
        ) -> Option<Result<mush_kernel::RestoreReport, String>> {
            self.restores
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.gate.notified().await;
            self.outcome.clone()
        }
        async fn reset(&self) {}
        fn attach_session(&self, _session: mush_kernel::KernelSession) {}
    }

    fn restoring_kernel(
        outcome: Result<mush_kernel::RestoreReport, String>,
    ) -> Arc<RestoringKernel> {
        Arc::new(RestoringKernel {
            outcome: Some(outcome),
            gate: tokio::sync::Notify::new(),
            restores: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn resumed_worker(
        kernel: &Arc<RestoringKernel>,
        snapshot: bool,
    ) -> (InProcessConnection, crate::testing::Requests) {
        let requests: crate::testing::Requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(crate::testing::Echo {
            requests: requests.clone(),
            reply: "ok".into(),
        }));
        let mut worker =
            crate::WorkerConfig::new(crate::testing::echo_config(crate::testing::echo_model()));
        if snapshot {
            let path = mush_session::SessionStore::kernel_state_path(&worker.session_id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"snapshot").unwrap();
        }
        worker.kernel_state = Some(Arc::clone(kernel) as Arc<dyn mush_kernel::KernelState>);
        (worker.connect(&ToolRegistry::new(), &registry), requests)
    }

    fn user_texts(context: &mush_ai::registry::LlmContext) -> Vec<String> {
        context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(user.text()),
                _ => None,
            })
            .collect()
    }

    async fn prompt_after_restore(
        connection: &mut InProcessConnection,
        kernel: &RestoringKernel,
        text: &str,
    ) -> String {
        connection
            .send(ClientCommand::Prompt {
                text: text.into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let waited = !connection.state().turn_active;
        kernel.gate.notify_one();
        let ClientEvent::Session(SessionEvent::Status(status)) = wait_for(connection, |event| {
            matches!(event, ClientEvent::Session(SessionEvent::Status(_)))
        })
        .await
        else {
            unreachable!()
        };
        connection.start_pending().await;
        drain_turn(connection).await;
        assert!(waited, "the turn waits for the restore");
        status
    }

    #[tokio::test]
    async fn a_resumed_kernel_names_what_came_back_once_before_the_first_request() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let kernel = restoring_kernel(Ok(mush_kernel::RestoreReport {
            restored: vec!["df".into(), "helper".into()],
            failed: vec!["sock".into()],
        }));
        let (mut connection, requests) = resumed_worker(&kernel, true);

        let status = prompt_after_restore(&mut connection, &kernel, "carry on").await;
        connection
            .send(ClientCommand::Prompt {
                text: "and again".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;

        let requests = requests.lock().unwrap().clone();
        let first = user_texts(&requests[0]);
        assert_eq!(
            first,
            [
                "[python-state-restored]\n\nYour Python kernel state was revived from your \
                 previous session. These names are available again: df, helper.\nThese could \
                 not be restored and must be recreated if needed: sock.",
                "carry on"
            ]
        );
        let notices = |texts: &[String]| {
            texts
                .iter()
                .filter(|text| crate::kernel_notice::is_notice(text))
                .count()
        };
        assert_eq!(notices(&user_texts(&requests[1])), 1);
        assert_eq!(status, "python state restored");
        assert_eq!(kernel.restores.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_resume_restore_tells_the_model_the_kernel_starts_fresh() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let kernel = restoring_kernel(Err("kernel exited".into()));
        let (mut connection, requests) = resumed_worker(&kernel, true);

        let status = prompt_after_restore(&mut connection, &kernel, "carry on").await;

        let first = user_texts(&requests.lock().unwrap()[0]);
        assert_eq!(
            first,
            [
                "[python-state-restored]\n\nYour previous Python kernel state could not be \
                 revived; the kernel is starting fresh, so re-create any variables, imports, or \
                 loaded data you need.",
                "carry on"
            ]
        );
        assert_eq!(status, "python state could not be restored");
    }

    #[tokio::test]
    async fn a_resume_without_a_snapshot_starts_at_once_with_no_notice() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let kernel = restoring_kernel(Ok(mush_kernel::RestoreReport::default()));
        let (mut connection, requests) = resumed_worker(&kernel, false);

        connection
            .send(ClientCommand::Prompt {
                text: "fresh".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        assert!(connection.state().turn_active);
        drain_turn(&mut connection).await;

        assert_eq!(user_texts(&requests.lock().unwrap()[0]), ["fresh"]);
        assert_eq!(kernel.restores.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_errored_turn_fails_the_goal_and_stops_autonomous_mode() {
        let (mut connection, _requests) = connection();
        connection.set_deps(ToolRegistry::new(), ApiRegistry::new());
        let goal = Arc::new(crate::goal::GoalStore::new(None));
        connection.set_goal_store(Some(Arc::clone(&goal)));
        let autonomous = Arc::new(crate::autonomous::Autonomous::new(std::env::temp_dir()));
        connection.set_autonomous(Some(Arc::clone(&autonomous)));
        connection
            .send(ClientCommand::Autonomous(
                crate::autonomous::AutonomousCommand::On {
                    limits: crate::autonomous::Limits::unlimited(),
                    gates: crate::autonomous::Gates::default(),
                    subagent_keep_alive_ms: 0,
                },
            ))
            .await
            .unwrap();
        connection
            .send(ClientCommand::Goal(crate::goal::GoalCommand::Create {
                objective: "never reachable".into(),
                token_budget: None,
            }))
            .await
            .unwrap();

        connection.start_pending().await;
        assert!(connection.state().turn_active);
        drain_turn(&mut connection).await;

        let failed = goal.get().unwrap();
        assert_eq!(failed.status, crate::goal::GoalStatus::Failed);
        assert!(failed.error.is_some());
        assert!(!autonomous.due());
        connection.start_pending().await;
        assert!(!connection.state().turn_active);
    }

    #[tokio::test]
    async fn setting_the_max_depth_reaches_the_family_and_the_system_prompt() {
        let (mut connection, _requests) = connection();
        let family = crate::family::Family::new(
            crate::family::Identity::root("root"),
            "root".into(),
            2,
            None,
            None,
        );
        connection.set_family(Some(Arc::clone(&family)));
        connection.set_system_prompt_builder(Some(Arc::new({
            let family = Arc::clone(&family);
            move || format!("max depth {}", family.max_depth())
        })));

        connection
            .send(ClientCommand::SetRlmMaxDepth(3))
            .await
            .unwrap();
        assert_eq!(family.max_depth(), 3);
        assert!(connection.runtime.system_prompt.is_none());

        let events = connection
            .send(ClientCommand::SetRlmMaxDepth(0))
            .await
            .unwrap();
        assert_eq!(family.max_depth(), 0);
        assert_eq!(
            connection.runtime.system_prompt.as_deref(),
            Some("max depth 0")
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ClientEvent::Session(SessionEvent::SystemPromptChanged(Some(_)))
        )));
        assert_eq!(
            connection.state().rlm_max_depth_source,
            crate::session::RlmMaxDepthSource::Chat
        );

        connection
            .send(ClientCommand::Session(SessionCommand::New))
            .await
            .unwrap();
        assert_eq!(family.max_depth(), 2);
        assert_eq!(connection.state().rlm_max_depth, 2);
        assert_eq!(
            connection.state().rlm_max_depth_source,
            crate::session::RlmMaxDepthSource::Default
        );
        assert_eq!(
            connection.runtime.system_prompt.as_deref(),
            Some("max depth 2")
        );
    }

    struct ModelKernel(std::sync::Mutex<Vec<(String, bool)>>);

    #[async_trait::async_trait]
    impl mush_kernel::KernelState for ModelKernel {
        async fn snapshot(
            &self,
            _path: &std::path::Path,
        ) -> Result<Option<mush_kernel::SnapshotReport>, String> {
            Ok(None)
        }
        async fn restore_from(
            &self,
            _path: &std::path::Path,
        ) -> Option<Result<mush_kernel::RestoreReport, String>> {
            None
        }
        async fn reset(&self) {}
        fn attach_session(&self, _session: mush_kernel::KernelSession) {}
        fn set_model(&self, id: &str, accepts_images: bool) {
            self.0
                .lock()
                .unwrap()
                .push((id.to_string(), accepts_images));
        }
    }

    #[tokio::test]
    async fn a_turn_tells_the_kernel_which_model_it_runs_on() {
        let (mut connection, _requests) = connection();
        let kernel = Arc::new(ModelKernel(std::sync::Mutex::new(Vec::new())));
        connection.runtime.set_kernel_state(Some(
            Arc::clone(&kernel) as Arc<dyn mush_kernel::KernelState>
        ));
        let model = connection.runtime.model(&connection.config.default_model);

        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;

        assert_eq!(
            *kernel.0.lock().unwrap(),
            [(
                model.id.to_string(),
                model.input.contains(&mush_ai::types::InputModality::Image)
            )]
        );
    }

    #[tokio::test]
    async fn a_goal_holds_while_background_bash_runs() {
        let (mut connection, _requests) = connection();
        let kernel = Arc::new(BusyKernel(std::sync::atomic::AtomicBool::new(true)));
        connection.runtime.set_kernel_state(Some(
            Arc::clone(&kernel) as Arc<dyn mush_kernel::KernelState>
        ));
        let goal = Arc::new(crate::goal::GoalStore::new(None));
        connection.set_goal_store(Some(Arc::clone(&goal)));
        connection
            .send(ClientCommand::Goal(crate::goal::GoalCommand::Create {
                objective: "wait for the build".into(),
                token_budget: None,
            }))
            .await
            .unwrap();

        connection.start_pending().await;
        assert!(!connection.state().turn_active);
        assert_eq!(goal.get().unwrap().continuations, 0);

        kernel.0.store(false, std::sync::atomic::Ordering::SeqCst);
        connection.start_pending().await;
        assert!(connection.state().turn_active);
        assert_eq!(goal.get().unwrap().continuations, 1);
    }

    #[tokio::test]
    async fn a_due_heartbeat_becomes_a_prompt_when_idle_and_a_steer_when_busy() {
        let (mut connection, requests) = connection();
        let scheduler = Arc::new(crate::schedule::Scheduler::new(None));
        connection.set_scheduler(Some(Arc::clone(&scheduler)));
        let events = connection
            .send(ClientCommand::Schedule(
                crate::schedule::ScheduleCommand::Heartbeat {
                    expression: "every 10s".into(),
                    prompt: "check the build".into(),
                    delivery: crate::schedule::Delivery::Steer,
                },
            ))
            .await
            .unwrap();
        assert!(matches!(&events[..], [ClientEvent::ScheduleChanged(jobs)] if jobs.len() == 1));

        connection.start_pending().await;
        assert!(!connection.state().turn_active, "nothing is due yet");

        let far = mush_ai::types::Timestamp::now().as_ms() + 11_000;
        let due = scheduler.take_due(far);
        assert_eq!(due.len(), 1);
        connection.queue_prompt(&due[0].prompt_text(), Vec::new());
        connection.start_pending().await;
        assert!(connection.state().turn_active);
        drain_turn(&mut connection).await;
        assert!(matches!(
            &requests.lock().unwrap()[0].messages[0],
            Message::User(user) if user.text() == "[heartbeat: every 10s run#1]\n\ncheck the build"
        ));

        connection
            .send(ClientCommand::Schedule(
                crate::schedule::ScheduleCommand::HeartbeatClear,
            ))
            .await
            .unwrap();
        assert!(scheduler.heartbeat().is_none());
        assert!(matches!(
            connection
                .send(ClientCommand::Schedule(
                    crate::schedule::ScheduleCommand::HeartbeatPause
                ))
                .await,
            Err(ConnectionError::Refused(_))
        ));
    }

    #[tokio::test]
    async fn a_new_session_rebinds_the_goal_and_schedule_files() {
        let dir = tempfile::tempdir().unwrap();
        let (mut connection, _requests) = connection();
        let goal = Arc::new(crate::goal::GoalStore::new(Some(
            dir.path().join("a/goal.json"),
        )));
        goal.apply(crate::goal::GoalCommand::Create {
            objective: "old".into(),
            token_budget: None,
        })
        .unwrap();
        connection.set_goal_store(Some(Arc::clone(&goal)));
        connection.runtime.saver = Some(Arc::new(|_| {}));

        connection
            .send(ClientCommand::Session(SessionCommand::New))
            .await
            .unwrap();

        assert!(
            goal.get().is_none(),
            "the new session starts without a goal"
        );
        assert!(dir.path().join("a/goal.json").exists());
    }

    struct ToolCaller;

    #[async_trait::async_trait]
    impl mush_ai::registry::ApiProvider for ToolCaller {
        fn api(&self) -> Api {
            Api::AnthropicMessages
        }

        async fn stream(
            &self,
            model: &mush_ai::types::Model,
            context: &mush_ai::registry::LlmContext,
            _options: &mush_ai::types::StreamOptions,
        ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
            let already_called = context
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResult(_)));
            let content = if already_called {
                vec![mush_ai::types::AssistantContentPart::Text(
                    mush_ai::types::TextContent {
                        text: "done".into(),
                    },
                )]
            } else {
                vec![mush_ai::types::AssistantContentPart::ToolCall(
                    mush_ai::types::ToolCall {
                        id: mush_ai::types::ToolCallId::from("call_block"),
                        name: mush_ai::types::ToolName::from("block"),
                        arguments: serde_json::json!({}),
                    },
                )]
            };
            let message = AssistantMessage::builder()
                .content(content)
                .model(model.id.clone())
                .provider(model.provider.clone())
                .api(model.api)
                .stop_reason(if already_called {
                    StopReason::Stop
                } else {
                    StopReason::ToolUse
                })
                .timestamp_ms(Timestamp::zero())
                .build();
            Ok(Box::pin(futures::stream::iter([
                mush_ai::stream::StreamEvent::Done {
                    reason: message.stop_reason,
                    message,
                },
            ])))
        }
    }

    struct Blocker;

    #[async_trait::async_trait]
    impl mush_agent::tool::AgentTool for Blocker {
        fn name(&self) -> &str {
            "block"
        }
        fn label(&self) -> &str {
            "block"
        }
        fn description(&self) -> &str {
            "never returns"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _args: serde_json::Value) -> mush_agent::tool::ToolResult {
            std::future::pending().await
        }
        fn output_limit(&self) -> mush_agent::tool::OutputLimit {
            mush_agent::tool::OutputLimit::Head
        }
    }

    #[tokio::test]
    async fn abort_returns_while_a_tool_is_still_running() {
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(ToolCaller));
        let model = crate::testing::echo_model();
        let mut runtime = SessionRuntime::new(ConversationState::new());
        runtime.model_id = model.id.to_string();
        let mut connection = InProcessConnection::new(
            runtime,
            ToolRegistry::from_shared(vec![Arc::new(Blocker) as mush_agent::tool::SharedTool]),
            registry,
            crate::testing::echo_config(model),
            mush_agent::AgentInjections::default(),
        );
        connection
            .send(ClientCommand::Prompt {
                text: "block".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match connection.next_event().await {
                    Some(ClientEvent::Agent {
                        event: AgentEvent::ToolExecStart { .. },
                        ..
                    }) => break,
                    Some(_) => {}
                    None => panic!("closed"),
                }
            }
        })
        .await
        .expect("the tool starts");

        let aborted = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connection.send(ClientCommand::Abort),
        )
        .await
        .expect("abort returns while the tool blocks")
        .unwrap();
        assert!(matches!(
            &aborted[..],
            [ClientEvent::ToolsCancelled(ids), ClientEvent::TurnEnded { cancelled: true }] if ids.len() == 1
        ));
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while connection.state().turn_active {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    connection.next_event(),
                )
                .await;
            }
        })
        .await;
        assert!(settled.is_ok(), "the aborted turn winds down");
    }

    #[tokio::test]
    async fn the_notes_ride_in_the_conversation_and_not_the_system_prompt() {
        let (mut connection, requests) = connection();
        connection.runtime.system_prompt = Some("base".into());
        let notes = crate::notes::Notes::new(None);
        connection.set_notes(Some(Arc::clone(&notes)));
        notes.append("remember the socket path").unwrap();
        assert_eq!(connection.system_prompt().await.as_deref(), Some("base"));

        connection
            .send(ClientCommand::Prompt {
                text: "first".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        let turn = requests.lock().unwrap()[0].messages.clone();
        let Message::User(carried) = &turn[0] else {
            panic!("{turn:?}")
        };
        let (fingerprint, body) = crate::notes::extract(&carried.text()).unwrap();
        assert_eq!(fingerprint, crate::notes::fingerprint(&notes.read()));
        assert!(body.ends_with("- remember the socket path"));
        assert!(matches!(&turn[1], Message::User(u) if u.text() == "first"));

        connection
            .send(ClientCommand::Prompt {
                text: "second".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        let turn = requests.lock().unwrap()[1].messages.clone();
        assert_eq!(
            turn.iter()
                .filter(|message| matches!(message, Message::User(u) if crate::notes::extract(&u.text()).is_some()))
                .count(),
            1,
            "unchanged notes are not re-delivered"
        );

        notes.append("and the pid file").unwrap();
        connection
            .send(ClientCommand::Prompt {
                text: "third".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;
        let turn = requests.lock().unwrap()[2].messages.clone();
        assert_eq!(
            turn.iter()
                .filter(|message| matches!(message, Message::User(u) if crate::notes::extract(&u.text()).is_some()))
                .count(),
            2,
            "a changed note lands at the tail"
        );
        let Message::User(latest) = &turn[turn.len() - 2] else {
            panic!("{turn:?}")
        };
        assert!(crate::notes::extract(&latest.text()).is_some());
    }

    #[tokio::test]
    async fn autonomous_mode_continues_until_its_limit_and_reports_the_stop() {
        let (mut connection, requests) = connection();
        let autonomous = Arc::new(crate::autonomous::Autonomous::new(std::env::temp_dir()));
        connection.set_autonomous(Some(Arc::clone(&autonomous)));
        connection
            .send(ClientCommand::Autonomous(
                crate::autonomous::AutonomousCommand::On {
                    limits: crate::autonomous::Limits {
                        max_continuations: Some(1),
                        ..crate::autonomous::Limits::unlimited()
                    },
                    gates: crate::autonomous::Gates::default(),
                    subagent_keep_alive_ms: 0,
                },
            ))
            .await
            .unwrap();
        connection
            .send(ClientCommand::Prompt {
                text: "start".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;

        connection.start_pending().await;
        assert!(
            connection.state().turn_active,
            "one continuation follows the first turn"
        );
        drain_turn(&mut connection).await;
        assert!(matches!(
            &requests.lock().unwrap()[1].messages.last(),
            Some(Message::User(user)) if user.text().starts_with("[autonomous-continuation 1]")
        ));

        connection.start_pending().await;
        assert!(!connection.state().turn_active, "the limit stops the run");
        let stopped =
            std::iter::from_fn(|| connection.try_next_event()).find_map(|event| match event {
                ClientEvent::AutonomousChanged(status) if status.stopped.is_some() => Some(status),
                _ => None,
            });
        assert_eq!(
            stopped.unwrap().stopped.as_deref(),
            Some("1 continuations used")
        );
    }

    #[tokio::test]
    async fn a_failing_gate_becomes_the_next_prompt_and_a_passing_one_ends_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let (mut connection, requests) = connection();
        let autonomous = Arc::new(crate::autonomous::Autonomous::new(dir.path().to_path_buf()));
        connection.set_autonomous(Some(Arc::clone(&autonomous)));
        connection
            .send(ClientCommand::Autonomous(
                crate::autonomous::AutonomousCommand::On {
                    limits: crate::autonomous::Limits::unlimited(),
                    gates: crate::autonomous::Gates {
                        commands: vec!["test -f done".into()],
                        max_retries: 3,
                        timeout_ms: 5_000,
                    },
                    subagent_keep_alive_ms: 0,
                },
            ))
            .await
            .unwrap();
        connection
            .send(ClientCommand::Prompt {
                text: "start".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_turn(&mut connection).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !connection.state().turn_active {
            assert!(
                std::time::Instant::now() < deadline,
                "the gate failure prompts a continuation"
            );
            connection.start_pending().await;
            let _ = connection.try_next_event();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        drain_turn(&mut connection).await;
        assert!(matches!(
            &requests.lock().unwrap()[1].messages.last(),
            Some(Message::User(user)) if user.text().starts_with("[autonomous-continuation: gate failed]")
                && user.text().contains("`test -f done` exited with 1")
        ));

        std::fs::write(dir.path().join("done"), "").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            connection.start_pending().await;
            let stopped =
                std::iter::from_fn(|| connection.try_next_event()).find_map(|event| match event {
                    ClientEvent::AutonomousChanged(status) if status.stopped.is_some() => {
                        Some(status)
                    }
                    _ => None,
                });
            if let Some(status) = stopped {
                assert_eq!(status.stopped.as_deref(), Some("all gates passed"));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the passing gate ends the run"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!connection.state().turn_active);
    }

    #[tokio::test]
    async fn a_host_command_runs_once_the_turn_is_over() {
        let (mut connection, _requests) = connection();
        let commands = Arc::new(HostCommands::default());
        connection.set_host_commands(Some(Arc::clone(&commands)));
        connection
            .send(ClientCommand::Prompt {
                text: "hello".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        commands.push(ClientCommand::Compact {
            focus: CompactionFocus::default(),
            auto: false,
        });
        connection.start_pending().await;
        assert_eq!(commands.pending(), 1, "held while the turn runs");

        drain_turn(&mut connection).await;
        connection.start_pending().await;

        assert_eq!(commands.pending(), 0);
        let status =
            std::iter::from_fn(|| connection.try_next_event()).find_map(|event| match event {
                ClientEvent::Session(SessionEvent::Status(text)) => Some(text),
                _ => None,
            });
        assert_eq!(status.as_deref(), Some("conversation too short to compact"));
    }

    #[tokio::test]
    async fn a_refused_compaction_from_the_agent_lands_in_the_conversation() {
        let (mut connection, _requests) = connection();
        let commands = Arc::new(HostCommands::default());
        connection.set_host_commands(Some(Arc::clone(&commands)));
        connection.runtime.conversation =
            ConversationState::from_messages(vec![user("one"), user("two")]);

        commands.push(ClientCommand::Compact {
            focus: mush_session::compact::CompactionFocus::default(),
            auto: false,
        });
        connection.run_host_commands().await;

        let last = connection.runtime.conversation.context().pop().unwrap();
        let Message::User(message) = last else {
            panic!("expected the outcome as a user message")
        };
        let text = message.text();
        assert!(text.starts_with(mush_session::compact::COMPACTION_OUTCOME_PREFIX));
        assert!(text.contains("skipped"), "{text}");
        assert!(text.contains("too short"), "{text}");
    }

    #[tokio::test]
    async fn a_second_compaction_request_in_one_turn_is_refused() {
        let commands = HostCommands::default();
        assert!(!commands.compaction_queued());
        commands.push(ClientCommand::Compact {
            focus: mush_session::compact::CompactionFocus::default(),
            auto: false,
        });
        assert!(commands.compaction_queued());
    }

    fn a_pause() -> mush_session::Pause {
        mush_session::Pause {
            at: mush_ai::types::Timestamp::now(),
            reason: Some("rebuilding".into()),
            by: Some("cli".into()),
        }
    }

    #[tokio::test]
    async fn a_pause_carried_by_the_worker_config_reaches_the_runtime() {
        let mut worker =
            crate::WorkerConfig::new(crate::testing::echo_config(crate::testing::echo_model()));
        worker.paused = Some(a_pause());
        let connection = worker.connect(
            &mush_agent::ToolRegistry::new(),
            &mush_ai::registry::ApiRegistry::new(),
        );
        assert!(connection.state().paused.is_some());
    }

    #[tokio::test]
    async fn a_resumed_worker_restores_the_thinking_level_its_branch_ran_with() {
        use mush_ai::types::ThinkingLevel;
        let model = crate::testing::echo_model();
        let connect = |conversation: ConversationState| {
            let mut worker = crate::WorkerConfig::new(crate::testing::echo_config(model.clone()));
            worker.conversation = conversation;
            worker.thinking_level = ThinkingLevel::High;
            worker.connect(
                &mush_agent::ToolRegistry::new(),
                &mush_ai::registry::ApiRegistry::new(),
            )
        };

        let mut recorded = ConversationState::from_messages(vec![user("go")]);
        recorded.record_thinking_level(&ThinkingLevel::Low);
        assert_eq!(
            connect(recorded).state().thinking_level,
            model
                .clamp_thinking(&ThinkingLevel::Low)
                .normalize_visible()
        );
        assert_eq!(
            connect(ConversationState::from_messages(vec![user("go")]))
                .state()
                .thinking_level,
            ThinkingLevel::High
        );
    }

    #[tokio::test]
    async fn a_paused_agent_holds_a_queued_prompt_until_it_resumes() {
        let (mut connection, _requests) = connection();
        connection
            .send(ClientCommand::SetPaused(Some(a_pause())))
            .await
            .unwrap();

        connection
            .send(ClientCommand::QueuePrompt {
                text: "work".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        assert!(!connection.start_pending().await);
        assert!(!connection.turn_active());
        assert!(connection.runtime.pending_prompt.is_some());

        connection
            .send(ClientCommand::SetPaused(None))
            .await
            .unwrap();
        assert!(connection.start_pending().await);
        assert!(connection.turn_active());
    }

    #[tokio::test]
    async fn a_pause_reaches_the_client_and_a_prompt_lifts_it() {
        let (mut connection, _requests) = connection();
        let events = connection
            .send(ClientCommand::SetPaused(Some(a_pause())))
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::PausedChanged(Some(_)))),
            "{events:?}"
        );
        assert!(connection.state().paused.is_some());

        let events = connection
            .send(ClientCommand::Prompt {
                text: "go".into(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ClientEvent::PausedChanged(None))),
            "{events:?}"
        );
        assert!(connection.state().paused.is_none());
        assert!(connection.turn_active());
    }

    #[tokio::test]
    async fn the_session_account_is_checked_against_the_logins_and_saved() {
        let _dirs = crate::testing::IsolatedDirs::acquire().await;
        let work = mush_ai::types::AccountLabel::parse("work").unwrap();
        let (mut connection, _requests) = connection();

        let refused = connection
            .send(ClientCommand::SetAccount(Some(work.clone())))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("run /login"), "{refused}");

        let creds = || mush_ai::oauth::OAuthCredentials {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: u64::MAX,
            account_id: None,
        };
        mush_ai::oauth::save_login("anthropic", None, creds()).unwrap();
        mush_ai::oauth::save_login("anthropic", Some(work.clone()), creds()).unwrap();

        let unknown = connection
            .send(ClientCommand::SetAccount(Some(
                mush_ai::types::AccountLabel::parse("nope").unwrap(),
            )))
            .await
            .unwrap_err();
        assert!(unknown.to_string().contains("default, work"), "{unknown}");

        connection
            .send(ClientCommand::SetAccount(Some(work.clone())))
            .await
            .unwrap();
        let state = connection.state();
        assert_eq!(state.accounts.get("anthropic"), Some(&work));
        let model = connection.runtime.model(&connection.config.default_model);
        assert_eq!(connection.runtime.account_for(&model), Some(work.clone()));
        assert_eq!(
            connection.runtime.models.accounts().get("anthropic"),
            Some(&work),
            "children inherit the switch"
        );
        assert_eq!(
            connection.runtime.snapshot().accounts.get("anthropic"),
            Some(&work)
        );
    }

    #[tokio::test]
    async fn the_subagent_model_is_session_state_and_reaches_the_client() {
        let (mut connection, _requests) = connection();
        assert!(connection.state().subagent_model.is_none());
        assert!(connection.runtime.models.for_child(None).is_none());

        connection
            .send(ClientCommand::SetSubagentModel(Some("cheap".into())))
            .await
            .unwrap();

        assert_eq!(connection.state().subagent_model.as_deref(), Some("cheap"));
        assert_eq!(
            connection.runtime.models.for_child(None).as_deref(),
            Some("cheap")
        );
        assert_eq!(
            connection
                .runtime
                .models
                .for_child(Some("named"))
                .as_deref(),
            Some("named"),
            "a spawn that names a model still wins"
        );

        connection
            .send(ClientCommand::SetSubagentModel(None))
            .await
            .unwrap();
        assert!(connection.state().subagent_model.is_none());
        assert!(connection.runtime.models.for_child(None).is_none());
    }

    #[tokio::test]
    async fn changing_the_session_model_moves_what_a_spawn_inherits() {
        let (mut connection, _requests) = connection();
        connection.runtime.models.set_current("at-start".into());

        connection
            .send(ClientCommand::SetModel("now".into()))
            .await
            .unwrap();

        assert_eq!(connection.runtime.models.current(), "now");
    }
}
