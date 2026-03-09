//! session types and management
//!
//! a session holds the conversation history and metadata for a single
//! agent interaction. sessions can be persisted, resumed, and branched.

use crate::conversation::ConversationState;
use mush_ai::types::{Message, ModelId, PaneId, Timestamp};
use serde::{Deserialize, Serialize};

pub use mush_ai::types::SessionId;

/// metadata about a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub title: Option<String>,
    pub model_id: ModelId,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub message_count: usize,
    pub cwd: String,
}

/// per-pane session state for multi-pane persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSession {
    pub pane_id: PaneId,
    pub label: Option<String>,
    pub model_id: ModelId,
    #[serde(flatten)]
    pub conversation: ConversationState,
}

/// a full session with messages and tree structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub meta: SessionMeta,
    #[serde(flatten)]
    pub conversation: ConversationState,
    /// additional panes (empty for single-pane sessions)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneSession>,
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
            conversation: ConversationState::new(),
            panes: vec![],
        }
    }

    pub fn push_message(&mut self, message: Message) {
        self.conversation.append_message(message);
        self.meta.message_count = self.context().len();
        self.meta.updated_at = Timestamp::now();
    }

    /// get the conversation for the current branch (what the LLM sees)
    pub fn context(&self) -> Vec<Message> {
        self.conversation.context()
    }

    /// set title from first user message if not already set
    ///
    /// collapses whitespace, strips leading slash commands/skill hints,
    /// and truncates at a word boundary
    pub fn auto_title(&mut self) {
        if self.meta.title.is_some() {
            return;
        }

        let first_text = self.context().into_iter().find_map(|m| match m {
            Message::User(u) => {
                let t = u.text();
                if t.is_empty() { None } else { Some(t) }
            }
            _ => None,
        });

        if let Some(text) = first_text {
            self.meta.title = Some(clean_title(&text, 80));
        }
    }
}

/// clean up raw user text into a readable session title
///
/// - strips leading `[relevant skills: ...]` hints injected by prompt enrichment
/// - collapses all whitespace (newlines, tabs, runs of spaces) into single spaces
/// - truncates at a word boundary, appending "…" if shortened
fn clean_title(text: &str, max_len: usize) -> String {
    // strip leading skill hints like "[relevant skills: foo, bar. ...]"
    let stripped = if text.starts_with("[relevant skills:") {
        text.find("]\n")
            .or_else(|| text.find("] "))
            .map(|i| &text[i + 1..])
            .unwrap_or(text)
            .trim_start()
    } else {
        text
    };

    // collapse whitespace
    let collapsed: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() <= max_len {
        return collapsed;
    }

    // take up to max_len-1 chars, then find a word boundary to break at
    let truncated: String = collapsed.chars().take(max_len - 1).collect();
    let break_at = truncated.rfind(' ').unwrap_or(truncated.len());
    format!("{}…", &truncated[..break_at])
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::*;

    #[test]
    fn new_session_has_no_messages() {
        let session = Session::new("test-model", "/tmp");
        assert!(session.context().is_empty());
        assert_eq!(session.meta.message_count, 0);
        assert!(session.meta.title.is_none());
    }

    #[test]
    fn push_message_updates_count() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp_ms: Timestamp::zero(),
        }));
        assert_eq!(session.meta.message_count, 1);
        assert_eq!(session.context().len(), 1);
    }

    #[test]
    fn auto_title_from_first_message() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text("explain how rust traits work".into()),
            timestamp_ms: Timestamp::zero(),
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
        let long_text = "the quick brown fox jumps over the lazy dog and then proceeds to do many other things that make this message very long indeed";
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text(long_text.into()),
            timestamp_ms: Timestamp::zero(),
        }));
        session.auto_title();
        let title = session.meta.title.unwrap();
        assert!(title.len() <= 81); // 80 + 1 for multi-byte …
        assert!(title.ends_with('…'));
        // should break at a word boundary
        assert!(!title.trim_end_matches('…').ends_with(' '));
    }

    #[test]
    fn auto_title_collapses_whitespace() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text("hello\n\nworld\t  foo".into()),
            timestamp_ms: Timestamp::zero(),
        }));
        session.auto_title();
        assert_eq!(session.meta.title.as_deref(), Some("hello world foo"));
    }

    #[test]
    fn auto_title_strips_skill_hints() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text(
                "[relevant skills: commit, jj. follow their instructions.]\nfix the build".into(),
            ),
            timestamp_ms: Timestamp::zero(),
        }));
        session.auto_title();
        assert_eq!(session.meta.title.as_deref(), Some("fix the build"));
    }

    #[test]
    fn clean_title_basics() {
        assert_eq!(clean_title("hello world", 80), "hello world");
        assert_eq!(clean_title("  spaced  out  ", 80), "spaced out");
        assert_eq!(
            clean_title("[relevant skills: foo, bar. instructions.]\ndo stuff", 80),
            "do stuff"
        );
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
            timestamp_ms: Timestamp::from_ms(1000),
        }));

        let json = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.meta.id, session.meta.id);
        assert_eq!(restored.context().len(), 1);
    }
}
