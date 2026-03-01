//! session types and management
//!
//! a session holds the conversation history and metadata for a single
//! agent interaction. sessions can be persisted, resumed, and branched.

use crate::tree::SessionTree;
use mush_ai::types::{Message, Timestamp};
use serde::{Deserialize, Serialize};

/// unique session identifier
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let time = Timestamp::now().as_ms();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id() as u64;
        let id = time
            .wrapping_mul(6364136223846793005)
            .wrapping_add(pid)
            .wrapping_add(count);
        Self(format!("{id:016x}"))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// metadata about a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub title: Option<String>,
    pub model_id: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub message_count: usize,
    pub cwd: String,
}

/// a full session with messages and tree structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
    /// tree structure for branching (optional for backwards compat)
    #[serde(default)]
    pub tree: SessionTree,
}

impl Session {
    pub fn new(model_id: &str, cwd: &str) -> Self {
        let now = Timestamp::now();
        let id = SessionId::new();
        Self {
            meta: SessionMeta {
                id,
                title: None,
                model_id: model_id.into(),
                created_at: now,
                updated_at: now,
                message_count: 0,
                cwd: cwd.into(),
            },
            messages: vec![],
            tree: SessionTree::new(),
        }
    }

    pub fn push_message(&mut self, message: Message) {
        self.tree.append_message(message.clone());
        self.messages.push(message);
        self.meta.message_count = self.messages.len();
        self.meta.updated_at = Timestamp::now();
    }

    /// get the conversation for the current branch (what the LLM sees)
    pub fn context(&self) -> Vec<Message> {
        if self.tree.is_empty() {
            // backwards compat: old sessions without tree
            self.messages.clone()
        } else {
            self.tree.build_context()
        }
    }

    /// set title from first user message if not already set
    pub fn auto_title(&mut self) {
        if self.meta.title.is_some() {
            return;
        }

        let first_text = self.messages.iter().find_map(|m| match m {
            Message::User(u) => match &u.content {
                mush_ai::types::UserContent::Text(t) => Some(t.as_str()),
                mush_ai::types::UserContent::Parts(parts) => parts.iter().find_map(|p| match p {
                    mush_ai::types::UserContentPart::Text(t) => Some(t.text.as_str()),
                    _ => None,
                }),
            },
            _ => None,
        });

        if let Some(text) = first_text {
            let title = if text.len() > 80 {
                format!("{}...", &text[..77])
            } else {
                text.to_string()
            };
            self.meta.title = Some(title);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::*;

    #[test]
    fn new_session_has_no_messages() {
        let session = Session::new("test-model", "/tmp");
        assert_eq!(session.messages.len(), 0);
        assert_eq!(session.meta.message_count, 0);
        assert!(session.meta.title.is_none());
    }

    #[test]
    fn push_message_updates_count() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp_ms: Timestamp(0),
        }));
        assert_eq!(session.meta.message_count, 1);
        assert_eq!(session.messages.len(), 1);
    }

    #[test]
    fn auto_title_from_first_message() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text("explain how rust traits work".into()),
            timestamp_ms: Timestamp(0),
        }));
        session.auto_title();
        assert_eq!(
            session.meta.title.as_deref(),
            Some("explain how rust traits work")
        );
    }

    #[test]
    fn auto_title_truncates_long_messages() {
        let mut session = Session::new("test-model", "/tmp");
        let long_text = "a".repeat(200);
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text(long_text),
            timestamp_ms: Timestamp(0),
        }));
        session.auto_title();
        let title = session.meta.title.unwrap();
        assert!(title.len() <= 83);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn session_id_is_unique() {
        let id1 = SessionId::new();
        let id2 = SessionId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn session_serialisation_roundtrip() {
        let mut session = Session::new("test-model", "/tmp/project");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text("hello".into()),
            timestamp_ms: Timestamp(1000),
        }));

        let json = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.meta.id, session.meta.id);
        assert_eq!(restored.messages.len(), 1);
    }
}
