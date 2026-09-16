use std::path::PathBuf;
use std::sync::Arc;

use mush_ai::types::{ImageMimeType, Message};
use mush_session::ConversationState;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct PendingImage {
    pub data: Vec<u8>,
    pub mime_type: ImageMimeType,
    pub dimensions: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct PendingPrompt {
    pub text: String,
    pub images: Vec<PendingImage>,
    persisted: bool,
}

impl PendingPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            persisted: false,
        }
    }

    pub fn with_images(text: impl Into<String>, images: Vec<PendingImage>) -> Self {
        Self {
            text: text.into(),
            images,
            persisted: false,
        }
    }

    pub fn is_persisted(&self) -> bool {
        self.persisted
    }

    pub fn mark_persisted(&mut self) {
        self.persisted = true;
    }
}

pub struct SessionRuntime {
    pub conversation: ConversationState,
    pub pending_prompt: Option<PendingPrompt>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    pub pending_compaction_inputs: Vec<(String, Vec<PendingImage>)>,
    pub title_generated: bool,
    pub control: Option<mush_agent::ipc::control::Control>,
    pub control_socket: Option<PathBuf>,
}

impl SessionRuntime {
    #[must_use]
    pub fn new(conversation: ConversationState) -> Self {
        Self {
            conversation,
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            pending_compaction_inputs: Vec::new(),
            title_generated: false,
            control: None,
            control_socket: None,
        }
    }

    #[must_use]
    pub fn control_endpoint(&self) -> Option<mush_agent::exec_context::ControlEndpoint> {
        Some(mush_agent::exec_context::ControlEndpoint {
            socket: self.control_socket.clone()?,
            target: self.control.as_ref()?.target(),
        })
    }

    pub fn drain_compaction_inputs(&mut self) -> Vec<(String, Vec<PendingImage>)> {
        std::mem::take(&mut self.pending_compaction_inputs)
    }
}

impl Default for SessionRuntime {
    fn default() -> Self {
        Self::new(ConversationState::new())
    }
}
