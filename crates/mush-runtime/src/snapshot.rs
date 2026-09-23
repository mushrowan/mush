use mush_ai::types::{PaneId, SessionId};
use mush_session::ConversationState;

pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub title: Option<String>,
    pub primary: ConversationState,
    pub model_id: String,
    pub panes: Vec<PaneSnapshot>,
    pub system_prompt: Option<String>,
    pub parent: Option<SessionId>,
    pub subagent_model: Option<String>,
    pub paused: Option<mush_session::Pause>,
    pub rlm_max_depth: Option<u32>,
}

pub struct PaneSnapshot {
    pub pane_id: PaneId,
    pub label: Option<String>,
    pub model_id: String,
    pub conversation: ConversationState,
}

pub type SessionSaver = std::sync::Arc<dyn Fn(SessionSnapshot) + Send + Sync>;
