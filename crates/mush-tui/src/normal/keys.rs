use crossterm::event::{KeyCode, KeyEvent};

use crate::keybinds::{Action, KeyMap, KeyScope};
use crate::motion::{Motion, Word};

const SCROLLS: &[(Action, Scroll)] = &[
    (Action::LineUp, Scroll::LineUp),
    (Action::LineDown, Scroll::LineDown),
    (Action::HalfPageUp, Scroll::HalfPageUp),
    (Action::HalfPageDown, Scroll::HalfPageDown),
    (Action::PageUp, Scroll::PageUp),
    (Action::PageDown, Scroll::PageDown),
];

const MOTIONS: &[(Action, Motion)] = &[
    (Action::CursorLineStart, Motion::LineStart),
    (Action::CursorFirstNonBlank, Motion::FirstNonBlank),
    (Action::CursorLineEnd, Motion::LineEnd),
    (Action::CursorLeft, Motion::Left),
    (Action::CursorRight, Motion::Right),
    (Action::PreviousItem, Motion::Up),
    (Action::NextItem, Motion::Down),
    (Action::NavigateTop, Motion::Top),
    (Action::NavigateBottom, Motion::Bottom),
    (Action::CursorWordForward, Motion::WordForward(Word::Small)),
    (Action::CursorChunkForward, Motion::WordForward(Word::Big)),
    (Action::CursorWordBack, Motion::WordBack(Word::Small)),
    (Action::CursorChunkBack, Motion::WordBack(Word::Big)),
    (Action::CursorWordEnd, Motion::WordEnd(Word::Small)),
    (Action::CursorChunkEnd, Motion::WordEnd(Word::Big)),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scroll {
    LineUp,
    LineDown,
    HalfPageUp,
    HalfPageDown,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Move(Motion),
    Scroll(Scroll),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Pending,
    Run(Command, Option<usize>),
    Dropped,
}

#[derive(Debug, Default)]
pub(crate) struct Keys {
    count: Option<usize>,
}

impl Keys {
    pub(crate) fn feed(&mut self, keymap: &KeyMap, key: KeyEvent) -> Step {
        if self.take_digit(key) {
            return Step::Pending;
        }
        let count = self.count.take();
        let bound = |action| keymap.matches_in(KeyScope::Normal, action, key);
        if let Some((_, scroll)) = SCROLLS.iter().find(|(action, _)| bound(*action)) {
            return Step::Run(Command::Scroll(*scroll), count);
        }
        MOTIONS
            .iter()
            .find(|(action, _)| bound(*action))
            .map_or(Step::Dropped, |(_, motion)| {
                Step::Run(Command::Move(*motion), count)
            })
    }

    fn take_digit(&mut self, key: KeyEvent) -> bool {
        let KeyCode::Char(c) = key.code else {
            return false;
        };
        if !key.modifiers.is_empty() {
            return false;
        }
        let Some(digit) = c.to_digit(10).map(|d| d as usize) else {
            return false;
        };
        if digit == 0 && self.count.is_none() {
            return false;
        }
        let count = self
            .count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit);
        self.count = Some(count.min(MAX_COUNT));
        true
    }
}

const MAX_COUNT: usize = 99_999;

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn feed(keys: &mut Keys, typed: &str) -> Vec<Step> {
        let keymap = KeyMap::default();
        typed
            .chars()
            .map(|c| {
                let modifiers = if c.is_ascii_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                keys.feed(&keymap, KeyEvent::new(KeyCode::Char(c), modifiers))
            })
            .collect()
    }

    fn last(typed: &str) -> Step {
        let mut keys = Keys::default();
        feed(&mut keys, typed).pop().unwrap_or(Step::Dropped)
    }

    #[test]
    fn a_motion_key_runs_at_once() {
        assert_eq!(
            last("w"),
            Step::Run(Command::Move(Motion::WordForward(Word::Small)), None)
        );
    }

    #[test]
    fn digits_build_a_count_for_the_motion() {
        assert_eq!(
            last("12j"),
            Step::Run(Command::Move(Motion::Down), Some(12))
        );
    }

    #[test]
    fn a_leading_zero_is_the_line_start() {
        assert_eq!(last("0"), Step::Run(Command::Move(Motion::LineStart), None));
        assert_eq!(
            last("10l"),
            Step::Run(Command::Move(Motion::Right), Some(10))
        );
    }

    #[test]
    fn an_unbound_key_drops_the_count() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "4zw");

        assert_eq!(steps[1], Step::Dropped);
        assert_eq!(
            steps[2],
            Step::Run(Command::Move(Motion::WordForward(Word::Small)), None)
        );
    }

    #[test]
    fn a_huge_count_saturates() {
        assert_eq!(
            last("99999999999999999999999j"),
            Step::Run(Command::Move(Motion::Down), Some(MAX_COUNT))
        );
    }
}
