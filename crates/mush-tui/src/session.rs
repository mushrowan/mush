use std::collections::HashMap;

use mush_session::ConversationState;

use crate::app::App;
use crate::input_buffer::PendingImage;

pub use mush_runtime::{AgentConnection, InProcessConnection, PendingPrompt, SessionRuntime};

pub type ImageProtocols = HashMap<(usize, usize), ratatui_image::protocol::StatefulProtocol>;

pub struct Session {
    pub app: App,
    pub connection: Box<dyn AgentConnection>,
    pub image_protos: ImageProtocols,
    pub label: Option<String>,
}

impl Session {
    pub fn new(app: App) -> Self {
        Self::with_conversation(app, ConversationState::new())
    }

    pub fn with_conversation(app: App, conversation: ConversationState) -> Self {
        Self::with_connection(
            app,
            Box::new(InProcessConnection::detached(SessionRuntime::new(
                conversation,
            ))),
        )
    }

    pub fn with_connection(app: App, connection: Box<dyn AgentConnection>) -> Self {
        Self {
            app,
            connection,
            image_protos: HashMap::new(),
            label: None,
        }
    }

    pub fn app_and_runtime(&mut self) -> (&mut App, &mut SessionRuntime) {
        let connection = self
            .connection
            .as_any_mut()
            .downcast_mut::<InProcessConnection>()
            .unwrap_or_else(|| unreachable!("this session is attached to a remote worker"));
        (&mut self.app, &mut connection.runtime)
    }

    pub fn in_process(&mut self) -> &mut InProcessConnection {
        self.connection
            .as_any_mut()
            .downcast_mut()
            .unwrap_or_else(|| unreachable!("this session is attached to a remote worker"))
    }

    pub async fn queue_input_during_compaction(&mut self, text: String, images: Vec<PendingImage>) {
        let image_bytes: Vec<Vec<u8>> = images.iter().map(|image| image.data.clone()).collect();
        self.app
            .push_queued_message_with_images(text.clone(), image_bytes);
        self.app.input.text.clear();
        self.app.input.cursor = 0;
        let _ = self
            .connection
            .send(mush_runtime::ClientCommand::QueuePrompt { text, images })
            .await;
    }

    pub fn fields_mut(&mut self) -> (&mut App, &mut ImageProtocols) {
        (&mut self.app, &mut self.image_protos)
    }
}
