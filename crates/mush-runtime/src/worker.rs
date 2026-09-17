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
    pub kernel_state: Option<Arc<dyn mush_kernel::KernelState>>,
    pub saver: Option<SessionSaver>,
    pub turn: TurnConfig,
    pub injections: mush_agent::AgentInjections,
    pub agent_card: Option<mush_agent::AgentCard>,
    pub kernel_signals: Option<crate::kernel_channels::KernelSignals>,
    pub system_prompt_builder: Option<crate::connection::SystemPromptBuilder>,
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
            kernel_state: None,
            saver: None,
            turn,
            injections: mush_agent::AgentInjections::default(),
            agent_card: None,
            kernel_signals: None,
            system_prompt_builder: None,
        }
    }

    #[must_use]
    pub fn connect(self, tools: &ToolRegistry, registry: &ApiRegistry) -> InProcessConnection {
        let mut runtime = SessionRuntime::new(self.conversation);
        runtime.saver = self.saver;
        runtime.session_id = self.session_id;
        runtime.title = self.title;
        runtime.model_id = self.turn.default_model.id.to_string();
        runtime.system_prompt = self.system_prompt;
        runtime.rlm_depth = self.rlm_depth;
        runtime.rlm_max_depth = self.rlm_max_depth;
        runtime.thinking_level = self.thinking_level;
        runtime.set_kernel_state(self.kernel_state);
        let mut connection = InProcessConnection::new(
            runtime,
            tools.clone(),
            registry.clone(),
            self.turn,
            self.injections,
        );
        connection.attach_kernel(self.kernel_signals);
        connection.set_system_prompt_builder(self.system_prompt_builder);
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
