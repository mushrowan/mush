use std::fs::File;
use std::io;
use std::path::Path;

use mush_ai::types::{Message, PromptId, Timestamp, UserContent, UserMessage};
pub use mush_supervisor::inbox::{Binding, Event, Inbox, TerminalReason};

use crate::{ConversationState, EntryKind, SessionId, SessionStore};

pub struct Consumer {
    inbox: Inbox,
    _lock: File,
    session_id: SessionId,
    binding: Binding,
}

pub fn contains(conversation: &ConversationState, event_id: &str) -> bool {
    let prompt_id = PromptId::from(format!("orchestrator-event-{event_id}"));
    conversation.entries().iter().any(|entry| {
        matches!(&entry.kind, EntryKind::Message { message: Message::User(user) } if user.prompt_id.as_ref() == Some(&prompt_id))
    })
}

pub fn append(conversation: &mut ConversationState, event: &Event) -> io::Result<bool> {
    if contains(conversation, &event.event_id) {
        return Ok(false);
    }
    conversation.append_message(Message::User(
        UserMessage::builder()
            .content(UserContent::Text(event.message()?))
            .timestamp_ms(Timestamp::now())
            .prompt_id(PromptId::from(format!(
                "orchestrator-event-{}",
                event.event_id
            )))
            .build(),
    ));
    Ok(true)
}

impl Consumer {
    pub fn matches(&self, session_id: &SessionId) -> bool {
        self.session_id == *session_id
    }

    pub fn open(path: &Path, session_id: SessionId, cwd: &Path) -> io::Result<Self> {
        let inbox = Inbox::open(path)?;
        let lock = inbox.consumer(&session_id, cwd)?;
        let binding = inbox.binding()?;
        Ok(Self {
            inbox,
            _lock: lock,
            session_id,
            binding,
        })
    }

    pub fn pending(&self) -> io::Result<Vec<Event>> {
        self.check_binding()?;
        self.inbox.pending()
    }

    fn check_binding(&self) -> io::Result<()> {
        if self.inbox.binding()? != self.binding {
            return Err(io::Error::other("orchestrator inbox binding changed"));
        }
        Ok(())
    }

    pub fn acknowledge_saved(&self, event_id: &str, store: &SessionStore) -> io::Result<bool> {
        self.check_binding()?;
        let saved = match store.load(&self.session_id) {
            Ok(saved) => saved,
            Err(crate::StoreError::NotFound(_)) => return Ok(false),
            Err(error) => return Err(io::Error::other(error)),
        };
        if !contains(&saved.conversation, event_id) {
            return Ok(false);
        }
        self.inbox.acknowledge(event_id)?;
        Ok(true)
    }
}
