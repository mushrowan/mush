use derive_more::Display;
use mush_ai::types::*;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub timestamp: Timestamp,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    Message {
        message: Message,
    },
    BranchSummary {
        summary: String,
        from_id: EntryId,
    },
    Compaction {
        summary: String,
        first_kept_id: EntryId,
    },
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub entry: SessionEntry,
    pub children: Vec<TreeNode>,
}

#[derive(Debug, Clone)]
pub struct TreeRow<'a> {
    pub entry: &'a SessionEntry,
    pub depth: usize,
    pub on_branch: bool,
    pub is_leaf: bool,
    pub is_fork: bool,
}

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

    pub fn branch(&mut self, from_id: &EntryId) -> bool {
        if self.by_id(from_id).is_some() {
            self.leaf_id = Some(from_id.clone());
            true
        } else {
            false
        }
    }

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

    pub fn compact_at_leaf(
        &mut self,
        summary: String,
        first_kept_context_index: usize,
    ) -> Option<EntryId> {
        let first_kept_id = {
            let context = self.context_entries();
            let entry = context.get(first_kept_context_index)?;
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

    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    pub fn leaf_id(&self) -> Option<&EntryId> {
        self.leaf_id.as_ref()
    }

    pub fn current_branch(&self) -> Vec<&SessionEntry> {
        self.branch_ending_at(self.leaf_id.as_ref())
    }

    fn branch_ending_at(&self, id: Option<&EntryId>) -> Vec<&SessionEntry> {
        let index = self.build_index();
        let mut path = Vec::new();
        let mut current = id;
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

    pub fn entries_matching_prefix(&self, prefix: &str) -> Vec<&SessionEntry> {
        let prefix = prefix.to_ascii_lowercase();
        self.entries
            .iter()
            .filter(|entry| entry.id.starts_with(&prefix))
            .collect()
    }

    pub fn unanswered_tool_calls(&self, id: &EntryId) -> Vec<ToolCallId> {
        let mut open: Vec<ToolCallId> = Vec::new();
        for entry in self.branch_ending_at(Some(id)) {
            let EntryKind::Message { message } = &entry.kind else {
                continue;
            };
            match message {
                Message::Assistant(assistant) => {
                    open.extend(assistant.content.iter().filter_map(|part| match part {
                        AssistantContentPart::ToolCall(call) => Some(call.id.clone()),
                        _ => None,
                    }));
                }
                Message::ToolResult(result) => open.retain(|call| call != &result.tool_call_id),
                Message::User(_) => {}
            }
        }
        open
    }

    pub fn nearest_branchable_ancestor(&self, id: &EntryId) -> Option<&EntryId> {
        let index = self.build_index();
        let mut current = index.get(&**id)?.parent_id.as_ref();
        while let Some(candidate) = current {
            let entry: &SessionEntry = index.get(&**candidate)?;
            if self.unanswered_tool_calls(&entry.id).is_empty() {
                return Some(&entry.id);
            }
            current = entry.parent_id.as_ref();
        }
        None
    }

    pub fn rows(&self) -> Vec<TreeRow<'_>> {
        let on_branch: std::collections::HashSet<&str> =
            self.current_branch().into_iter().map(|e| &*e.id).collect();

        let mut children: std::collections::HashMap<&str, Vec<&SessionEntry>> =
            std::collections::HashMap::new();
        let mut roots: Vec<&SessionEntry> = Vec::new();
        for entry in &self.entries {
            match &entry.parent_id {
                Some(parent) => children.entry(&**parent).or_default().push(entry),
                None => roots.push(entry),
            }
        }

        let mut rows = Vec::with_capacity(self.entries.len());
        let mut stack: Vec<(&SessionEntry, usize)> =
            roots.into_iter().rev().map(|entry| (entry, 0)).collect();
        while let Some((entry, depth)) = stack.pop() {
            let kids = children
                .get(&*entry.id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            rows.push(TreeRow {
                entry,
                depth,
                on_branch: on_branch.contains(&*entry.id),
                is_leaf: self.leaf_id.as_deref() == Some(&*entry.id),
                is_fork: kids.len() > 1,
            });

            let main = kids
                .iter()
                .position(|kid| on_branch.contains(&*kid.id))
                .unwrap_or_else(|| kids.len().saturating_sub(1));
            if let Some(kid) = kids.get(main) {
                stack.push((kid, depth));
            }
            for (index, kid) in kids.iter().enumerate().rev() {
                if index != main {
                    stack.push((kid, depth + 1));
                }
            }
        }
        rows
    }

    pub fn build_context(&self) -> Vec<Message> {
        self.collect_context(None)
    }

    pub fn build_context_prefix(&self, limit: usize) -> Vec<Message> {
        self.collect_context(Some(limit))
    }

    pub fn build_display_history(&self) -> Vec<Message> {
        self.current_branch()
            .into_iter()
            .map(entry_to_message)
            .collect()
    }

    pub fn context_len(&self) -> usize {
        self.context_entries().len()
    }

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

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn by_id(&self, id: &EntryId) -> Option<&SessionEntry> {
        self.entries.iter().find(|e| e.id == *id)
    }

    pub fn children(&self, parent_id: &EntryId) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent_id.as_ref() == Some(parent_id))
            .collect()
    }

    pub fn roots(&self) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent_id.is_none())
            .collect()
    }

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

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

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

    fn assistant_calling(tool_call_id: &str) -> Message {
        Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::ToolCall(ToolCall {
                    id: tool_call_id.into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({}),
                })])
                .model("test".into())
                .provider(Provider::Custom("test".into()))
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn tool_result(tool_call_id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: tool_call_id.into(),
            tool_name: "bash".into(),
            content: vec![ToolResultContentPart::Text(TextContent {
                text: "done".into(),
            })],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn forked_tree() -> SessionTree {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q0"));
        let fork = tree.append_message(assistant_msg("a0"));
        tree.append_message(user_msg("q1"));
        tree.append_message(assistant_msg("a1"));
        tree.branch(&fork);
        tree.append_message(user_msg("q2"));
        tree.append_message(assistant_msg("a2"));
        tree
    }

    #[test]
    fn a_unique_prefix_resolves_to_one_entry() {
        let mut tree = SessionTree::new();
        let id = tree.append_message(user_msg("q0"));

        let found = tree.entries_matching_prefix(&id[..4]);

        assert_eq!(found.len(), 1, "one entry should match");
        assert_eq!(found[0].id, id);
    }

    #[test]
    fn an_ambiguous_prefix_reports_every_candidate() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q0"));
        tree.append_message(user_msg("q1"));

        let found = tree.entries_matching_prefix("");

        assert_eq!(found.len(), 2);
    }

    #[test]
    fn an_unknown_prefix_matches_nothing() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q0"));

        assert!(tree.entries_matching_prefix("zzzzzzzz").is_empty());
    }

    #[test]
    fn rows_keep_the_current_branch_flush_left() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q0"));
        let leaf = tree.append_message(assistant_msg("a0"));

        let rows = tree.rows();

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.depth == 0 && row.on_branch));
        assert!(rows.last().unwrap().is_leaf);
        assert!(rows.last().unwrap().entry.id == leaf);
        assert!(rows.iter().all(|row| !row.is_fork));
    }

    #[test]
    fn rows_indent_an_abandoned_branch_above_the_line_that_replaced_it() {
        let rows = forked_tree();
        let rows = rows.rows();

        let rendered: Vec<(usize, String, bool)> = rows
            .iter()
            .map(|row| {
                (
                    row.depth,
                    text_of(&entry_to_message(row.entry)),
                    row.on_branch,
                )
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                (0, "q0".into(), true),
                (0, "a0".into(), true),
                (1, "q1".into(), false),
                (1, "a1".into(), false),
                (0, "q2".into(), true),
                (0, "a2".into(), true),
            ]
        );
        assert!(
            rows[1].is_fork,
            "the entry the branches diverge from should be marked"
        );
    }

    #[test]
    fn every_entry_in_the_tree_gets_a_row() {
        let tree = forked_tree();
        assert_eq!(tree.rows().len(), tree.entries().len());
    }

    #[test]
    fn unanswered_tool_calls_names_the_calls_a_leaf_would_leave_open() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q0"));
        let calling = tree.append_message(assistant_calling("call_1"));
        let answered = tree.append_message(tool_result("call_1"));

        assert_eq!(
            tree.unanswered_tool_calls(&calling),
            vec![ToolCallId::from("call_1")],
            "branching off the assistant turn leaves its tool call unanswered"
        );
        assert!(
            tree.unanswered_tool_calls(&answered).is_empty(),
            "the tool result closes the call"
        );
    }

    #[test]
    fn the_nearest_branchable_ancestor_skips_entries_with_open_tool_calls() {
        let mut tree = SessionTree::new();
        let user = tree.append_message(user_msg("q0"));
        let calling = tree.append_message(assistant_calling("call_1"));

        assert_eq!(tree.nearest_branchable_ancestor(&calling), Some(&user));
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

        assert!(tree.branch(&u1));

        tree.append_message(assistant_msg("hey!"));
        tree.append_message(user_msg("tell me about python"));

        let branch = tree.current_branch();
        assert_eq!(branch.len(), 3);

        assert_eq!(tree.len(), 6);

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

        assert_eq!(tree.leaf_id(), Some(&summary_id));

        let ctx = tree.build_context();
        assert_eq!(ctx.len(), 2);
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
        assert_eq!(ctx.len(), 3);
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

        assert_eq!(tree.leaf_id(), Some(&compaction_id));

        assert_eq!(tree.entries().len(), entries_before + 1);

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
        if let Message::User(u) = &ctx[1] {
            if let UserContent::Text(t) = &u.content {
                assert_eq!(t.as_str(), "q3");
            }
        } else {
            panic!("expected q3 as first kept message");
        }

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
        let mut tree = SessionTree::new();
        for i in 0..20 {
            if i % 2 == 0 {
                tree.append_message(user_msg(&format!("q{i}")));
            } else {
                tree.append_message(assistant_msg(&format!("a{i}")));
            }
        }

        tree.compact_at_leaf("first summary".into(), 15)
            .expect("first compaction should succeed");
        assert_eq!(tree.build_context().len(), 6, "summary + 5 kept");

        for i in 20..30 {
            if i % 2 == 0 {
                tree.append_message(user_msg(&format!("q{i}")));
            } else {
                tree.append_message(assistant_msg(&format!("a{i}")));
            }
        }
        assert_eq!(tree.build_context().len(), 16);

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
        assert!(
            texts.iter().any(|t| t == "q24"),
            "kept window start q24 missing: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "a29"),
            "kept window end a29 missing: {texts:?}"
        );
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

        assert_eq!(tree.build_context().len(), 3);

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
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].children.len(), 2);
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

        tree.append_message(user_msg("second root"));
        let roots = tree.roots();
        assert_eq!(roots.len(), 2);
    }
}
