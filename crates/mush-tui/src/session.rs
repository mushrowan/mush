use std::collections::HashMap;

use mush_session::ConversationState;

use crate::app::App;
use crate::input_buffer::PendingImage;

pub use mush_runtime::{AgentConnection, InProcessConnection, PendingPrompt, SessionRuntime};

pub struct Session {
    pub app: App,
    pub connection: InProcessConnection,
    pub image_protos: HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    pub label: Option<String>,
    pub pending_side_questions: Vec<crate::slash::side_question::PendingSideQuestion>,
}

impl Session {
    pub fn new(app: App) -> Self {
        Self::with_conversation(app, ConversationState::new())
    }

    pub fn with_conversation(app: App, conversation: ConversationState) -> Self {
        Self::with_connection(
            app,
            InProcessConnection::detached(SessionRuntime::new(conversation)),
        )
    }

    pub fn with_connection(app: App, connection: InProcessConnection) -> Self {
        Self {
            app,
            connection,
            image_protos: HashMap::new(),
            label: None,
            pending_side_questions: Vec::new(),
        }
    }

    pub async fn queue_input_during_compaction(&mut self, text: String, images: Vec<PendingImage>) {
        let image_bytes: Vec<Vec<u8>> = images.iter().map(|image| image.data.clone()).collect();
        if image_bytes.is_empty() {
            self.app.push_queued_message(text.clone());
        } else {
            self.app
                .push_queued_message_with_images(text.clone(), image_bytes);
        }
        self.app.input.text.clear();
        self.app.input.cursor = 0;
        let _ = self
            .connection
            .send(mush_runtime::ClientCommand::QueuePrompt { text, images })
            .await;
    }

    pub fn fields_mut(
        &mut self,
    ) -> (
        &mut App,
        &mut HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    ) {
        (&mut self.app, &mut self.image_protos)
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
