//! session tree - append-only tree structure for conversation branching
//!
//! each entry has an id and parent_id forming a tree. a leaf pointer tracks
//! the current position. appending creates a child of the leaf. branching
//! moves the leaf to an earlier entry, so the next append starts a new branch.
//! existing entries are never modified or deleted.

use derive_more::Display;
use mush_ai::types::*;
use serde::{Deserialize, Serialize};

/// unique entry identifier (8 hex chars)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Display, Serialize, Deserialize)]
pub struct EntryId(String);

impl From<&str> for EntryId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl std::ops::Deref for EntryId {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl EntryId {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let t = Timestamp::now().as_ms();
        let hash = t.wrapping_mul(2654435761).wrapping_add(n);
        Self(format!("{:08x}", hash as u32))
    }
}

impl Default for EntryId {
    fn default() -> Self {
        Self::new()
    }
}

/// a single entry in the session tree
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp: Timestamp,
    pub kind: EntryKind,
}

/// what kind of entry this is
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    /// a conversation message (user, assistant, or tool result)
    Message { message: Message },
    /// summary of an abandoned branch (injected when branching)
    BranchSummary { summary: String, from_id: EntryId },
    /// compaction summary replacing older entries
    Compaction {
        summary: String,
        first_kept_id: EntryId,
    },
}

/// tree node for getTree() - entry with its children
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub entry: SessionEntry,
    pub children: Vec<TreeNode>,
}

/// append-only session tree with a leaf pointer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTree {
    entries: Vec<SessionEntry>,
    leaf_id: Option<EntryId>,
}

fn push_context_message(
    messages: &mut Vec<Message>,
    limit: Option<usize>,
    message: Message,
) -> bool {
    if let Some(limit) = limit
        && messages.len() >= limit
    {
        return false;
    }

    messages.push(message);
    if let Some(limit) = limit {
        messages.len() < limit
    } else {
        true
    }
}

/// render a single entry as the message it contributes to a conversation
/// view. real messages pass through; branch and compaction markers become
/// their synthetic summary user messages
fn entry_to_message(entry: &SessionEntry) -> Message {
    match &entry.kind {
        EntryKind::Message { message } => message.clone(),
        EntryKind::BranchSummary { summary, .. } => Message::User(UserMessage {
            content: UserContent::Text(format!(
                "Context from a previous conversation branch:\n\n\
                 <branch_summary>\n{summary}\n</branch_summary>"
            )),
            timestamp_ms: entry.timestamp,
        }),
        EntryKind::Compaction { summary, .. } => Message::User(UserMessage {
            content: UserContent::Text(format!(
                "The conversation history before this point was compacted \
                 into the following summary:\n\n<summary>\n{summary}\n</summary>"
            )),
            timestamp_ms: entry.timestamp,
        }),
    }
}

impl SessionTree {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            leaf_id: None,
        }
    }

    /// append a message as child of current leaf, advance leaf
    pub fn append_message(&mut self, message: Message) -> EntryId {
        let entry = SessionEntry {
            id: EntryId::new(),
            parent_id: self.leaf_id.clone(),
            timestamp: Timestamp::now(),
            kind: EntryKind::Message { message },
        };
        let id = entry.id.clone();
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        id
    }

    /// start a new branch from an earlier entry.
    /// moves the leaf pointer so the next append creates a child of that entry.
    pub fn branch(&mut self, from_id: &EntryId) -> bool {
        if self.by_id(from_id).is_some() {
            self.leaf_id = Some(from_id.clone());
            true
        } else {
            false
        }
    }

    /// branch with a summary of the abandoned path
    pub fn branch_with_summary(
        &mut self,
        from_id: &EntryId,
        summary: impl Into<String>,
    ) -> Option<EntryId> {
        self.by_id(from_id)?;
        let old_leaf = self.leaf_id.clone().unwrap_or_else(|| from_id.clone());
        self.leaf_id = Some(from_id.clone());

        let entry = SessionEntry {
            id: EntryId::new(),
            parent_id: Some(from_id.clone()),
            timestamp: Timestamp::now(),
            kind: EntryKind::BranchSummary {
                summary: summary.into(),
                from_id: old_leaf,
            },
        };
        let id = entry.id.clone();
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        Some(id)
    }

    /// add a compaction marker at the current leaf summarising the first
    /// `first_kept_context_index` context messages of the current branch.
    /// existing entries are left in place so `/tree` and `/branch` can
    /// still navigate back to pre-compaction state. the LLM context
    /// produced by [`SessionTree::build_context`] will collapse to the
    /// summary plus the messages from `first_kept_id` onward.
    ///
    /// `first_kept_context_index` indexes into the collapsed context
    /// (exactly what [`SessionTree::build_context`] returns), so it stays
    /// correct when the branch already holds a prior compaction: the new
    /// marker folds the earlier summary into itself and keeps the recent
    /// tail rather than re-expanding already-summarised history. returns
    /// `None` if the index is out of range or lands on a prior compaction
    /// summary (index 0 of an already-compacted branch, a no-op).
    pub fn compact_at_leaf(
        &mut self,
        summary: String,
        first_kept_context_index: usize,
    ) -> Option<EntryId> {
        let first_kept_id = {
            let context = self.context_entries();
            let entry = context.get(first_kept_context_index)?;
            // the leading context entry can be a prior compaction node
            // (its synthetic summary). that only happens at index 0, which
            // would summarise nothing, so reject it.
            if matches!(entry.kind, EntryKind::Compaction { .. }) {
                return None;
            }
            entry.id.clone()
        };
        let parent_id = self.leaf_id.clone();
        let entry = SessionEntry {
            id: EntryId::new(),
            parent_id,
            timestamp: Timestamp::now(),
            kind: EntryKind::Compaction {
                summary,
                first_kept_id,
            },
        };
        let id = entry.id.clone();
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        Some(id)
    }

    /// reset leaf to before any entries (next append creates a root)
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    /// get the current leaf id
    pub fn leaf_id(&self) -> Option<&EntryId> {
        self.leaf_id.as_ref()
    }

    /// walk from leaf to root, returning entries in root-first order
    pub fn current_branch(&self) -> Vec<&SessionEntry> {
        let index = self.build_index();
        let mut path = Vec::new();
        let mut current = self.leaf_id.as_ref();
        while let Some(id) = current {
            if let Some(entry) = index.get(&**id) {
                path.push(*entry);
                current = entry.parent_id.as_ref();
            } else {
                break;
            }
        }
        path.reverse();
        path
    }

    /// build the conversation messages for the current branch (what the LLM sees)
    pub fn build_context(&self) -> Vec<Message> {
        self.collect_context(None)
    }

    /// build at most the first `limit` visible context messages
    pub fn build_context_prefix(&self, limit: usize) -> Vec<Message> {
        self.collect_context(Some(limit))
    }

    /// build the full branch as messages for display, without collapsing
    /// compacted history. every entry from root to leaf is rendered, so
    /// resumed sessions show their whole scrollback while the LLM context
    /// (from [`SessionTree::build_context`]) stays collapsed. compaction
    /// and branch markers render as their synthetic summary messages
    pub fn build_display_history(&self) -> Vec<Message> {
        self.current_branch()
            .into_iter()
            .map(entry_to_message)
            .collect()
    }

    /// count visible context messages without cloning them
    pub fn context_len(&self) -> usize {
        self.context_entries().len()
    }

    /// the branch entries that [`SessionTree::build_context`] turns into
    /// messages, in order. collapses to the most recent compaction node's
    /// summary (represented by the compaction entry itself, always first
    /// when present) followed by the messages kept after it, then the
    /// live tail. single source of truth for context ordering, shared by
    /// `build_context`, `context_len`, and `compact_at_leaf` so their
    /// indices cannot drift.
    fn context_entries(&self) -> Vec<&SessionEntry> {
        let branch = self.current_branch();
        let mut entries: Vec<&SessionEntry> = Vec::with_capacity(branch.len());

        let compaction_idx = branch
            .iter()
            .rposition(|e| matches!(e.kind, EntryKind::Compaction { .. }));

        let start = if let Some(idx) = compaction_idx {
            entries.push(branch[idx]);
            if let EntryKind::Compaction { first_kept_id, .. } = &branch[idx].kind
                && let Some(kept_pos) = branch.iter().position(|e| e.id == *first_kept_id)
            {
                for &entry in &branch[kept_pos..idx] {
                    if matches!(entry.kind, EntryKind::Message { .. }) {
                        entries.push(entry);
                    }
                }
            }
            idx + 1
        } else {
            0
        };

        for &entry in &branch[start..] {
            if matches!(
                entry.kind,
                EntryKind::Message { .. } | EntryKind::BranchSummary { .. }
            ) {
                entries.push(entry);
            }
        }

        entries
    }

    fn collect_context(&self, limit: Option<usize>) -> Vec<Message> {
        let entries = self.context_entries();
        let capacity = limit.map_or(entries.len(), |limit| limit.min(entries.len()));
        let mut messages = Vec::with_capacity(capacity);

        for entry in entries {
            if !push_context_message(&mut messages, limit, entry_to_message(entry)) {
                return messages;
            }
        }

        messages
    }

    /// get all entries
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// get entry by id
    pub fn by_id(&self, id: &EntryId) -> Option<&SessionEntry> {
        self.entries.iter().find(|e| e.id == *id)
    }

    /// get direct children of an entry
    pub fn children(&self, parent_id: &EntryId) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent_id.as_ref() == Some(parent_id))
            .collect()
    }

    /// get root entries (no parent)
    pub fn roots(&self) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent_id.is_none())
            .collect()
    }

    /// build the full tree structure
    pub fn tree(&self) -> Vec<TreeNode> {
        fn build_subtree<'a>(entry: &'a SessionEntry, all: &'a [SessionEntry]) -> TreeNode {
            let children: Vec<TreeNode> = all
                .iter()
                .filter(|e| e.parent_id.as_ref() == Some(&entry.id))
                .map(|child| build_subtree(child, all))
                .collect();
            TreeNode {
                entry: entry.clone(),
                children,
            }
        }

        self.entries
            .iter()
            .filter(|e| e.parent_id.is_none())
            .map(|root| build_subtree(root, &self.entries))
            .collect()
    }

    /// find user message entries in the current branch (for navigation UI)
    pub fn user_messages_in_branch(&self) -> Vec<&SessionEntry> {
        self.current_branch()
            .into_iter()
            .filter(|e| {
                matches!(
                    &e.kind,
                    EntryKind::Message {
                        message: Message::User(_)
                    }
                )
            })
            .collect()
    }

    /// number of entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// check if a given entry has multiple children (a branch point)
    pub fn is_branch_point(&self, id: &EntryId) -> bool {
        self.children(id).len() > 1
    }

    fn build_index(&self) -> std::collections::HashMap<&str, &SessionEntry> {
        self.entries.iter().map(|e| (&*e.id, e)).collect()
    }
}

impl Default for SessionTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: text.into(),
                })])
                .model("test".into())
                .provider(Provider::Custom("test".into()))
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[test]
    fn empty_tree() {
        let tree = SessionTree::new();
        assert!(tree.is_empty());
        assert!(tree.leaf_id().is_none());
        assert!(tree.current_branch().is_empty());
        assert!(tree.build_context().is_empty());
    }

    #[test]
    fn linear_conversation() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("hello"));
        tree.append_message(assistant_msg("hi there"));
        tree.append_message(user_msg("how are you?"));

        assert_eq!(tree.len(), 3);
        assert_eq!(tree.current_branch().len(), 3);
        assert_eq!(tree.build_context().len(), 3);
    }

    #[test]
    fn branch_creates_fork() {
        let mut tree = SessionTree::new();
        let u1 = tree.append_message(user_msg("hello"));
        let _a1 = tree.append_message(assistant_msg("hi"));
        let _u2 = tree.append_message(user_msg("tell me about rust"));
        let _a2 = tree.append_message(assistant_msg("rust is great"));

        // branch from after the first user message
        assert!(tree.branch(&u1));

        // now we're at u1, append a different question
        tree.append_message(assistant_msg("hey!"));
        tree.append_message(user_msg("tell me about python"));

        // the branch should show: u1 -> a1_new -> u2_new
        let branch = tree.current_branch();
        assert_eq!(branch.len(), 3);

        // original entries still exist
        assert_eq!(tree.len(), 6);

        // tree should have a branch point at u1
        assert!(tree.is_branch_point(&u1));
    }

    #[test]
    fn branch_with_summary() {
        let mut tree = SessionTree::new();
        let u1 = tree.append_message(user_msg("hello"));
        tree.append_message(assistant_msg("hi"));
        tree.append_message(user_msg("topic A"));
        tree.append_message(assistant_msg("about topic A..."));

        let summary_id = tree
            .branch_with_summary(&u1, "discussed topic A briefly")
            .unwrap();

        // leaf should be the summary entry
        assert_eq!(tree.leaf_id(), Some(&summary_id));

        // context should include the branch summary
        let ctx = tree.build_context();
        assert_eq!(ctx.len(), 2); // u1 + branch_summary
        if let Message::User(u) = &ctx[1] {
            match &u.content {
                UserContent::Text(t) => assert!(t.contains("topic A briefly")),
                _ => panic!("expected text"),
            }
        }
    }

    #[test]
    fn build_context_linear() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));
        tree.append_message(user_msg("q2"));

        let ctx = tree.build_context();
        assert_eq!(ctx.len(), 3);
    }

    #[test]
    fn build_context_after_branch() {
        let mut tree = SessionTree::new();
        let u1 = tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));
        tree.append_message(user_msg("q2")); // this gets abandoned
        tree.append_message(assistant_msg("a2")); // this too

        tree.branch(&u1);
        tree.append_message(assistant_msg("a1_v2"));
        tree.append_message(user_msg("q2_v2"));

        let ctx = tree.build_context();
        assert_eq!(ctx.len(), 3); // u1 -> a1_v2 -> q2_v2
    }

    #[test]
    fn context_len_matches_built_context() {
        let mut tree = SessionTree::new();
        let u1 = tree.append_message(user_msg("hello"));
        tree.append_message(assistant_msg("hi"));
        tree.append_message(user_msg("topic A"));
        tree.append_message(assistant_msg("about topic A"));
        tree.branch_with_summary(&u1, "discussed topic A briefly");

        assert_eq!(tree.context_len(), tree.build_context().len());
    }

    #[test]
    fn compact_at_leaf_preserves_old_entries_and_collapses_context() {
        // simulate a /compact: long conversation, then compact at index 3
        // (keep last 2 messages, summarise the first 3)
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));
        tree.append_message(user_msg("q2"));
        tree.append_message(assistant_msg("a2"));
        let kept_first = tree.append_message(user_msg("q3"));
        let _kept_last = tree.append_message(assistant_msg("a3"));
        let entries_before = tree.entries().len();

        let compaction_id = tree
            .compact_at_leaf("summary of q1/a1/q2/a2".into(), 4)
            .expect("compaction should succeed when index is in current branch");

        // leaf moves to the compaction node
        assert_eq!(tree.leaf_id(), Some(&compaction_id));

        // all six pre-compaction entries still in the tree (plus the new
        // compaction node = 7 entries total). the user can /branch back
        assert_eq!(tree.entries().len(), entries_before + 1);

        // LLM context now sees: synthetic summary message + last 2 kept messages
        let ctx = tree.build_context();
        assert_eq!(ctx.len(), 3);
        if let Message::User(u) = &ctx[0] {
            if let UserContent::Text(t) = &u.content {
                assert!(t.contains("summary of q1/a1/q2/a2"));
            } else {
                panic!("expected text content in summary message");
            }
        } else {
            panic!("expected first context message to be the summary");
        }
        // first kept message is q3
        if let Message::User(u) = &ctx[1] {
            if let UserContent::Text(t) = &u.content {
                assert_eq!(t.as_str(), "q3");
            }
        } else {
            panic!("expected q3 as first kept message");
        }

        // sanity: the Compaction entry's first_kept_id points at the q3 entry
        if let EntryKind::Compaction { first_kept_id, .. } = &tree
            .by_id(&compaction_id)
            .expect("compaction entry present")
            .kind
        {
            assert_eq!(first_kept_id, &kept_first);
        } else {
            panic!("expected Compaction kind on new entry");
        }
    }

    #[test]
    fn repeated_compaction_collapses_without_reexpanding() {
        // regression: the second /compact on an already-compacted branch
        // must keep only the recent messages, not re-expand everything the
        // first compaction already summarised away. the index passed to
        // compact_at_leaf is relative to the *collapsed* context that
        // build_context produces, not the raw root-to-leaf branch.
        let mut tree = SessionTree::new();
        for i in 0..20 {
            if i % 2 == 0 {
                tree.append_message(user_msg(&format!("q{i}")));
            } else {
                tree.append_message(assistant_msg(&format!("a{i}")));
            }
        }

        // first compaction: summarise the first 15, keep the last 5
        tree.compact_at_leaf("first summary".into(), 15)
            .expect("first compaction should succeed");
        assert_eq!(tree.build_context().len(), 6, "summary + 5 kept");

        // conversation continues with 10 more messages
        for i in 20..30 {
            if i % 2 == 0 {
                tree.append_message(user_msg(&format!("q{i}")));
            } else {
                tree.append_message(assistant_msg(&format!("a{i}")));
            }
        }
        // collapsed context is now first-summary + 5 + 10 = 16
        assert_eq!(tree.build_context().len(), 16);

        // second compaction: summarise the first 10 of the *collapsed*
        // context (index 10 = "q24"), keep the last 6
        tree.compact_at_leaf("second summary".into(), 10)
            .expect("second compaction should succeed");

        let ctx = tree.build_context();
        assert_eq!(
            ctx.len(),
            7,
            "second compaction must collapse to summary + 6 kept, not re-expand. got {} messages",
            ctx.len()
        );

        let texts: Vec<String> = ctx
            .iter()
            .map(|m| match m {
                Message::User(u) => u.text(),
                Message::Assistant(a) => a.text(),
                _ => String::new(),
            })
            .collect();

        assert!(
            texts[0].contains("second summary"),
            "leading message must be the new summary, got {:?}",
            texts[0]
        );
        // the 6 most recent real messages survive verbatim
        assert!(
            texts.iter().any(|t| t == "q24"),
            "kept window start q24 missing: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "a29"),
            "kept window end a29 missing: {texts:?}"
        );
        // anything the first compaction folded away must stay folded
        assert!(
            !texts.iter().any(|t| t == "q0" || t == "a15" || t == "a23"),
            "summarised history leaked back into context: {texts:?}"
        );
    }

    fn text_of(m: &Message) -> String {
        match m {
            Message::User(u) => u.text(),
            Message::Assistant(a) => a.text(),
            _ => String::new(),
        }
    }

    #[test]
    fn build_display_history_keeps_pre_compaction_messages_and_banner() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));
        tree.append_message(user_msg("q2"));
        tree.append_message(assistant_msg("a2"));
        tree.append_message(user_msg("q3"));
        tree.append_message(assistant_msg("a3"));
        tree.compact_at_leaf("summary".into(), 4)
            .expect("compaction applies");

        // the LLM context collapses to summary + last two messages
        assert_eq!(tree.build_context().len(), 3);

        // the display history keeps every original message plus a banner
        let history = tree.build_display_history();
        assert_eq!(history.len(), 7, "6 originals + 1 compaction banner");
        assert_eq!(text_of(&history[0]), "q1");
        assert_eq!(text_of(&history[5]), "a3");
        assert!(
            text_of(&history[6]).contains("compacted into the following summary"),
            "the compaction banner should follow the pre-compaction messages"
        );
    }

    #[test]
    fn compact_at_leaf_rejects_index_past_branch() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));

        // only 2 messages in branch, index 5 is out of range
        assert!(tree.compact_at_leaf("summary".into(), 5).is_none());
    }

    #[test]
    fn build_context_prefix_limits_clones() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));
        tree.append_message(user_msg("q2"));

        let prefix = tree.build_context_prefix(2);
        assert_eq!(prefix, vec![user_msg("q1"), assistant_msg("a1")]);
    }

    #[test]
    fn branch_nonexistent_returns_false() {
        let mut tree = SessionTree::new();
        assert!(!tree.branch(&EntryId::from("nope")));
    }

    #[test]
    fn tree_structure() {
        let mut tree = SessionTree::new();
        let u1 = tree.append_message(user_msg("root"));
        tree.append_message(assistant_msg("branch 1"));

        tree.branch(&u1);
        tree.append_message(assistant_msg("branch 2"));

        let nodes = tree.tree();
        assert_eq!(nodes.len(), 1); // one root
        assert_eq!(nodes[0].children.len(), 2); // two branches
    }

    #[test]
    fn user_messages_in_branch() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));
        tree.append_message(user_msg("q2"));
        tree.append_message(assistant_msg("a2"));

        let user_msgs = tree.user_messages_in_branch();
        assert_eq!(user_msgs.len(), 2);
    }

    #[test]
    fn serialisation_roundtrip() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("hello"));
        tree.append_message(assistant_msg("hi"));

        let json = serde_json::to_string(&tree).unwrap();
        let restored: SessionTree = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.build_context().len(), 2);
    }

    #[test]
    fn reset_leaf() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("first"));
        assert!(tree.leaf_id().is_some());

        tree.reset_leaf();
        assert!(tree.leaf_id().is_none());

        // next append creates a new root
        tree.append_message(user_msg("second root"));
        let roots = tree.roots();
        assert_eq!(roots.len(), 2);
    }
}
