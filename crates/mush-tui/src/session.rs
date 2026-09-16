use std::collections::HashMap;
use std::sync::Arc;

use mush_ai::types::Message;
use mush_session::ConversationState;
use tokio::sync::Mutex;

use crate::app::App;
use crate::input_buffer::PendingImage;

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

pub struct Session {
    pub control: Option<mush_agent::ipc::control::Control>,
    pub control_socket: Option<std::path::PathBuf>,
    pub app: App,
    pub conversation: ConversationState,
    pub image_protos: HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    pub pending_prompt: Option<PendingPrompt>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    pub pending_compaction_inputs: Vec<(String, Vec<PendingImage>)>,
    pub label: Option<String>,
    pub title_generated: bool,
    pub pending_side_questions: Vec<crate::slash::side_question::PendingSideQuestion>,
}

impl Session {
    pub fn new(app: App) -> Self {
        Self {
            control: None,
            control_socket: None,
            app,
            conversation: ConversationState::new(),
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            pending_compaction_inputs: Vec::new(),
            label: None,
            title_generated: false,
            pending_side_questions: Vec::new(),
        }
    }

    pub fn with_conversation(app: App, conversation: ConversationState) -> Self {
        Self {
            control: None,
            control_socket: None,
            app,
            conversation,
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            pending_compaction_inputs: Vec::new(),
            label: None,
            title_generated: false,
            pending_side_questions: Vec::new(),
        }
    }

    pub fn queue_input_during_compaction(&mut self, text: String, images: Vec<PendingImage>) {
        let image_bytes: Vec<Vec<u8>> = images.iter().map(|image| image.data.clone()).collect();
        if image_bytes.is_empty() {
            self.app.push_queued_message(text.clone());
        } else {
            self.app
                .push_queued_message_with_images(text.clone(), image_bytes);
        }
        self.pending_compaction_inputs.push((text, images));
        self.app.input.text.clear();
        self.app.input.cursor = 0;
    }

    pub fn control_endpoint(&self) -> Option<mush_agent::exec_context::ControlEndpoint> {
        Some(mush_agent::exec_context::ControlEndpoint {
            socket: self.control_socket.clone()?,
            target: self.control.as_ref()?.target(),
        })
    }

    pub fn drain_compaction_inputs(&mut self) -> Vec<(String, Vec<PendingImage>)> {
        let drained = std::mem::take(&mut self.pending_compaction_inputs);
        let _ = self.app.take_queued_messages();
        drained
    }

    pub fn fields_mut(
        &mut self,
    ) -> (
        &mut App,
        &mut ConversationState,
        &mut HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    ) {
        (
            &mut self.app,
            &mut self.conversation,
            &mut self.image_protos,
        )
    }

    pub async fn poll_side_questions(&mut self) -> bool {
        let finished: Vec<usize> = self
            .pending_side_questions
            .iter()
            .enumerate()
            .filter_map(|(index, pending)| pending.task.is_finished().then_some(index))
            .collect();
        if finished.is_empty() {
            return false;
        }
        for index in finished.into_iter().rev() {
            let pending = self.pending_side_questions.remove(index);
            let outcome = pending.task.await;
            crate::slash::side_question::finish_side_question(
                &mut self.app,
                &pending.question,
                outcome,
            );
        }
        true
    }

    pub fn ensure_image_protocols(
        &mut self,
        picker: &ratatui_image::picker::Picker,
        render_areas: &[crate::display_types::ImageRenderArea],
    ) {
        use crate::display_types::MessageRole;
        for area in render_areas {
            let key = (area.msg_idx, area.tc_idx);
            if self.image_protos.contains_key(&key) {
                continue;
            }
            let Some(msg) = self.app.messages.get(area.msg_idx) else {
                continue;
            };
            let bytes: Option<&[u8]> = if msg.role == MessageRole::User {
                msg.images.get(area.tc_idx).map(Vec::as_slice)
            } else {
                msg.tool_calls
                    .get(area.tc_idx)
                    .and_then(|tc| tc.image_data.as_deref())
            };
            let Some(bytes) = bytes else { continue };
            let Ok(dyn_img) = image::load_from_memory(bytes) else {
                continue;
            };
            self.image_protos
                .insert(key, picker.new_resize_protocol(dyn_img));
        }
    }
}
