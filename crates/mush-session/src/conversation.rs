use mush_ai::types::{Message, ThinkingLevel};
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

    pub fn append_refinement(
        &mut self,
        source: crate::harness::RefinementSource,
        result: crate::harness::RefinementResult,
    ) -> EntryId {
        self.tree.append_refinement(source, result)
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        *self = Self::from_messages(messages);
    }

    pub fn context(&self) -> Vec<Message> {
        self.tree.build_context()
    }

    pub fn display_history(&self) -> Vec<Message> {
        self.tree.build_display_history()
    }

    pub fn context_prefix(&self, limit: usize) -> Vec<Message> {
        self.tree.build_context_prefix(limit)
    }

    pub fn context_len(&self) -> usize {
        self.tree.context_len()
    }

    pub fn messages(&self) -> Vec<Message> {
        self.context()
    }

    pub fn len(&self) -> usize {
        self.context_len()
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

    pub fn reset_leaf_with_summary(&mut self, summary: impl Into<String>) -> EntryId {
        self.tree.reset_leaf_with_summary(summary)
    }

    pub fn compact_at_leaf(
        &mut self,
        summary: String,
        first_kept_context_index: usize,
    ) -> Option<EntryId> {
        self.tree.compact_at_leaf(summary, first_kept_context_index)
    }

    pub fn apply_compaction(
        &mut self,
        summary: String,
        summarised_count: usize,
        messages: Vec<Message>,
    ) {
        if summary.is_empty()
            || summarised_count == 0
            || self.compact_at_leaf(summary, summarised_count).is_none()
        {
            self.replace_messages(messages);
        }
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

    pub fn thinking_level(&self) -> Option<ThinkingLevel> {
        self.tree.thinking_level()
    }

    pub fn record_thinking_level(&mut self, thinking_level: &ThinkingLevel) -> bool {
        if self.thinking_level().as_ref() == Some(thinking_level) {
            return false;
        }
        self.tree.append_thinking_level(thinking_level.clone());
        true
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
        if repr.tree.is_empty() && repr.tree.leaf_id().is_none() {
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

    #[test]
    fn a_branch_keeps_the_thinking_level_recorded_on_it() {
        let mut conversation = ConversationState::new();
        assert_eq!(conversation.thinking_level(), None);
        assert!(conversation.record_thinking_level(&ThinkingLevel::Medium));
        assert!(!conversation.record_thinking_level(&ThinkingLevel::Medium));
        let first = conversation.append_message(user_message("one"));
        assert!(conversation.record_thinking_level(&ThinkingLevel::High));
        conversation.append_message(user_message("two"));
        assert_eq!(conversation.thinking_level(), Some(ThinkingLevel::High));
        assert_eq!(conversation.context().len(), 2);
        assert_eq!(conversation.display_history().len(), 2);

        conversation.branch(&first);
        assert_eq!(conversation.thinking_level(), Some(ThinkingLevel::Medium));
        assert_eq!(conversation.context().len(), 1);

        let saved = serde_json::to_string(&conversation).unwrap();
        let restored: ConversationState = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.thinking_level(), Some(ThinkingLevel::Medium));
        assert!(saved.contains("\"thinking_level_change\""), "{saved}");
    }

    fn user_message(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
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
