use crate::conversation::ConversationState;
use mush_ai::types::{Message, ModelId, PaneId, SessionAccounts, Timestamp};
use serde::{Deserialize, Serialize};

pub use mush_ai::types::SessionId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub title: Option<String>,
    pub model_id: ModelId,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub message_count: usize,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_model: Option<String>,
    #[serde(default, skip_serializing_if = "SessionAccounts::is_empty")]
    pub accounts: SessionAccounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<Pause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rlm_max_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Lifecycle::is_live")]
    pub lifecycle: Lifecycle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    #[default]
    Live,
    Archived,
}

impl Lifecycle {
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl SyncState {
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.taken_by.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pause {
    pub at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSession {
    pub pane_id: PaneId,
    pub label: Option<String>,
    pub model_id: ModelId,
    #[serde(flatten)]
    pub conversation: ConversationState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub meta: SessionMeta,
    #[serde(flatten)]
    pub conversation: ConversationState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneSession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
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
                parent: None,
                subagent_model: None,
                accounts: SessionAccounts::default(),
                paused: None,
                rlm_max_depth: None,
                lifecycle: Lifecycle::Live,
            },
            conversation: ConversationState::new(),
            panes: vec![],
            system_prompt: None,
        }
    }

    pub fn push_message(&mut self, message: Message) {
        self.conversation.append_message(message);
        self.meta.message_count = self.conversation.context_len();
        self.meta.updated_at = Timestamp::now();
    }

    pub fn context(&self) -> Vec<Message> {
        self.conversation.context()
    }

    pub fn auto_title(&mut self) {
        if self.meta.title.is_some() {
            return;
        }

        let first_text = self
            .conversation
            .user_messages_in_branch()
            .into_iter()
            .find_map(|entry| match &entry.kind {
                crate::tree::EntryKind::Message {
                    message: Message::User(user),
                } => {
                    let text = user.text();
                    if text.is_empty() { None } else { Some(text) }
                }
                _ => None,
            });

        if let Some(text) = first_text {
            self.meta.title = Some(clean_title(&text, 80));
        }
    }
}

fn clean_title(text: &str, max_len: usize) -> String {
    let stripped = if text.starts_with("[relevant skills:") {
        text.find("]\n")
            .or_else(|| text.find("] "))
            .map(|i| &text[i + 1..])
            .unwrap_or(text)
            .trim_start()
    } else {
        text
    };

    let collapsed: String = stripped.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() <= max_len {
        return collapsed;
    }

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
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));
        assert_eq!(session.meta.message_count, 1);
        assert_eq!(session.context().len(), 1);
    }

    #[test]
    fn auto_title_from_first_message() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("explain how rust traits work".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));
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
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text(long_text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));
        session.auto_title();
        let title = session.meta.title.unwrap();
        assert!(title.len() <= 81);
        assert!(title.ends_with('…'));
        assert!(!title.trim_end_matches('…').ends_with(' '));
    }

    #[test]
    fn auto_title_collapses_whitespace() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hello\n\nworld\t  foo".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));
        session.auto_title();
        assert_eq!(session.meta.title.as_deref(), Some("hello world foo"));
    }

    #[test]
    fn auto_title_strips_skill_hints() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text(
                    "[relevant skills: commit, jj. follow their instructions.]\nfix the build"
                        .into(),
                ))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));
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
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hello".into()))
                .timestamp_ms(Timestamp::from_ms(1000))
                .build(),
        ));

        let json = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.meta.id, session.meta.id);
        assert_eq!(restored.context().len(), 1);
    }

    #[test]
    fn session_with_panes_roundtrips() {
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("main pane".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));

        let mut pane_conv = ConversationState::new();
        pane_conv.append_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("pane 2".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        ));
        session.panes.push(PaneSession {
            pane_id: PaneId::new(2),
            label: Some("helper".into()),
            model_id: "gpt-4o".into(),
            conversation: pane_conv,
        });

        let json = serde_json::to_string(&session).unwrap();
        let restored: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.panes.len(), 1);
        assert_eq!(restored.panes[0].pane_id, PaneId::new(2));
        assert_eq!(restored.panes[0].label.as_deref(), Some("helper"));
        assert_eq!(restored.panes[0].conversation.context().len(), 1);
    }

    #[test]
    fn session_without_panes_omits_field() {
        let session = Session::new("test-model", "/tmp");
        let json = serde_json::to_string(&session).unwrap();
        assert!(!json.contains("panes"));

        let restored: Session = serde_json::from_str(&json).unwrap();
        assert!(restored.panes.is_empty());
    }
}
