//! the conversation on screen: display state, session tree and the
//! prompt waiting to go out

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

/// one conversation: its display, its session tree and its in-flight prompt
pub struct Session {
    pub app: App,
    pub conversation: ConversationState,
    pub image_protos: HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>,
    pub pending_prompt: Option<PendingPrompt>,
    pub steering_queue: Arc<Mutex<Vec<Message>>>,
    /// inputs the user submitted while a /compact (or /fork-compact)
    /// task was running. each entry is `(text, image_bytes)` mirroring
    /// what UserSubmit would normally have done. drained by the
    /// runtime once compaction lands so they replay as a fresh turn
    /// instead of getting silently dropped or clobbering the input box
    pub pending_compaction_inputs: Vec<(String, Vec<PendingImage>)>,
    /// user-facing label (auto-generated or manual)
    pub label: Option<String>,
    /// whether an LLM title has already been generated
    pub title_generated: bool,
}

impl Session {
    pub fn new(app: App) -> Self {
        Self {
            app,
            conversation: ConversationState::new(),
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            pending_compaction_inputs: Vec::new(),
            label: None,
            title_generated: false,
        }
    }

    /// resume from a stored conversation
    pub fn with_conversation(app: App, conversation: ConversationState) -> Self {
        Self {
            app,
            conversation,
            image_protos: HashMap::new(),
            pending_prompt: None,
            steering_queue: Arc::new(Mutex::new(Vec::new())),
            pending_compaction_inputs: Vec::new(),
            label: None,
            title_generated: false,
        }
    }

    /// stash a user submit while a /compact run is in flight.
    /// records the (text, images) for later replay, surfaces a
    /// queued (dimmed) bubble in the chat so the user sees it didn't
    /// vanish, and clears the input buffer so they can keep typing
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

    /// drain everything queued during the just-finished compaction.
    /// also clears the queued bubbles from the display so the runtime
    /// can render them as real (non-queued) user messages without
    /// duplicates
    pub fn drain_compaction_inputs(&mut self) -> Vec<(String, Vec<PendingImage>)> {
        let drained = std::mem::take(&mut self.pending_compaction_inputs);
        let _ = self.app.take_queued_messages();
        drained
    }

    /// mutable references to all major fields at once,
    /// enabling the borrow checker to see disjoint borrows
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

    /// ensure `image_protos` has a protocol for every `(msg_idx, tc_idx)`
    /// referenced by `render_areas`, creating missing entries on demand
    /// from the raw bytes stored on the matching message or tool call.
    ///
    /// this is a safety net for paths that register image render areas
    /// without eagerly creating a protocol: session reloads (protocols
    /// aren't persisted), steering submits with attached images, and
    /// any future pushes via `push_user_message_with_images` from a
    /// code path that forgets to poke `image_protos`
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
