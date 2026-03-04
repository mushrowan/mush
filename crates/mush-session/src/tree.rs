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
pub struct EntryId(pub String);

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
    pub fn branch_with_summary(&mut self, from_id: &EntryId, summary: String) -> Option<EntryId> {
        self.by_id(from_id)?;
        let old_leaf = self.leaf_id.clone().unwrap_or_else(|| from_id.clone());
        self.leaf_id = Some(from_id.clone());

        let entry = SessionEntry {
            id: EntryId::new(),
            parent_id: Some(from_id.clone()),
            timestamp: Timestamp::now(),
            kind: EntryKind::BranchSummary {
                summary,
                from_id: old_leaf,
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
        let branch = self.current_branch();
        let mut messages = Vec::new();

        // find latest compaction in the path
        let compaction_idx = branch
            .iter()
            .rposition(|e| matches!(e.kind, EntryKind::Compaction { .. }));

        let start = if let Some(idx) = compaction_idx {
            if let EntryKind::Compaction {
                ref summary,
                ref first_kept_id,
            } = branch[idx].kind
            {
                // emit compaction summary as user message
                messages.push(Message::User(UserMessage {
                    content: UserContent::Text(format!(
                        "The conversation history before this point was compacted \
                         into the following summary:\n\n<summary>\n{summary}\n</summary>"
                    )),
                    timestamp_ms: branch[idx].timestamp,
                }));

                // find the first kept entry and emit from there to compaction
                if let Some(kept_pos) = branch.iter().position(|e| e.id == *first_kept_id) {
                    for entry in &branch[kept_pos..idx] {
                        if let EntryKind::Message { ref message } = entry.kind {
                            messages.push(message.clone());
                        }
                    }
                }
            }
            idx + 1
        } else {
            0
        };

        // emit entries after compaction (or all entries if no compaction)
        for entry in &branch[start..] {
            match &entry.kind {
                EntryKind::Message { message } => messages.push(message.clone()),
                EntryKind::BranchSummary { summary, .. } => {
                    messages.push(Message::User(UserMessage {
                        content: UserContent::Text(format!(
                            "Context from a previous conversation branch:\n\n\
                             <branch_summary>\n{summary}\n</branch_summary>"
                        )),
                        timestamp_ms: entry.timestamp,
                    }));
                }
                EntryKind::Compaction { .. } => {} // already handled above
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
            timestamp_ms: Timestamp(0),
        })
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContentPart::Text(TextContent {
                text: text.into(),
            })],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::AnthropicMessages,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp(0),
        })
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
            .branch_with_summary(&u1, "discussed topic A briefly".into())
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
    fn branch_nonexistent_returns_false() {
        let mut tree = SessionTree::new();
        assert!(!tree.branch(&EntryId("nope".into())));
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
