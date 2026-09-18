use std::cell::RefCell;

use crate::fuzzy::FuzzyFilter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePickerPurpose {
    Fork,
    Branch,
}

impl MessagePickerPurpose {
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Fork => " fork from message ",
            Self::Branch => " branch from message ",
        }
    }

    #[must_use]
    pub const fn verb(self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Branch => "branch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagePickerItem {
    pub index: usize,
    pub text: String,
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
        let selected = items.len().saturating_sub(1);
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
        .map(|(i, text)| MessagePickerItem {
            index: i + 1,
            text: text.split_whitespace().collect::<Vec<_>>().join(" "),
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
