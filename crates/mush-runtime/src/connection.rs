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
    ConfirmTool {
        allow: bool,
    },
    SetModel(String),
    SetThinking(ThinkingLevel),
    SetRlmMaxDepth(u32),
    SetSystemPrompt(Option<String>),
    ReloadSystemPrompt,
    SetStreamOptions(mush_ai::types::StreamOptions),
    SetCompactionModel(Option<Box<(mush_ai::types::Model, mush_ai::types::StreamOptions)>>),
    Session(SessionCommand),
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
    ChildUpdated(Box<crate::family::ChildSummary>),
    GoalChanged(Option<Box<crate::goal::Goal>>),
    ScheduleChanged(Vec<crate::schedule::Job>),
    ProgressNote(String),
    AutonomousChanged(Box<crate::autonomous::AutonomousStatus>),
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
    #[serde(default)]
    pub last_request: Option<mush_ai::request_snapshot::RequestDigest>,
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
    async fn tree(&mut self) -> mush_session::tree::SessionTree;
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
    failed_compaction: Option<FailedCompaction>,
    title: Option<tokio::task::JoinHandle<Option<String>>>,
    inbox: Option<crate::inbox::OrchestratorInbox>,
    kernel: Option<crate::kernel_channels::KernelSignals>,
    busy_reply: Option<tokio::sync::oneshot::Sender<mush_tools::ipython::BusyKernelChoice>>,
    side_questions: Vec<(String, tokio::task::JoinHandle<Result<String, String>>)>,
    system_prompt_builder: Option<SystemPromptBuilder>,
    agent_inbox: Option<crate::family::AgentInbox>,
    deferred_deliveries: VecDeque<crate::family::Delivery>,
    family: Option<std::sync::Arc<crate::family::Family>>,
    known_children: std::collections::HashMap<crate::family::ChildId, crate::family::ChildSummary>,
    goal: Option<std::sync::Arc<crate::goal::GoalStore>>,
    known_goal: Option<crate::goal::Goal>,
    scheduler: Option<std::sync::Arc<crate::schedule::Scheduler>>,
    known_jobs: Vec<crate::schedule::Job>,
    deferred_jobs: VecDeque<String>,
    notes: Option<std::sync::Arc<crate::notes::Notes>>,
    autonomous: Option<std::sync::Arc<crate::autonomous::Autonomous>>,
    gate_task: Option<tokio::task::JoinHandle<Vec<crate::autonomous::GateRun>>>,
    host_commands: Option<std::sync::Arc<HostCommands>>,
    closed: bool,
}

#[derive(Default)]
pub struct HostCommands {
    queue: std::sync::Mutex<VecDeque<ClientCommand>>,
}

impl HostCommands {
    pub fn push(&self, command: ClientCommand) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(command);
    }

    fn drain(&self) -> Vec<ClientCommand> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

pub type SystemPromptBuilder = std::sync::Arc<dyn Fn() -> String + Send + Sync>;

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
            failed_compaction: None,
            title: None,
            inbox: None,
            kernel: None,
            busy_reply: None,
            side_questions: Vec::new(),
            system_prompt_builder: None,
            agent_inbox: None,
            deferred_deliveries: VecDeque::new(),
            family: None,
            known_children: std::collections::HashMap::new(),
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

    pub fn set_host_commands(&mut self, commands: Option<std::sync::Arc<HostCommands>>) {
        self.host_commands = commands;
    }

    async fn run_host_commands(&mut self) {
        let Some(commands) = self.host_commands.clone() else {
            return;
        };
        if self.runtime.turn_active() || self.compaction.is_some() {
            return;
        }
        for command in commands.drain() {
            match self.send(command).await {
                Ok(events) => self.queued.extend(events),
                Err(error) => {
                    self.queued
                        .push_back(ClientEvent::Session(SessionEvent::Status(
                            error.to_string(),
                        )));
                }
            }
        }
    }

    pub fn set_autonomous(
        &mut self,
        autonomous: Option<std::sync::Arc<crate::autonomous::Autonomous>>,
    ) {
        self.autonomous = autonomous;
    }

    async fn continue_autonomous(&mut self) {
        let Some(autonomous) = self.autonomous.clone() else {
            return;
        };
        if !autonomous.due()
            || self.gate_task.is_some()
            || self.runtime.turn_active()
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
        let commands = autonomous.gate_commands();
        if commands.is_empty() {
            self.apply_autonomous_decision(autonomous.decide(Vec::new()));
            return;
        }
        let timeout = autonomous.gate_timeout();
        self.gate_task = Some(tokio::spawn(async move {
            let mut runs = Vec::new();
            for command in commands {
                let snapshot = crate::autonomous::workspace_snapshot(autonomous.cwd());
                if autonomous.skip_gate(&command, snapshot.as_deref()) {
                    runs.push(crate::autonomous::GateRun {
                        command,
                        passed: false,
                        skipped: true,
                        exit_text: String::new(),
                        output: String::new(),
                        snapshot,
                    });
                    break;
                }
                let run = crate::autonomous::run_gate(&command, autonomous.cwd(), timeout).await;
                let passed = run.passed;
                runs.push(run);
                if !passed {
                    break;
                }
            }
            runs
        }));
    }

    fn try_poll_autonomous(&mut self) -> Option<ClientEvent> {
        use futures::FutureExt;
        let finished = self
            .gate_task
            .as_ref()
            .is_some_and(|task| task.is_finished());
        if !finished {
            return None;
        }
        let task = self.gate_task.take()?;
        let runs = task.now_or_never()?.unwrap_or_default();
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

    pub fn set_notes(&mut self, notes: Option<std::sync::Arc<crate::notes::Notes>>) {
        if let Some(notes) = &notes {
            self.injections.dynamic_system_context = Some(notes.dynamic_context());
        }
        self.notes = notes;
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

    pub fn set_scheduler(&mut self, scheduler: Option<std::sync::Arc<crate::schedule::Scheduler>>) {
        self.known_jobs = scheduler
            .as_ref()
            .map(|scheduler| scheduler.jobs())
            .unwrap_or_default();
        self.scheduler = scheduler;
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

    pub fn set_goal_store(&mut self, goal: Option<std::sync::Arc<crate::goal::GoalStore>>) {
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
        self.family
            .as_ref()
            .is_some_and(|family| family.all().iter().any(|child| !child.status.settled()))
    }

    async fn continue_goal(&mut self) {
        let Some(goal) = self.goal.clone() else {
            return;
        };
        if self.runtime.turn_active()
            || self.compaction.is_some()
            || self.runtime.pending_prompt.is_some()
            || self.children_running()
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

    pub fn set_family(&mut self, family: Option<std::sync::Arc<crate::family::Family>>) {
        self.family = family;
    }

    #[must_use]
    pub fn family(&self) -> Option<&std::sync::Arc<crate::family::Family>> {
        self.family.as_ref()
    }

    pub fn attach_agent_inbox(&mut self, inbox: crate::family::AgentInbox) {
        self.agent_inbox = Some(inbox);
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
            use crate::family::{DeliveryMode, Receipt};
            if held.len() >= crate::family::MAX_PENDING_MESSAGES {
                delivery.ack(Receipt::Rejected);
                continue;
            }
            match (busy, delivery.mode) {
                (true, DeliveryMode::Auto | DeliveryMode::Steer) => {
                    self.steer(delivery.event.text(), Vec::new()).await;
                    delivery.ack(Receipt::Delivered);
                }
                (true, DeliveryMode::FollowUp) => {
                    delivery.ack(Receipt::Queued);
                    held.push_back(delivery);
                }
                (false, _) => {
                    if self.runtime.pending_prompt.is_some() {
                        delivery.ack(Receipt::Queued);
                        held.push_back(delivery);
                        continue;
                    }
                    self.runtime.accept_event(delivery.event.clone());
                    self.queued
                        .push_back(ClientEvent::MessageInjected(delivery.event.clone()));
                    delivery.ack(Receipt::Delivered);
                }
            }
        }
        self.deferred_deliveries = held;
    }

    pub fn set_system_prompt_builder(&mut self, builder: Option<SystemPromptBuilder>) {
        self.system_prompt_builder = builder;
    }

    pub fn attach_kernel(&mut self, signals: Option<crate::kernel_channels::KernelSignals>) {
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
                provider_api_keys: std::collections::HashMap::new(),
                confirm_tools: false,
                auto_compact: false,
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

    pub fn attach_inbox(&mut self, inbox: crate::inbox::OrchestratorInbox) {
        self.inbox = Some(inbox);
    }

    pub fn attach_inbox_from_env(&mut self) {
        let cwd = self
            .injections
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        match crate::inbox::OrchestratorInbox::from_env(&self.runtime, &cwd) {
            Ok(Some(inbox)) => self.inbox = Some(inbox),
            Ok(None) => {}
            Err(error) => self.report_inbox_disabled(&error),
        }
    }

    #[must_use]
    pub fn inbox(&self) -> Option<&crate::inbox::OrchestratorInbox> {
        self.inbox.as_ref()
    }

    pub fn inbox_mut(&mut self) -> Option<&mut crate::inbox::OrchestratorInbox> {
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
        if !self.poll_inbox().await {
            self.runtime.save_if_dirty();
            return false;
        }
        self.poll_control().await;
        self.poll_agent_inbox().await;
        self.submit_kernel_notices().await;
        self.poll_schedule().await;
        self.run_host_commands().await;
        self.continue_goal().await;
        self.continue_autonomous().await;
        if std::mem::take(&mut self.fork_compaction_due) {
            let _ = self
                .start_compaction(CompactionFocus::default(), true)
                .await;
        }
        let started = self.start_pending_prompt().await;
        self.runtime.save_if_dirty();
        started
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
        if let Some(event) = self.queued.pop_front() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_confirm() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_title() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_kernel() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_side_questions() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_family() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_goal() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_schedule() {
            return Some(event);
        }
        if let Some(event) = self.try_poll_autonomous() {
            return Some(event);
        }
        self.try_poll_compaction()
    }

    fn try_poll_side_questions(&mut self) -> Option<ClientEvent> {
        use futures::FutureExt;
        let index = self
            .side_questions
            .iter()
            .position(|(_, task)| task.is_finished())?;
        let (question, task) = self.side_questions.remove(index);
        let answer = match task.now_or_never()? {
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
        let task = tokio::spawn(async move {
            let options = resolve_auth_into_options(&model, options, &keys).await;
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
            if self.runtime.turn_active() {
                self.steer(text, Vec::new()).await;
            } else {
                self.queue_prompt(&text, Vec::new());
            }
        }
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
        let was_active = self.turn_active();
        let cancelled = self.runtime.record_cancelled_tools();
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

    fn queue_prompt(&mut self, text: &str, images: Vec<PendingImage>) {
        match &mut self.runtime.pending_prompt {
            Some(pending) => pending.merge(text, images),
            None => self.runtime.pending_prompt = Some(PendingPrompt::with_images(text, images)),
        }
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
        let pane_model = self.runtime.model(&self.config.default_model);
        let live = (target.model.id == pane_model.id).then(|| {
            let tools = crate::turn::aligned_tools(
                &self.tools,
                &pane_model,
                &self.config,
                self.injections.cwd.clone().unwrap_or_default(),
            );
            mush_session::compact::LiveContext {
                system_prompt: mush_agent::effective_system_prompt(
                    self.runtime.system_prompt.as_deref(),
                    self.injections.dynamic_system_context.as_ref(),
                ),
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
                if let Some(goal) = &self.goal {
                    goal.record_usage(message.usage.total_tokens().get());
                }
                if let Some(autonomous) = &self.autonomous {
                    autonomous.record_usage(message.usage.total_tokens().get());
                    autonomous.record_assistant_turn();
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
                self.fork_compaction_due = !cancelled && self.fork_compaction_due();
                if !cancelled && let Some(goal) = &self.goal {
                    goal.turn_ended();
                }
                if !cancelled && let Some(autonomous) = &self.autonomous {
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
            ClientCommand::Prompt { text, images } => {
                if self.turn_active() {
                    self.steer(text, images).await;
                } else {
                    self.queue_prompt(&text, images);
                    self.start_pending_prompt().await;
                }
            }
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
            ClientCommand::SetModel(id) => self.runtime.model_id = id,
            ClientCommand::SetThinking(level) => self.runtime.thinking_level = level,
            ClientCommand::SetRlmMaxDepth(depth) => self.runtime.set_rlm_max_depth(depth),
            ClientCommand::SetSystemPrompt(prompt) => self.runtime.system_prompt = prompt,
            ClientCommand::ReloadSystemPrompt => {
                if let Some(build) = &self.system_prompt_builder {
                    let prompt = build();
                    self.runtime.system_prompt = Some(prompt.clone());
                    self.queued
                        .push_back(ClientEvent::Session(SessionEvent::SystemPromptChanged(
                            Some(prompt),
                        )));
                }
            }
            ClientCommand::SetStreamOptions(options) => self.config.options = options,
            ClientCommand::SetCompactionModel(model) => {
                self.config.compaction_model = model.map(|boxed| *boxed);
            }
            ClientCommand::Session(command) => {
                let before = self.runtime.session_id.clone();
                let events = self.runtime.handle(command).await;
                self.sync_control_target();
                if self.runtime.session_id != before {
                    self.rebind_artifacts();
                }
                self.runtime.save_now_and_settle();
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
                if let Some(family) = self.family.clone() {
                    family.shutdown().await;
                }
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
            last_request: mush_ai::request_snapshot::last_request(self.runtime.session_id.as_str()),
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

    async fn system_prompt(&mut self) -> Option<String> {
        let base = self.runtime.system_prompt.clone()?;
        let extra = self
            .injections
            .dynamic_system_context
            .as_ref()
            .and_then(|context| context());
        Some(match extra {
            Some(extra) => format!("{base}\n\n{extra}"),
            None => base,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Api, AssistantMessage, StopReason};
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
    async fn a_compaction_request_shares_the_prefix_of_the_live_turn() {
        let (mut connection, requests) = connection();
        connection.runtime.system_prompt = Some("you are terse".into());
        let mut tools = ToolRegistry::new();
        tools.register_shared(Arc::new(NoteTool));
        let registry = std::mem::replace(&mut connection.registry, ApiRegistry::new());
        connection.set_deps(tools, registry);
        connection.runtime.conversation =
            ConversationState::from_messages((0..30).map(|i| user(&format!("turn {i}"))).collect());

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
            Message::User(user) if user.text() == "[heartbeat: heartbeat]\n\ncheck the build"
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
    async fn the_system_prompt_read_includes_the_notes() {
        let (mut connection, _requests) = connection();
        connection.runtime.system_prompt = Some("base".into());
        let notes = crate::notes::Notes::new(None);
        connection.set_notes(Some(Arc::clone(&notes)));
        assert_eq!(connection.system_prompt().await.as_deref(), Some("base"));

        notes.append("remember the socket path").unwrap();

        let prompt = connection.system_prompt().await.unwrap();
        assert!(prompt.starts_with("base\n\n# Notes"));
        assert!(prompt.ends_with("- remember the socket path"));
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
}
