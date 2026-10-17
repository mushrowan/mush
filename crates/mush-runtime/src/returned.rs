use mush_ai::types::{Message, UserContent, UserContentPart};
use mush_session::tree::{EntryId, EntryKind, SessionEntry, SessionTree};

use crate::session::{PendingImage, SessionRuntime};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReturnedPrompt {
    pub text: String,
    pub images: Vec<PendingImage>,
    pub error: String,
}

impl SessionRuntime {
    pub fn return_rejected_prompt(&mut self, error: &str) -> Option<ReturnedPrompt> {
        if !mush_ai::registry::is_unsendable_media_message(error) {
            return None;
        }
        let id = rejected_prompt(self.conversation.tree())?;
        let lifted = self.conversation.lift_out(&id)?;
        let EntryKind::Message {
            message: Message::User(user),
        } = lifted.kind
        else {
            return None;
        };
        self.refresh_context_tokens();
        let (text, images) = split(user.content);
        Some(ReturnedPrompt {
            text,
            images,
            error: error.to_string(),
        })
    }
}

fn rejected_prompt(tree: &SessionTree) -> Option<EntryId> {
    tree.current_branch()
        .into_iter()
        .rev()
        .take_while(|entry| !answered(entry))
        .find(|entry| carries_images(entry))
        .map(|entry| entry.id.clone())
}

fn answered(entry: &SessionEntry) -> bool {
    match &entry.kind {
        EntryKind::Message {
            message: Message::Assistant(reply),
        } => reply.is_replayable(),
        EntryKind::Message {
            message: Message::User(_) | Message::Event(_),
        }
        | EntryKind::Refinement { .. }
        | EntryKind::ThinkingLevelChange { .. } => false,
        EntryKind::Message {
            message: Message::ToolResult(_),
        }
        | EntryKind::BranchSummary { .. }
        | EntryKind::Compaction { .. }
        | EntryKind::Unknown(_) => true,
    }
}

fn carries_images(entry: &SessionEntry) -> bool {
    matches!(
        &entry.kind,
        EntryKind::Message {
            message: Message::User(user),
        } if matches!(
            &user.content,
            UserContent::Parts(parts) if parts.iter().any(|part| matches!(part, UserContentPart::Image(_)))
        )
    )
}

fn split(content: UserContent) -> (String, Vec<PendingImage>) {
    use base64::Engine;
    let parts = match content {
        UserContent::Text(text) => return (text, Vec::new()),
        UserContent::Parts(parts) => parts,
    };
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        match part {
            UserContentPart::Text(text) => texts.push(text.text),
            UserContentPart::Image(image) => {
                if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(&image.data) {
                    images.push(PendingImage {
                        data,
                        mime_type: image.mime_type,
                        dimensions: None,
                        note: None,
                    });
                }
            }
        }
    }
    (texts.join(" "), images)
}
