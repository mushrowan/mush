//! canonical conversation state
//!
//! conversation history is stored canonically as a session tree.
//! flat message lists are derived views used for compatibility and
//! for building the llm context for the current branch.

use mush_ai::types::Message;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::tree::{EntryId, SessionEntry, SessionTree, TreeNode};

#[derive(Debug, Clone, Default)]
pub struct ConversationState {
    tree: SessionTree,
}

#[derive(Serialize, Deserialize)]
struct ConversationStateRepr {
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    tree: SessionTree,
}

impl ConversationState {
    pub fn new() -> Self {
        Self {
            tree: SessionTree::new(),
        }
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        let mut tree = SessionTree::new();
        for message in messages {
            tree.append_message(message);
        }
        Self { tree }
    }

    pub fn from_tree(tree: SessionTree) -> Self {
        Self { tree }
    }

    pub fn append_message(&mut self, message: Message) -> EntryId {
        self.tree.append_message(message)
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        *self = Self::from_messages(messages);
    }

    pub fn context(&self) -> Vec<Message> {
        self.tree.build_context()
    }

    pub fn messages(&self) -> Vec<Message> {
        self.context()
    }

    pub fn len(&self) -> usize {
        self.context().len()
    }

    pub fn tree(&self) -> &SessionTree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut SessionTree {
        &mut self.tree
    }

    pub fn branch(&mut self, from_id: &EntryId) -> bool {
        self.tree.branch(from_id)
    }

    pub fn branch_with_summary(
        &mut self,
        from_id: &EntryId,
        summary: impl Into<String>,
    ) -> Option<EntryId> {
        self.tree.branch_with_summary(from_id, summary)
    }

    pub fn reset_leaf(&mut self) {
        self.tree.reset_leaf();
    }

    pub fn leaf_id(&self) -> Option<&EntryId> {
        self.tree.leaf_id()
    }

    pub fn entries(&self) -> &[SessionEntry] {
        self.tree.entries()
    }

    pub fn tree_nodes(&self) -> Vec<TreeNode> {
        self.tree.tree()
    }

    pub fn user_messages_in_branch(&self) -> Vec<&SessionEntry> {
        self.tree.user_messages_in_branch()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn is_branch_point(&self, id: &EntryId) -> bool {
        self.tree.is_branch_point(id)
    }
}

impl Serialize for ConversationState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ConversationStateRepr {
            messages: self.context(),
            tree: self.tree.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConversationState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let repr = ConversationStateRepr::deserialize(deserializer)?;
        if repr.tree.is_empty() {
            Ok(Self::from_messages(repr.messages))
        } else {
            Ok(Self::from_tree(repr.tree))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Message, Timestamp, UserContent, UserMessage};

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
    }

    #[test]
    fn from_messages_builds_context() {
        let state = ConversationState::from_messages(vec![user_message("hello")]);
        let context = state.context();
        assert_eq!(context.len(), 1);
    }

    #[test]
    fn deserialize_legacy_messages_only() {
        let json = serde_json::json!({
            "messages": [user_message("hello")],
        });
        let state: ConversationState = serde_json::from_value(json).unwrap();
        assert_eq!(state.context().len(), 1);
    }
}
