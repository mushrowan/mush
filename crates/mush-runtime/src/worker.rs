use std::io;
use std::sync::Arc;

use mush_agent::tool::ToolRegistry;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{SessionId, ThinkingLevel};
use mush_session::ConversationState;

use crate::connection::InProcessConnection;
use crate::session::{DEFAULT_RLM_MAX_DEPTH, SessionRuntime};
use crate::snapshot::SessionSaver;
use crate::turn::TurnConfig;

pub struct WorkerConfig {
    pub conversation: ConversationState,
    pub session_id: SessionId,
    pub title: Option<String>,
    pub system_prompt: Option<String>,
    pub thinking_level: ThinkingLevel,
    pub rlm_depth: u32,
    pub rlm_max_depth: u32,
    pub rlm_max_depth_source: crate::session::RlmMaxDepthSource,
    pub rlm_max_depth_chat: Option<u32>,
    pub kernel_state: Option<Arc<dyn mush_kernel::KernelState>>,
    pub saver: Option<SessionSaver>,
    pub lease: Option<mush_session::SessionLease>,
    pub turn: TurnConfig,
    pub injections: mush_agent::AgentInjections,
    pub agent_card: Option<mush_agent::AgentCard>,
    pub kernel_signals: Option<crate::kernel_channels::KernelSignals>,
    pub system_prompt_builder: Option<crate::connection::SystemPromptBuilder>,
    pub agent_inbox: Option<crate::family::AgentInbox>,
    pub observe_inbox: Option<crate::family::ObserveInbox>,
    pub family: Option<Arc<crate::family::Family>>,
    pub family_member: Option<(Arc<crate::family::Family>, crate::family::ChildId)>,
    pub goal: Option<Arc<crate::goal::GoalStore>>,
    pub scheduler: Option<Arc<crate::schedule::Scheduler>>,
    pub notes: Option<Arc<crate::notes::Notes>>,
    pub parent: Option<SessionId>,
    pub autonomous: Option<Arc<crate::autonomous::Autonomous>>,
    pub host_commands: Option<Arc<crate::connection::HostCommands>>,
    pub recovery_notice: Option<String>,
    pub models: crate::models::ModelSelection,
    pub subagent_model: Option<String>,
    pub paused: Option<mush_session::Pause>,
}

impl WorkerConfig {
    #[must_use]
    pub fn new(turn: TurnConfig) -> Self {
        Self {
            conversation: ConversationState::default(),
            session_id: SessionId::new(),
            title: None,
            system_prompt: None,
            thinking_level: ThinkingLevel::Off,
            rlm_depth: 0,
            rlm_max_depth: DEFAULT_RLM_MAX_DEPTH,
            rlm_max_depth_source: crate::session::RlmMaxDepthSource::Default,
            rlm_max_depth_chat: None,
            kernel_state: None,
            saver: None,
            lease: None,
            turn,
            injections: mush_agent::AgentInjections::default(),
            agent_card: None,
            kernel_signals: None,
            system_prompt_builder: None,
            agent_inbox: None,
            observe_inbox: None,
            family: None,
            family_member: None,
            goal: None,
            scheduler: None,
            notes: None,
            parent: None,
            autonomous: None,
            host_commands: None,
            recovery_notice: None,
            models: crate::models::ModelSelection::default(),
            subagent_model: None,
            paused: None,
        }
    }

    #[must_use]
    pub fn connect(self, tools: &ToolRegistry, registry: &ApiRegistry) -> InProcessConnection {
        let thinking_level =
            self.conversation
                .thinking_level()
                .map_or(self.thinking_level, |level| {
                    self.turn
                        .default_model
                        .clamp_thinking(&level)
                        .normalize_visible()
                });
        let mut runtime = SessionRuntime::new(self.conversation);
        runtime.saver = self.saver;
        runtime.lease = self.lease;
        runtime.session_id = self.session_id;
        runtime.title = self.title;
        runtime.model_id = self.turn.default_model.id.to_string();
        self.models.set_current(runtime.model_id.clone());
        self.models.set_thinking(thinking_level.clone());
        self.models.set_subagent(self.subagent_model.clone());
        runtime.subagent_model = self.subagent_model;
        runtime.paused = self.paused;
        runtime.models = self.models;
        runtime.system_prompt = self.system_prompt;
        runtime.rlm_depth = self.rlm_depth;
        runtime.rlm_max_depth_base = (self.rlm_max_depth, self.rlm_max_depth_source);
        runtime.restore_rlm_max_depth(self.rlm_max_depth_chat);
        runtime.thinking_level = thinking_level;
        runtime.parent = self.parent;
        runtime.set_kernel_state(self.kernel_state);
        runtime.begin_kernel_restore();
        runtime.save_now();
        if let Some(notice) = self.recovery_notice {
            runtime
                .conversation
                .append_message(mush_ai::types::Message::Event(
                    mush_ai::types::EventMessage {
                        content: notice,
                        timestamp_ms: mush_ai::types::Timestamp::now(),
                        kind: mush_ai::types::EventKind::Recovery,
                        receipts: Vec::new(),
                    },
                ));
            runtime.save_now();
        }
        let mut connection = InProcessConnection::new(
            runtime,
            tools.clone(),
            registry.clone(),
            self.turn,
            self.injections,
        );
        connection.attach_kernel(self.kernel_signals);
        connection.set_system_prompt_builder(self.system_prompt_builder);
        if let Some(inbox) = self.observe_inbox {
            connection.attach_observe_inbox(inbox);
        }
        if let Some(inbox) = self.agent_inbox {
            connection.attach_agent_inbox(inbox);
        }
        connection.set_family(self.family);
        connection.set_family_member(self.family_member);
        connection.set_goal_store(self.goal);
        connection.set_scheduler(self.scheduler);
        connection.set_notes(self.notes);
        connection.set_autonomous(self.autonomous);
        connection.set_host_commands(self.host_commands);
        connection
    }
}

pub struct HostedWorker {
    pub connection: InProcessConnection,
    pub ipc_listener: Option<mush_agent::IpcListener>,
}

pub fn host_worker(
    config: WorkerConfig,
    tools: &ToolRegistry,
    registry: &ApiRegistry,
) -> io::Result<HostedWorker> {
    let card = config.agent_card.clone().map(Arc::new);
    let session_id = config.session_id.to_string();
    let mut connection = config.connect(tools, registry);
    let ipc_listener = match card {
        Some(card) => {
            let control = mush_agent::ipc::control::Control::new(session_id);
            let sock = mush_agent::ipc::control_socket_path(&control.target().instance);
            let listener =
                mush_agent::IpcListener::start_control(sock.clone(), card, control.clone())?;
            connection.attach_control(control, sock);
            Some(listener)
        }
        None => None,
    };
    connection.attach_inbox_from_env();
    Ok(HostedWorker {
        connection,
        ipc_listener,
    })
}
