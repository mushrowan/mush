use std::cell::RefCell;

use crate::fuzzy::FuzzyFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePickerPurpose {
    Fork,
    Branch,
    Tree,
}

impl MessagePickerPurpose {
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Fork => " fork from message ",
            Self::Branch => " branch from message ",
            Self::Tree => " conversation tree ",
        }
    }

    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Branch | Self::Tree => "branch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePickerKind {
    User,
    BranchSummary,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagePickerItem {
    pub index: usize,
    pub text: String,
    pub kind: MessagePickerKind,
    pub depth: usize,
    pub on_branch: bool,
    pub fork: bool,
    pub entry_id: Option<mush_session::tree::EntryId>,
}

impl MessagePickerItem {
    #[must_use]
    pub fn numbered(index: usize, text: impl Into<String>) -> Self {
        Self {
            index,
            text: one_line(&text.into()),
            kind: MessagePickerKind::User,
            depth: 0,
            on_branch: true,
            fork: false,
            entry_id: None,
        }
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug)]
pub struct MessagePickerState {
    pub purpose: MessagePickerPurpose,
    pub items: Vec<MessagePickerItem>,
    pub selected: usize,
    pub filter: String,
    matcher: RefCell<FuzzyFilter>,
}

impl MessagePickerState {
    #[must_use]
    pub fn new(purpose: MessagePickerPurpose, items: Vec<MessagePickerItem>) -> Self {
        let selected = items
            .iter()
            .rposition(|item| item.on_branch)
            .unwrap_or(items.len().saturating_sub(1));
        Self {
            purpose,
            items,
            selected,
            filter: String::new(),
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    #[must_use]
    pub fn selected_item(&self) -> Option<&MessagePickerItem> {
        filtered_items(self).get(self.selected).copied()
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_next(&mut self) {
        if self.selected + 1 < filtered_items(self).len() {
            self.selected += 1;
        }
    }

    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
    }
}

#[must_use]
pub fn filtered_items(picker: &MessagePickerState) -> Vec<&MessagePickerItem> {
    if picker.filter.is_empty() {
        return picker.items.iter().collect();
    }
    let mut matcher = picker.matcher.borrow_mut();
    let indices = matcher.filter(&picker.items, &picker.filter, |item| item.text.as_str());
    indices.into_iter().map(|i| &picker.items[i]).collect()
}

#[must_use]
pub fn user_message_items(messages: &[mush_ai::types::Message]) -> Vec<MessagePickerItem> {
    messages
        .iter()
        .filter_map(|message| match message {
            mush_ai::types::Message::User(user) => Some(user.text()),
            _ => None,
        })
        .enumerate()
        .map(|(i, text)| MessagePickerItem::numbered(i + 1, text))
        .collect()
}

#[must_use]
pub fn tree_items(tree: &mush_session::tree::SessionTree) -> Vec<MessagePickerItem> {
    use mush_session::tree::EntryKind;
    let mut index = 0;
    tree.rows()
        .into_iter()
        .filter_map(|row| {
            let (kind, text) = match &row.entry.kind {
                EntryKind::Message {
                    message: mush_ai::types::Message::User(user),
                } => (MessagePickerKind::User, user.text()),
                EntryKind::BranchSummary { summary, .. } => {
                    (MessagePickerKind::BranchSummary, summary.clone())
                }
                EntryKind::Compaction { summary, .. } => {
                    (MessagePickerKind::Compaction, summary.clone())
                }
                EntryKind::Message { .. } => return None,
            };
            index += 1;
            Some(MessagePickerItem {
                index,
                text: one_line(&text),
                kind,
                depth: row.depth,
                on_branch: row.on_branch,
                fork: row.is_fork,
                entry_id: Some(row.entry.id.clone()),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Message, Timestamp, UserContent, UserMessage};

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn picker() -> MessagePickerState {
        MessagePickerState::new(
            MessagePickerPurpose::Fork,
            user_message_items(&[user("fix the\n  build"), user("add tests"), user("ship it")]),
        )
    }

    #[test]
    fn items_are_numbered_from_one_and_flattened_to_a_line() {
        let picker = picker();
        assert_eq!(picker.items[0].index, 1);
        assert_eq!(picker.items[0].text, "fix the build");
        assert_eq!(picker.items[2].index, 3);
    }

    #[test]
    fn the_newest_message_starts_selected() {
        let picker = picker();
        assert_eq!(picker.selected_item().unwrap().index, 3);
    }

    #[test]
    fn filtering_keeps_the_original_index() {
        let mut picker = picker();
        for c in "tests".chars() {
            picker.push_filter(c);
        }
        let shown = filtered_items(&picker);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].index, 2);
        assert_eq!(picker.selected_item().unwrap().index, 2);
    }

    #[test]
    fn tree_items_keep_the_shape_and_start_on_the_live_branch() {
        use mush_session::tree::SessionTree;
        let mut tree = SessionTree::new();
        let q0 = tree.append_message(user("q0"));
        tree.append_message(user("q1 abandoned"));
        tree.branch(&q0);
        tree.append_message(user("q2"));

        let items = tree_items(&tree);
        let shape: Vec<(usize, &str, usize, bool)> = items
            .iter()
            .map(|item| (item.index, item.text.as_str(), item.depth, item.on_branch))
            .collect();
        assert_eq!(
            shape,
            vec![
                (1, "q0", 0, true),
                (2, "q1 abandoned", 1, false),
                (3, "q2", 0, true),
            ]
        );
        assert!(items[0].fork);
        assert!(items.iter().all(|item| item.entry_id.is_some()));

        let picker = MessagePickerState::new(MessagePickerPurpose::Tree, items);
        assert_eq!(picker.selected_item().unwrap().text, "q2");
    }

    #[test]
    fn navigation_stops_at_both_ends() {
        let mut picker = picker();
        picker.select_next();
        assert_eq!(picker.selected, 2);
        picker.select_previous();
        picker.select_previous();
        picker.select_previous();
        assert_eq!(picker.selected, 0);
    }
}
