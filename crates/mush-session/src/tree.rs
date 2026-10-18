use derive_more::Display;
use mush_ai::types::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::harness::{RefinementResult, RefinementSource};
use crate::user_text::user_text;

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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        event_receipts: Vec<EventReceipt>,
    },
    Compaction {
        summary: String,
        first_kept_id: EntryId,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        event_receipts: Vec<EventReceipt>,
    },
    Refinement {
        source: RefinementSource,
        result: RefinementResult,
    },
    ThinkingLevelChange {
        thinking_level: ThinkingLevel,
    },
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecentHistory {
    pub messages: Vec<Message>,
    pub omitted: usize,
    pub usage: Usage,
    pub cost: Dollars,
}

const PREVIEW: usize = 400;

fn clip(text: &str, width: usize) -> String {
    match text.char_indices().nth(width) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryKind {
    User,
    BranchSummary,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSummaryRow {
    pub id: EntryId,
    pub kind: SummaryKind,
    pub preview: String,
    pub depth: usize,
    pub on_branch: bool,
    pub is_fork: bool,
    pub is_leaf: bool,
}

#[derive(Debug, Clone)]
pub struct TreeRow<'a> {
    pub entry: &'a SessionEntry,
    pub depth: usize,
    pub on_branch: bool,
    pub is_leaf: bool,
    pub is_fork: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTree {
    entries: Vec<Arc<SessionEntry>>,
    leaf_id: Option<EntryId>,
    #[serde(skip)]
    index: Arc<Index>,
}

#[derive(Debug, Clone, Default)]
struct Index {
    positions: std::collections::HashMap<EntryId, usize>,
    parents: Vec<Option<usize>>,
    child_counts: Vec<usize>,
    open_tool_calls: Vec<Arc<[ToolCallId]>>,
}

impl SessionTree {
    pub fn rebuild(entries: Vec<SessionEntry>, leaf_id: Option<EntryId>) -> Result<Self, String> {
        let mut tree = Self::new();
        tree.leaf_id = leaf_id;
        if entries.is_empty() {
            return Ok(tree);
        }
        tree.entries.reserve(entries.len());
        for entry in entries {
            if entry
                .parent_id
                .as_ref()
                .is_some_and(|parent| !tree.index.positions.contains_key(parent))
            {
                return Err(String::from(
                    "session history parent missing or out of order",
                ));
            }
            match &entry.kind {
                EntryKind::BranchSummary { from_id, .. }
                    if !tree.index.positions.contains_key(from_id) =>
                {
                    return Err(String::from("branch summary history missing"));
                }
                EntryKind::Compaction { first_kept_id, .. }
                    if !tree.index.positions.contains_key(first_kept_id) =>
                {
                    return Err(String::from("compaction history missing"));
                }
                _ => {}
            }
            if tree.index.positions.contains_key(&entry.id) {
                return Err(String::from("duplicate session history id"));
            }
            tree.push(entry);
        }
        if tree
            .leaf_id
            .as_ref()
            .is_some_and(|leaf| !tree.index.positions.contains_key(leaf))
        {
            return Err(String::from("session history leaf missing"));
        }
        for index in 0..tree.entries.len() {
            let receipts = match &tree.entries[index].kind {
                EntryKind::BranchSummary { from_id, .. } => {
                    branch_receipts(tree.branch_ending_at(Some(from_id)))
                }
                EntryKind::Compaction { first_kept_id, .. } => {
                    let branch = tree.branch_ending_at(tree.entries[index].parent_id.as_ref());
                    let Some(kept) = branch.iter().position(|entry| entry.id == *first_kept_id)
                    else {
                        return Err(String::from("compaction boundary outside history"));
                    };
                    branch_receipts(branch[..kept].iter().copied())
                }
                EntryKind::Message {
                    message: Message::Event(event),
                } if event.kind.is_summary() => {
                    let receipts = branch_receipts(
                        tree.branch_ending_at(tree.entries[index].parent_id.as_ref()),
                    );
                    if event
                        .receipts
                        .iter()
                        .any(|receipt| !receipts.contains(receipt))
                    {
                        return Err(String::from(
                            "event summary receipt has no matching history",
                        ));
                    }
                    continue;
                }
                _ => continue,
            };
            match &mut Arc::make_mut(&mut tree.entries[index]).kind {
                EntryKind::BranchSummary { event_receipts, .. }
                | EntryKind::Compaction { event_receipts, .. } => {
                    if receipts.is_empty() && !event_receipts.is_empty() {
                        return Err(String::from(
                            "summary receipts have no retained event history",
                        ));
                    }
                    *event_receipts = receipts;
                }
                EntryKind::Message { .. }
                | EntryKind::Refinement { .. }
                | EntryKind::ThinkingLevelChange { .. }
                | EntryKind::Unknown(_) => unreachable!(),
            }
        }
        Ok(tree)
    }
}

impl<'de> Deserialize<'de> for SessionTree {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(default)]
            entries: Vec<SessionEntry>,
            leaf_id: Option<EntryId>,
        }
        let stored = Stored::deserialize(deserializer)?;
        Self::rebuild(stored.entries, stored.leaf_id).map_err(serde::de::Error::custom)
    }
}

fn summary_receipts(messages: &[Message]) -> Vec<EventReceipt> {
    EventMessage::summary(String::new(), Timestamp::zero(), messages).receipts
}

fn branch_receipts<'a>(entries: impl IntoIterator<Item = &'a SessionEntry>) -> Vec<EventReceipt> {
    let mut receipts: Vec<EventReceipt> = Vec::new();
    for receipt in entries.into_iter().flat_map(entry_receipts) {
        if !receipts.contains(receipt) {
            receipts.push(receipt.clone());
        }
    }
    receipts
}

fn open_tool_calls_after(
    open: Option<&Arc<[ToolCallId]>>,
    entry: &SessionEntry,
) -> Arc<[ToolCallId]> {
    let open = open.cloned().unwrap_or_default();
    let EntryKind::Message { message } = &entry.kind else {
        return open;
    };
    match message {
        Message::Assistant(assistant) => {
            let mut calls = assistant
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantContentPart::ToolCall(call) => Some(&call.id),
                    _ => None,
                })
                .peekable();
            if calls.peek().is_none() {
                return open;
            }
            open.iter().chain(calls).cloned().collect()
        }
        Message::ToolResult(result) if open.contains(&result.tool_call_id) => open
            .iter()
            .filter(|call| **call != result.tool_call_id)
            .cloned()
            .collect(),
        Message::ToolResult(_) | Message::User(_) | Message::Event(_) => open,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeftBehind {
    pub entries: usize,
    pub last: Option<EntryId>,
    pub events: Vec<EventMessage>,
}

fn carried_event(entry: &SessionEntry) -> Option<EventMessage> {
    let event = match &entry.kind {
        EntryKind::Message {
            message: Message::Event(event),
        } => event.clone(),
        EntryKind::Message {
            message: Message::User(user),
        } => EventMessage::from_text(&user.text())?,
        EntryKind::Message { .. }
        | EntryKind::BranchSummary { .. }
        | EntryKind::Compaction { .. }
        | EntryKind::Refinement { .. }
        | EntryKind::ThinkingLevelChange { .. }
        | EntryKind::Unknown(_) => return None,
    };
    (event.kind.is_carried() && !event.receipts.is_empty()).then_some(event)
}

fn delivered_receipts(entries: &[&SessionEntry]) -> Vec<PromptId> {
    entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message {
                message: Message::Event(event),
            } if !event.kind.is_summary() => Some(event.receipts.clone()),
            EntryKind::Message {
                message: Message::User(user),
            } => EventMessage::from_text(&user.text()).map(|event| event.receipts),
            EntryKind::Message { .. }
            | EntryKind::BranchSummary { .. }
            | EntryKind::Compaction { .. }
            | EntryKind::Refinement { .. }
            | EntryKind::ThinkingLevelChange { .. }
            | EntryKind::Unknown(_) => None,
        })
        .flatten()
        .map(|receipt| receipt.id)
        .collect()
}

fn receipt_ids(receipts: &[EventReceipt]) -> Vec<PromptId> {
    receipts.iter().map(|receipt| receipt.id.clone()).collect()
}

fn fresh(event: &EventMessage, seen: &mut Vec<PromptId>) -> bool {
    if event
        .receipts
        .iter()
        .any(|receipt| seen.contains(&receipt.id))
    {
        return false;
    }
    seen.extend(event.receipts.iter().map(|receipt| receipt.id.clone()));
    true
}

fn entry_receipts(entry: &SessionEntry) -> &[EventReceipt] {
    match &entry.kind {
        EntryKind::Message {
            message: Message::Event(event),
        } => &event.receipts,
        EntryKind::BranchSummary { event_receipts, .. }
        | EntryKind::Compaction { event_receipts, .. } => event_receipts,
        EntryKind::Message { .. }
        | EntryKind::Refinement { .. }
        | EntryKind::ThinkingLevelChange { .. }
        | EntryKind::Unknown(_) => &[],
    }
}

fn is_digest(entry: &SessionEntry) -> bool {
    matches!(
        &entry.kind,
        EntryKind::Message {
            message: Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }),
        } if text.starts_with(crate::harness::DIGEST_PREFIX)
    )
}

fn entry_to_message(entry: &SessionEntry) -> Option<Message> {
    Some(match &entry.kind {
        EntryKind::Message { message } => message.clone(),
        EntryKind::BranchSummary {
            summary,
            event_receipts,
            ..
        } if !event_receipts.is_empty() => Message::Event(EventMessage {
            content: summary.clone(),
            timestamp_ms: entry.timestamp,
            kind: EventKind::BranchSummary,
            receipts: event_receipts.clone(),
        }),
        EntryKind::Compaction {
            summary,
            event_receipts,
            ..
        } if !event_receipts.is_empty() => Message::Event(EventMessage {
            content: summary.clone(),
            timestamp_ms: entry.timestamp,
            kind: EventKind::Summary,
            receipts: event_receipts.clone(),
        }),
        EntryKind::BranchSummary { summary, .. } => {
            user_text(branch_summary_text(summary), entry.timestamp)
        }
        EntryKind::Compaction { summary, .. } => {
            user_text(compaction_summary_text(summary), entry.timestamp)
        }
        EntryKind::Refinement { source, result } => {
            user_text(result.notice_message(*source), entry.timestamp)
        }
        EntryKind::ThinkingLevelChange { .. } | EntryKind::Unknown(_) => return None,
    })
}

fn is_applied_refinement(entry: &SessionEntry) -> bool {
    matches!(&entry.kind, EntryKind::Refinement { result, .. } if result.applied_any())
}

impl SessionTree {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            leaf_id: None,
            index: Arc::default(),
        }
    }

    fn push(&mut self, entry: SessionEntry) {
        self.push_shared(Arc::new(entry));
    }

    fn push_shared(&mut self, entry: Arc<SessionEntry>) {
        let index = Arc::make_mut(&mut self.index);
        let parent = entry
            .parent_id
            .as_ref()
            .and_then(|parent| index.positions.get(parent).copied());
        if let Some(parent) = parent {
            index.child_counts[parent] += 1;
        }
        let open =
            open_tool_calls_after(parent.map(|parent| &index.open_tool_calls[parent]), &entry);
        index.positions.insert(entry.id.clone(), self.entries.len());
        index.parents.push(parent);
        index.child_counts.push(0);
        index.open_tool_calls.push(open);
        self.entries.push(entry);
    }

    #[must_use]
    pub fn carried_to(&self, leaf: Option<&EntryId>) -> Self {
        let mut carried = vec![false; self.entries.len()];
        let mut pending = vec![leaf];
        while let Some(id) = pending.pop() {
            for entry in self.branch_ending_at(id) {
                let position = self.index.positions[&entry.id];
                if std::mem::replace(&mut carried[position], true) {
                    continue;
                }
                if let EntryKind::BranchSummary { from_id, .. } = &entry.kind {
                    pending.push(Some(from_id));
                }
            }
        }
        let mut tree = Self::new();
        for (entry, carried) in self.entries.iter().zip(carried) {
            if carried {
                tree.push_shared(Arc::clone(entry));
            }
        }
        tree.leaf_id = leaf
            .filter(|leaf| tree.index.positions.contains_key(*leaf))
            .cloned();
        tree
    }

    #[must_use]
    pub fn forked_before(&self, context_index: usize) -> Option<Self> {
        let context = self.context_entries();
        let entry = context.get(context_index)?;
        let mut tree = self.carried_to(entry.parent_id.as_ref());
        if let Some(compaction) = context.first()
            && !tree.index.positions.contains_key(&compaction.id)
            && let EntryKind::Compaction {
                summary,
                first_kept_id,
                ..
            } = &compaction.kind
            && let Some(kept) = tree
                .context_entries()
                .iter()
                .position(|entry| entry.id == *first_kept_id)
            && kept > 0
            && tree.compact_at_leaf(summary.clone(), kept).is_some()
            && let Some(copy) = tree.entries.last_mut()
        {
            Arc::make_mut(copy).timestamp = compaction.timestamp;
        }
        Some(tree)
    }

    pub fn compact_matching(
        &mut self,
        summary: String,
        summarised_count: usize,
        kept: &[Message],
    ) -> Option<EntryId> {
        let first = kept.first()?;
        let context = self.build_context();
        let matches_at = |start: usize| {
            start > 0
                && context
                    .get(start..start + kept.len())
                    .is_some_and(|window| window.iter().zip(kept).all(same_message))
        };
        let start = if matches_at(summarised_count) {
            summarised_count
        } else {
            let candidates: Vec<_> = context
                .iter()
                .enumerate()
                .filter(|(start, message)| same_message((message, first)) && matches_at(*start))
                .map(|(start, _)| start)
                .collect();
            let [start] = candidates[..] else {
                return None;
            };
            start
        };
        self.compact_at_leaf(summary, start)
    }

    pub fn append_message(&mut self, message: Message) -> EntryId {
        self.append(EntryKind::Message { message })
    }

    pub fn append_refinement(
        &mut self,
        source: RefinementSource,
        result: RefinementResult,
    ) -> EntryId {
        self.append(EntryKind::Refinement { source, result })
    }

    fn append(&mut self, kind: EntryKind) -> EntryId {
        self.append_under(self.leaf_id.clone(), kind)
    }

    fn append_under(&mut self, parent_id: Option<EntryId>, kind: EntryKind) -> EntryId {
        let id = EntryId::new();
        self.push(SessionEntry {
            id: id.clone(),
            parent_id,
            timestamp: Timestamp::now(),
            kind,
        });
        self.leaf_id = Some(id.clone());
        id
    }

    pub fn append_thinking_level(&mut self, thinking_level: ThinkingLevel) -> EntryId {
        self.append(EntryKind::ThinkingLevelChange { thinking_level })
    }

    pub fn thinking_level(&self) -> Option<ThinkingLevel> {
        self.current_branch()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::ThinkingLevelChange { thinking_level } => Some(thinking_level.clone()),
                _ => None,
            })
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
        Some(self.summarised_branch(Some(from_id.clone()), summary))
    }

    pub fn reset_leaf_with_summary(&mut self, summary: impl Into<String>) -> EntryId {
        self.summarised_branch(None, summary)
    }

    fn summarised_branch(
        &mut self,
        from_id: Option<EntryId>,
        summary: impl Into<String>,
    ) -> EntryId {
        let old_leaf = self
            .leaf_id
            .clone()
            .or_else(|| from_id.clone())
            .unwrap_or_default();
        let event_receipts = summary_receipts(&self.build_context());
        self.append_under(
            from_id,
            EntryKind::BranchSummary {
                summary: summary.into(),
                from_id: old_leaf,
                event_receipts,
            },
        )
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
        let event_receipts = summary_receipts(&self.build_context_prefix(first_kept_context_index));
        Some(self.append(EntryKind::Compaction {
            summary,
            first_kept_id,
            event_receipts,
        }))
    }

    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
    }

    pub fn lift_out(&mut self, id: &EntryId) -> Option<SessionEntry> {
        let branch = self.current_branch();
        let at = branch.iter().position(|entry| entry.id == *id)?;
        let mut seen = delivered_receipts(&branch[..at]);
        let mut carried = Vec::new();
        for entry in &branch[at + 1..] {
            if let Some(event) = carried_event(entry) {
                if fresh(&event, &mut seen) {
                    carried.push(EntryKind::Message {
                        message: Message::Event(event),
                    });
                }
                continue;
            }
            match &entry.kind {
                EntryKind::Message {
                    message: Message::User(_) | Message::Event(_),
                }
                | EntryKind::Refinement { .. }
                | EntryKind::ThinkingLevelChange { .. } => carried.push(entry.kind.clone()),
                EntryKind::Message {
                    message: Message::Assistant(reply),
                } if !reply.is_replayable() => {}
                EntryKind::Message { .. }
                | EntryKind::BranchSummary { .. }
                | EntryKind::Compaction { .. }
                | EntryKind::Unknown(_) => return None,
            }
        }
        let lifted = branch[at].clone();
        self.leaf_id = lifted.parent_id.clone();
        for kind in carried {
            self.append(kind);
        }
        Some(lifted)
    }

    #[must_use]
    pub fn delivered_on_branch(&self, message: &Message) -> bool {
        let ids = match message {
            Message::Event(event) => receipt_ids(&event.receipts),
            Message::User(user) => match EventMessage::from_text(&user.text()) {
                Some(event) => receipt_ids(&event.receipts),
                None => {
                    return user.prompt_id.as_ref().is_some_and(|prompt| {
                        self.current_branch().into_iter().any(|entry| {
                            matches!(
                                &entry.kind,
                                EntryKind::Message {
                                    message: Message::User(held),
                                } if held.prompt_id.as_ref() == Some(prompt)
                            )
                        })
                    });
                }
            },
            Message::Assistant(_) | Message::ToolResult(_) => return false,
        };
        !ids.is_empty()
            && self.current_branch().into_iter().any(|entry| {
                let held = match &entry.kind {
                    EntryKind::Message {
                        message: Message::User(user),
                    } => EventMessage::from_text(&user.text())
                        .map(|event| receipt_ids(&event.receipts))
                        .unwrap_or_default(),
                    _ => receipt_ids(entry_receipts(entry)),
                };
                held.iter().any(|id| ids.contains(id))
            })
    }

    #[must_use]
    pub fn left_behind(&self, from: Option<&EntryId>, to: Option<&EntryId>) -> LeftBehind {
        let target = self.branch_ending_at(to);
        let staying: std::collections::HashSet<&str> =
            target.iter().map(|entry| &*entry.id).collect();
        let mut seen = delivered_receipts(&target);
        let mut left = LeftBehind::default();
        for entry in self.branch_ending_at(from) {
            if staying.contains(&*entry.id) {
                continue;
            }
            left.entries += 1;
            left.last = Some(entry.id.clone());
            if let Some(event) = carried_event(entry)
                && fresh(&event, &mut seen)
            {
                left.events.push(event);
            }
        }
        left
    }

    pub fn carry(&mut self, notice: EventMessage, events: Vec<EventMessage>) -> EntryId {
        let mut leaf = self.append_message(Message::Event(notice));
        for event in events {
            leaf = self.append_message(Message::Event(event));
        }
        leaf
    }

    pub fn leaf_id(&self) -> Option<&EntryId> {
        self.leaf_id.as_ref()
    }

    pub fn current_branch(&self) -> Vec<&SessionEntry> {
        self.branch_ending_at(self.leaf_id.as_ref())
    }

    pub fn abandoned_by_moving_to(&self, new_leaf: Option<&EntryId>) -> Vec<Message> {
        let staying: std::collections::HashSet<&str> = self
            .branch_ending_at(new_leaf)
            .into_iter()
            .map(|entry| &*entry.id)
            .collect();
        self.current_branch()
            .into_iter()
            .filter(|entry| !staying.contains(&*entry.id))
            .filter_map(entry_to_message)
            .collect()
    }

    fn branch_ending_at(&self, id: Option<&EntryId>) -> Vec<&SessionEntry> {
        let mut path = Vec::new();
        let mut current = id.and_then(|id| self.index.positions.get(id).copied());
        while let Some(position) = current {
            if path.len() == self.entries.len() {
                break;
            }
            path.push(&*self.entries[position]);
            current = self.index.parents[position];
        }
        path.reverse();
        path
    }

    pub fn unanswered_tool_calls(&self, id: &EntryId) -> Vec<ToolCallId> {
        self.index
            .positions
            .get(id)
            .map(|&position| self.index.open_tool_calls[position].to_vec())
            .unwrap_or_default()
    }

    pub fn nearest_branchable_ancestor(&self, id: &EntryId) -> Option<&EntryId> {
        let mut current = self.by_id(id)?.parent_id.as_ref();
        while let Some(candidate) = current {
            let entry = self.by_id(candidate)?;
            if self.unanswered_tool_calls(&entry.id).is_empty() {
                return Some(&entry.id);
            }
            current = entry.parent_id.as_ref();
        }
        None
    }

    #[must_use]
    pub fn summary(&self) -> Vec<TreeSummaryRow> {
        self.rows()
            .into_iter()
            .filter_map(|row| {
                let (kind, text) = match &row.entry.kind {
                    EntryKind::Message {
                        message: Message::User(user),
                    } => (SummaryKind::User, user.text()),
                    EntryKind::BranchSummary { summary, .. } => {
                        (SummaryKind::BranchSummary, summary.clone())
                    }
                    EntryKind::Compaction { summary, .. } => {
                        (SummaryKind::Compaction, summary.clone())
                    }
                    EntryKind::Message { .. }
                    | EntryKind::Refinement { .. }
                    | EntryKind::ThinkingLevelChange { .. }
                    | EntryKind::Unknown(_) => return None,
                };
                Some(TreeSummaryRow {
                    id: row.entry.id.clone(),
                    kind,
                    preview: clip(&text, PREVIEW),
                    depth: row.depth,
                    on_branch: row.on_branch,
                    is_fork: row.is_fork,
                    is_leaf: row.is_leaf,
                })
            })
            .collect()
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
        self.collect_context(usize::MAX)
    }

    pub fn build_context_prefix(&self, limit: usize) -> Vec<Message> {
        self.collect_context(limit)
    }

    pub fn build_display_history(&self) -> Vec<Message> {
        self.current_branch()
            .into_iter()
            .filter_map(entry_to_message)
            .collect()
    }

    #[must_use]
    pub fn recent_display_history(&self, limit: usize) -> RecentHistory {
        let branch = self.current_branch();
        let (usage, cost) = mush_ai::models::conversation_spend(branch.iter().filter_map(
            |entry| match &entry.kind {
                EntryKind::Message {
                    message: Message::Assistant(assistant),
                } => Some(assistant),
                _ => None,
            },
        ));
        let mut start = branch.len().saturating_sub(limit);
        if start > 0 {
            start = branch[start..]
                .iter()
                .position(|entry| {
                    matches!(
                        entry.kind,
                        EntryKind::Message {
                            message: Message::User(_)
                        }
                    )
                })
                .map_or(start, |offset| start + offset);
        }
        let omitted = branch[..start]
            .iter()
            .filter(|entry| {
                !matches!(
                    entry.kind,
                    EntryKind::ThinkingLevelChange { .. } | EntryKind::Unknown(_)
                )
            })
            .count();
        RecentHistory {
            messages: branch[start..]
                .iter()
                .copied()
                .filter_map(entry_to_message)
                .collect(),
            omitted,
            usage,
            cost,
        }
    }

    pub fn context_len(&self) -> usize {
        self.context_entries().len()
    }

    fn context_entries(&self) -> Vec<&SessionEntry> {
        let leaf = self
            .leaf_id
            .as_ref()
            .and_then(|id| self.index.positions.get(id).copied());
        let mut ancestors = std::iter::successors(leaf, |&position| self.index.parents[position])
            .take(self.entries.len())
            .map(|position| &*self.entries[position]);
        let mut since_compaction = Vec::new();
        let compaction = ancestors.by_ref().find(|entry| {
            let found = matches!(entry.kind, EntryKind::Compaction { .. });
            if !found {
                since_compaction.push(*entry);
            }
            found
        });
        let mut entries: Vec<&SessionEntry> = Vec::with_capacity(since_compaction.len() + 1);

        if let Some(compaction) = compaction {
            entries.push(compaction);
            if let EntryKind::Compaction { first_kept_id, .. } = &compaction.kind {
                let mut kept = Vec::new();
                if ancestors.any(|entry| {
                    kept.push(entry);
                    entry.id == *first_kept_id
                }) {
                    for &entry in kept.iter().rev() {
                        if matches!(entry.kind, EntryKind::Message { .. })
                            || is_applied_refinement(entry)
                        {
                            entries.push(entry);
                        }
                    }
                }
            }
        }

        for &entry in since_compaction.iter().rev() {
            if matches!(
                entry.kind,
                EntryKind::Message { .. } | EntryKind::BranchSummary { .. }
            ) || is_applied_refinement(entry)
            {
                entries.push(entry);
            }
        }

        if let Some(newest) = entries.iter().rposition(|entry| is_digest(entry)) {
            let newest = entries[newest].id.clone();
            entries.retain(|entry| entry.id == newest || !is_digest(entry));
        }
        entries
    }

    fn collect_context(&self, limit: usize) -> Vec<Message> {
        let entries = self.context_entries();
        let mut messages = Vec::with_capacity(limit.min(entries.len()));
        messages.extend(entries.into_iter().filter_map(entry_to_message).take(limit));
        messages
    }

    pub fn entries(&self) -> &[Arc<SessionEntry>] {
        &self.entries
    }

    pub fn by_id(&self, id: &EntryId) -> Option<&SessionEntry> {
        self.index
            .positions
            .get(id)
            .map(|&position| &*self.entries[position])
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
        self.index
            .positions
            .get(id)
            .is_some_and(|&position| self.index.child_counts[position] > 1)
    }
}

impl Default for SessionTree {
    fn default() -> Self {
        Self::new()
    }
}

fn same_message((stored, compacted): (&Message, &Message)) -> bool {
    match (stored, compacted) {
        (Message::ToolResult(stored), Message::ToolResult(compacted)) => {
            let ToolResultMessage {
                tool_call_id,
                tool_name,
                content,
                outcome,
                timestamp_ms: _,
                details,
            } = compacted;
            stored.tool_call_id == *tool_call_id
                && stored.tool_name == *tool_name
                && stored.content == *content
                && stored.outcome == *outcome
                && stored.details == *details
        }
        _ => stored == compacted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
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
            details: None,
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
    fn only_the_newest_digest_reaches_the_context_and_every_copy_stays_saved() {
        let digest =
            |version: &str| user_msg(&format!("{}\n\n{version}", crate::harness::DIGEST_PREFIX));
        let mut tree = SessionTree::new();
        tree.append_message(digest("v1"));
        tree.append_message(user_msg("q0"));
        tree.append_message(assistant_msg("a0"));
        tree.append_message(digest("v2"));
        tree.append_message(user_msg("q1"));

        let context: Vec<String> = tree
            .build_context()
            .iter()
            .map(|message| match message {
                Message::User(user) => user.text(),
                Message::Assistant(assistant) => assistant.text(),
                _ => String::new(),
            })
            .collect();

        assert_eq!(
            context,
            [
                "q0".to_string(),
                "a0".into(),
                format!("{}\n\nv2", crate::harness::DIGEST_PREFIX),
                "q1".into(),
            ]
        );
        assert_eq!(tree.context_len(), 4);
        assert_eq!(tree.entries().len(), 5);
    }

    fn refinement(content: &str) -> RefinementResult {
        crate::harness::State::default().apply(
            &crate::harness::Proposal {
                summary: "remember the port".into(),
                rationale: String::new(),
                expected_outcome: String::new(),
                edits: vec![crate::harness::Edit {
                    action: crate::harness::Action::Create,
                    kind: crate::harness::Kind::Memory,
                    id: None,
                    title: Some("port".into()),
                    content: Some(content.into()),
                    path: None,
                    reference: None,
                    arguments: None,
                    metadata: None,
                    reason: None,
                }],
            },
            "ref_1",
            crate::harness::Scope::Local,
            None,
        )
    }

    #[test]
    fn a_refinement_is_saved_and_reaches_the_context_as_a_notice_when_it_applied_edits() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q0"));
        tree.append_refinement(RefinementSource::Agent, refinement("8080"));
        let mut rejected = refinement("8080");
        rejected.applied_edits[0].applied = false;
        tree.append_refinement(RefinementSource::Auto, rejected);

        let context = tree.build_context();
        assert_eq!(context.len(), 2);
        let Message::User(notice) = &context[1] else {
            panic!("{context:?}");
        };
        assert_eq!(
            notice.text(),
            "[self-refinement]\n\nthese harness edits are already applied, nothing to do\nsummary: remember the port\n- created memory [local:port] port: 8080"
        );
        assert!(crate::harness::is_refinement_notice(&notice.text()));

        let saved: SessionTree =
            serde_json::from_str(&serde_json::to_string(&tree).unwrap()).unwrap();
        let sources: Vec<_> = saved
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Refinement { source, result } => Some((*source, result.applied_any())),
                _ => None,
            })
            .collect();
        assert_eq!(
            sources,
            [
                (RefinementSource::Agent, true),
                (RefinementSource::Auto, false)
            ]
        );
        assert!(
            serde_json::to_string(&tree)
                .unwrap()
                .contains("\"source\":\"self\"")
        );
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
                    text_of(&entry_to_message(row.entry).unwrap()),
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
    fn moving_back_up_the_live_branch_abandons_what_follows_the_target() {
        let tree = forked_tree();
        let q0 = tree.entries()[0].id.clone();

        let abandoned: Vec<String> = tree
            .abandoned_by_moving_to(Some(&q0))
            .iter()
            .map(text_of)
            .collect();

        assert_eq!(abandoned, vec!["a0", "q2", "a2"]);
        assert!(tree.abandoned_by_moving_to(tree.leaf_id()).is_empty());
        assert_eq!(tree.abandoned_by_moving_to(None).len(), 4);
    }

    #[test]
    fn moving_onto_a_side_branch_abandons_only_past_the_fork() {
        let tree = forked_tree();
        let a1 = tree.entries()[3].id.clone();

        let abandoned: Vec<String> = tree
            .abandoned_by_moving_to(Some(&a1))
            .iter()
            .map(text_of)
            .collect();

        assert_eq!(abandoned, vec!["q2", "a2"]);
    }

    fn event(text: &str) -> Message {
        Message::Event(EventMessage {
            content: text.into(),
            timestamp_ms: Timestamp::zero(),
            kind: EventKind::Message,
            receipts: Vec::new(),
        })
    }

    #[test]
    fn lifting_out_a_message_carries_what_arrived_after_it_onto_the_branch_before_it() {
        let mut tree = SessionTree::new();
        tree.append_message(user_msg("q0"));
        let answer = tree.append_message(assistant_msg("a0"));
        let rejected = tree.append_message(user_msg("look at this"));
        tree.append_message(event("[agent-message from child:auth] found it"));
        let mut failed = assistant_msg("");
        if let Message::Assistant(reply) = &mut failed {
            reply.stop_reason = StopReason::Error;
        }
        tree.append_message(failed);
        tree.append_message(user_msg("oop"));

        let lifted = tree.lift_out(&rejected).unwrap();

        assert_eq!(lifted.id, rejected);
        let context: Vec<String> = tree
            .build_context()
            .iter()
            .map(|message| match message {
                Message::Event(event) => event.content.clone(),
                other => text_of(other),
            })
            .collect();
        assert_eq!(
            context,
            [
                "q0",
                "a0",
                "[agent-message from child:auth] found it",
                "oop"
            ]
        );
        let carried = tree.current_branch();
        assert_eq!(carried[2].parent_id.as_ref(), Some(&answer));
        assert!(
            tree.by_id(&rejected).is_some(),
            "the rejected message stays in the tree"
        );
    }

    #[test]
    fn what_is_left_behind_counts_every_entry_but_carries_only_new_events() {
        let mut tree = SessionTree::new();
        let fork = tree.append_message(user_msg("q0"));
        tree.append_message(event_with(
            "[agent-message from child:auth]\n\nold news",
            "r1",
        ));
        tree.append_message(user_msg("q1"));
        tree.append_message(event_with("[agent-message from child:auth]\n\nfresh", "r2"));
        let mut summary = assistant_msg("summarised");
        if let Message::Assistant(reply) = &mut summary {
            reply.stop_reason = StopReason::Stop;
        }
        let from = tree.append_message(summary);
        let target = tree.entries()[1].id.clone();
        assert!(tree.branch(&target));
        let to = tree.leaf_id().cloned();
        assert_ne!(to.as_ref(), Some(&fork));

        let left = tree.left_behind(Some(&from), to.as_ref());

        assert_eq!(left.entries, 3);
        assert_eq!(left.last, Some(from));
        let carried: Vec<&str> = left
            .events
            .iter()
            .map(|event| event.content.as_str())
            .collect();
        assert_eq!(carried, ["[agent-message from child:auth]\n\nfresh"]);
    }

    fn event_with(text: &str, receipt: &str) -> Message {
        Message::Event(EventMessage {
            content: text.into(),
            timestamp_ms: Timestamp::zero(),
            kind: EventKind::Message,
            receipts: vec![EventReceipt {
                id: receipt.into(),
                timestamp_ms: Timestamp::zero(),
                kind: EventKind::Message,
                origin: EventOrigin::Agent {
                    session_id: "s".into(),
                    name: "auth".into(),
                    relationship: "child".into(),
                },
            }],
        })
    }

    #[test]
    fn a_message_with_work_after_it_is_not_lifted_out() {
        let mut tree = SessionTree::new();
        let asked = tree.append_message(user_msg("q0"));
        tree.append_message(assistant_msg("a0"));
        let leaf = tree.leaf_id().cloned();

        assert!(tree.lift_out(&asked).is_none());
        assert_eq!(tree.leaf_id(), leaf.as_ref());
        assert!(tree.lift_out(&EntryId::from("missing")).is_none());
    }

    #[test]
    fn lifting_out_the_first_message_empties_the_branch() {
        let mut tree = SessionTree::new();
        let only = tree.append_message(user_msg("hello"));

        assert!(tree.lift_out(&only).is_some());

        assert!(tree.leaf_id().is_none());
        assert!(tree.build_context().is_empty());
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
    fn unanswered_tool_calls_follow_only_the_asked_branch_and_survive_a_reload() {
        let mut tree = SessionTree::new();
        let user = tree.append_message(user_msg("q0"));
        let open = tree.append_message(assistant_calling("call_1"));
        assert!(tree.branch(&user));
        let other = tree.append_message(assistant_calling("call_2"));
        let answered = tree.append_message(tool_result("call_2"));
        let restored: SessionTree =
            serde_json::from_str(&serde_json::to_string(&tree).unwrap()).unwrap();

        for tree in [&tree, &restored] {
            assert_eq!(
                tree.unanswered_tool_calls(&open),
                vec![ToolCallId::from("call_1")]
            );
            assert_eq!(
                tree.unanswered_tool_calls(&other),
                vec![ToolCallId::from("call_2")]
            );
            assert!(tree.unanswered_tool_calls(&answered).is_empty());
            assert!(
                tree.unanswered_tool_calls(&EntryId::from("missing"))
                    .is_empty()
            );
        }
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
                UserContent::Text(t) => assert_eq!(
                    t,
                    "[branch-summary]\n\nThe following is a summary of a branch that this \
                     conversation came back from:\n\n<summary>\ndiscussed topic A briefly\n</summary>"
                ),
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

        assert!(tree.is_branch_point(&u1));
        let roots = tree.entries().iter().filter(|e| e.parent_id.is_none());
        assert_eq!(roots.count(), 1);
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
        let roots = tree.entries().iter().filter(|e| e.parent_id.is_none());
        assert_eq!(roots.count(), 2);
    }
}
