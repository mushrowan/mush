use crossterm::event::{KeyCode, KeyEvent};

use crate::keybinds::{Action, KeyMap, KeyScope};
use crate::motion::object::{Kind, Object};
use crate::motion::{Find, Motion, Word};

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
    (Action::MatchPair, Motion::MatchPair),
    (Action::ParagraphForward, Motion::ParagraphForward),
    (Action::ParagraphBack, Motion::ParagraphBack),
];

const FINDS: &[(Action, bool, bool)] = &[
    (Action::FindChar, true, false),
    (Action::FindCharBack, false, false),
    (Action::TillChar, true, true),
    (Action::TillCharBack, false, true),
];

const MARK_JUMPS: &[(Action, bool)] = &[(Action::GotoMark, true), (Action::GotoMarkLine, false)];

const OBJECTS: &[(Action, bool)] = &[(Action::InnerObject, false), (Action::AroundObject, true)];

const OPERATORS: &[(Action, Operator)] = &[
    (Action::Delete, Operator::Delete),
    (Action::Change, Operator::Change),
    (Action::Yank, Operator::Yank),
];

const SHORTHANDS: &[(Action, Operator, Target)] = &[
    (
        Action::DeleteUnderCursor,
        Operator::Delete,
        Target::Motion(Motion::Right),
    ),
    (
        Action::DeleteBeforeCursor,
        Operator::Delete,
        Target::Motion(Motion::Left),
    ),
    (
        Action::DeleteToLineEnd,
        Operator::Delete,
        Target::Motion(Motion::LineEnd),
    ),
    (
        Action::ChangeToLineEnd,
        Operator::Change,
        Target::Motion(Motion::LineEnd),
    ),
    (
        Action::Substitute,
        Operator::Change,
        Target::Motion(Motion::Right),
    ),
    (Action::SubstituteLine, Operator::Change, Target::Lines),
    (
        Action::YankToLineEnd,
        Operator::Yank,
        Target::Motion(Motion::LineEnd),
    ),
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
pub(crate) enum Operator {
    Delete,
    Change,
    Yank,
}

impl Operator {
    fn action(self) -> Action {
        match self {
            Self::Delete => Action::Delete,
            Self::Change => Action::Change,
            Self::Yank => Action::Yank,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    Before,
    After,
    LineStart,
    LineEnd,
    Below,
    Above,
}

const ENTRIES: &[(Action, Entry)] = &[
    (Action::EnterInsert, Entry::Before),
    (Action::Append, Entry::After),
    (Action::InsertLineStart, Entry::LineStart),
    (Action::AppendLineEnd, Entry::LineEnd),
    (Action::OpenBelow, Entry::Below),
    (Action::OpenAbove, Entry::Above),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Motion(Motion),
    Lines,
    Object(Object),
    Selection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Move(Motion),
    Scroll(Scroll),
    Operate(Operator, Target),
    Put { before: bool },
    Undo,
    Redo,
    Insert(Entry),
    Select(Object),
    SwapEnds,
    Replace(char),
    Join,
    ToggleCase,
    Repeat,
    Reselect,
    SetMark(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Pending,
    Run(Command, Option<usize>),
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Context {
    editable: bool,
    visual: bool,
}

impl Context {
    pub(crate) const LOG: Self = Self {
        editable: false,
        visual: false,
    };
    pub(crate) const COMPOSER: Self = Self {
        editable: true,
        visual: false,
    };

    pub(crate) fn visual(self) -> Self {
        Self {
            visual: true,
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    G,
    Find { forward: bool, till: bool },
    Object { around: bool },
    Replace,
    SetMark,
    Mark { exact: bool },
}

enum Reach {
    Motion(Motion),
    Object(Object),
    Command(Command),
}

#[derive(Debug, Default)]
pub(crate) struct Keys {
    count: Option<usize>,
    operator: Option<(Operator, Option<usize>)>,
    prefix: Option<Prefix>,
    last_find: Option<Find>,
}

impl Keys {
    pub(crate) fn is_pending(&self) -> bool {
        self.prefix.is_some() || self.operator.is_some()
    }

    pub(crate) fn clear(&mut self) -> bool {
        let pending = self.count.is_some() || self.is_pending();
        self.count = None;
        self.operator = None;
        self.prefix = None;
        pending
    }

    pub(crate) fn feed(&mut self, keymap: &KeyMap, key: KeyEvent, context: Context) -> Step {
        let step = self.step(keymap, key, context);
        if step != Step::Pending {
            self.clear();
        }
        step
    }

    fn step(&mut self, keymap: &KeyMap, key: KeyEvent, context: Context) -> Step {
        let bound = |action| keymap.matches_in(KeyScope::Normal, action, key);
        if let Some(prefix) = self.prefix.take() {
            return match self.complete(prefix, key, bound) {
                Some(Reach::Motion(motion)) => self.reach(motion),
                Some(Reach::Object(object)) => self.take_object(object),
                Some(Reach::Command(command)) if self.operator.is_none() => self.run(command),
                Some(Reach::Command(_)) | None => Step::Dropped,
            };
        }
        if self.take_digit(key) {
            return Step::Pending;
        }
        if let Some((operator, _)) = self.operator
            && bound(operator.action())
        {
            return self.run(Command::Operate(operator, Target::Lines));
        }
        if bound(Action::GPrefix) {
            self.prefix = Some(Prefix::G);
            return Step::Pending;
        }
        if let Some((_, forward, till)) = FINDS.iter().find(|(action, ..)| bound(*action)) {
            self.prefix = Some(Prefix::Find {
                forward: *forward,
                till: *till,
            });
            return Step::Pending;
        }
        if let Some((_, exact)) = MARK_JUMPS.iter().find(|(action, _)| bound(*action)) {
            self.prefix = Some(Prefix::Mark { exact: *exact });
            return Step::Pending;
        }
        if self.operator.is_none() && bound(Action::SetMark) {
            self.prefix = Some(Prefix::SetMark);
            return Step::Pending;
        }
        if let Some((_, scroll)) = SCROLLS.iter().find(|(action, _)| bound(*action)) {
            if self.operator.is_some() {
                return Step::Dropped;
            }
            return self.run(Command::Scroll(*scroll));
        }
        if let Some(motion) = self.repeat_find(bound) {
            return self.reach(motion);
        }
        if let Some((_, motion)) = MOTIONS.iter().find(|(action, _)| bound(*action)) {
            return self.reach(*motion);
        }
        if (self.operator.is_some() || context.visual)
            && let Some((_, around)) = OBJECTS.iter().find(|(action, _)| bound(*action))
        {
            self.prefix = Some(Prefix::Object { around: *around });
            return Step::Pending;
        }
        if context.visual && self.operator.is_none() {
            return self.on_selection(bound, context);
        }
        if self.operator.is_some() || !context.editable {
            return Step::Dropped;
        }
        if let Some((_, operator)) = OPERATORS.iter().find(|(action, _)| bound(*action)) {
            self.operator = Some((*operator, self.count.take()));
            return Step::Pending;
        }
        if let Some((_, operator, target)) = SHORTHANDS.iter().find(|(action, ..)| bound(*action)) {
            return self.run(Command::Operate(*operator, *target));
        }
        if bound(Action::PutAfter) {
            return self.run(Command::Put { before: false });
        }
        if bound(Action::PutBefore) {
            return self.run(Command::Put { before: true });
        }
        if bound(Action::Undo) {
            return self.run(Command::Undo);
        }
        if bound(Action::Redo) {
            return self.run(Command::Redo);
        }
        if let Some((_, entry)) = ENTRIES.iter().find(|(action, _)| bound(*action)) {
            return self.run(Command::Insert(*entry));
        }
        if bound(Action::RepeatChange) {
            return self.run(Command::Repeat);
        }
        self.shared_edit(bound).unwrap_or(Step::Dropped)
    }

    fn on_selection(&mut self, bound: impl Fn(Action) -> bool, context: Context) -> Step {
        if bound(Action::SwapSelectionEnds) {
            return self.run(Command::SwapEnds);
        }
        if !context.editable {
            return Step::Dropped;
        }
        let command = if bound(Action::Delete) || bound(Action::DeleteUnderCursor) {
            Command::Operate(Operator::Delete, Target::Selection)
        } else if bound(Action::Change) || bound(Action::Substitute) {
            Command::Operate(Operator::Change, Target::Selection)
        } else if bound(Action::PutAfter) {
            Command::Put { before: false }
        } else if bound(Action::PutBefore) {
            Command::Put { before: true }
        } else if let Some(step) = self.shared_edit(bound) {
            return step;
        } else {
            return Step::Dropped;
        };
        self.run(command)
    }

    fn shared_edit(&mut self, bound: impl Fn(Action) -> bool) -> Option<Step> {
        if bound(Action::ReplaceChar) {
            self.prefix = Some(Prefix::Replace);
            return Some(Step::Pending);
        }
        if bound(Action::JoinLines) {
            return Some(self.run(Command::Join));
        }
        if bound(Action::ToggleCase) {
            return Some(self.run(Command::ToggleCase));
        }
        None
    }

    fn reach(&mut self, motion: Motion) -> Step {
        match self.operator {
            Some((operator, _)) => self.run(Command::Operate(operator, Target::Motion(motion))),
            None => self.run(Command::Move(motion)),
        }
    }

    fn take_object(&mut self, object: Object) -> Step {
        match self.operator {
            Some((operator, _)) => self.run(Command::Operate(operator, Target::Object(object))),
            None => self.run(Command::Select(object)),
        }
    }

    fn complete(
        &mut self,
        prefix: Prefix,
        key: KeyEvent,
        bound: impl Fn(Action) -> bool,
    ) -> Option<Reach> {
        let motion = match prefix {
            Prefix::G if bound(Action::GPrefix) => Motion::Top,
            Prefix::G if bound(Action::CursorWordEnd) => Motion::WordEndBack(Word::Small),
            Prefix::G if bound(Action::CursorChunkEnd) => Motion::WordEndBack(Word::Big),
            Prefix::G if bound(Action::StartSelection) => {
                return Some(Reach::Command(Command::Reselect));
            }
            Prefix::G => return None,
            Prefix::Find { forward, till } => {
                let find = Find {
                    target: crate::keybinds::typed_char(key)?,
                    forward,
                    till,
                    repeat: false,
                };
                self.last_find = Some(find);
                Motion::Find(find)
            }
            Prefix::Object { around } => {
                let kind = Kind::for_char(crate::keybinds::typed_char(key)?)?;
                return Some(Reach::Object(Object { kind, around }));
            }
            Prefix::Replace => {
                return Some(Reach::Command(Command::Replace(
                    crate::keybinds::typed_char(key)?,
                )));
            }
            Prefix::SetMark => {
                let name = crate::keybinds::typed_char(key).filter(char::is_ascii_lowercase)?;
                return Some(Reach::Command(Command::SetMark(name)));
            }
            Prefix::Mark { exact } => Motion::Mark {
                name: crate::keybinds::typed_char(key)?,
                exact,
            },
        };
        Some(Reach::Motion(motion))
    }

    fn repeat_find(&self, bound: impl Fn(Action) -> bool) -> Option<Motion> {
        let find = if bound(Action::RepeatFind) {
            self.last_find?
        } else if bound(Action::RepeatFindBack) {
            self.last_find?.reversed()
        } else {
            return None;
        };
        Some(Motion::Find(find.again()))
    }

    fn run(&mut self, command: Command) -> Step {
        let count = self.count.take();
        let count = match self.operator.take() {
            Some((_, Some(outer))) => Some(outer.saturating_mul(count.unwrap_or(1)).min(MAX_COUNT)),
            _ => count,
        };
        Step::Run(command, count)
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
                keys.feed(
                    &keymap,
                    KeyEvent::new(KeyCode::Char(c), modifiers),
                    Context::COMPOSER,
                )
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
    fn gg_waits_for_the_second_g() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "gg");

        assert_eq!(steps[0], Step::Pending);
        assert_eq!(steps[1], Step::Run(Command::Move(Motion::Top), None));
    }

    #[test]
    fn a_count_before_gg_or_g_names_a_line() {
        assert_eq!(
            last("12gg"),
            Step::Run(Command::Move(Motion::Top), Some(12))
        );
        assert_eq!(
            last("7G"),
            Step::Run(Command::Move(Motion::Bottom), Some(7))
        );
    }

    #[test]
    fn ge_and_g_e_go_back_to_word_ends() {
        assert_eq!(
            last("2ge"),
            Step::Run(Command::Move(Motion::WordEndBack(Word::Small)), Some(2))
        );
        assert_eq!(
            last("gE"),
            Step::Run(Command::Move(Motion::WordEndBack(Word::Big)), None)
        );
    }

    #[test]
    fn g_then_an_unknown_key_drops_everything() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "3gzj");

        assert_eq!(steps[2], Step::Dropped);
        assert_eq!(steps[3], Step::Run(Command::Move(Motion::Down), None));
    }

    fn found(target: char, forward: bool, till: bool, repeat: bool) -> Command {
        Command::Move(Motion::Find(Find {
            target,
            forward,
            till,
            repeat,
        }))
    }

    impl Command {
        fn motion(self) -> Motion {
            match self {
                Self::Move(motion) => motion,
                _ => Motion::Left,
            }
        }
    }

    #[test]
    fn f_waits_for_the_character_to_find() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "3fx");

        assert_eq!(steps[1], Step::Pending);
        assert_eq!(steps[2], Step::Run(found('x', true, false, false), Some(3)));
    }

    #[test]
    fn each_find_key_has_its_direction_and_reach() {
        assert_eq!(last("Fx"), Step::Run(found('x', false, false, false), None));
        assert_eq!(last("tx"), Step::Run(found('x', true, true, false), None));
        assert_eq!(last("TX"), Step::Run(found('X', false, true, false), None));
    }

    #[test]
    fn semicolon_and_comma_repeat_the_last_find() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "tx;2,");

        assert_eq!(steps[2], Step::Run(found('x', true, true, true), None));
        assert_eq!(steps[4], Step::Run(found('x', false, true, true), Some(2)));
    }

    #[test]
    fn semicolon_with_nothing_found_yet_does_nothing() {
        assert_eq!(last(";"), Step::Dropped);
    }

    #[test]
    fn f_then_a_key_that_types_nothing_is_dropped() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();
        feed(&mut keys, "f");

        let step = keys.feed(&keymap, KeyEvent::from(KeyCode::Enter), Context::COMPOSER);

        assert_eq!(step, Step::Dropped);
        assert_eq!(feed(&mut keys, ";"), [Step::Dropped]);
    }

    fn operate(operator: Operator, target: Target) -> Command {
        Command::Operate(operator, target)
    }

    #[test]
    fn an_operator_waits_for_its_motion() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "dw");

        assert_eq!(steps[0], Step::Pending);
        assert_eq!(
            steps[1],
            Step::Run(
                operate(
                    Operator::Delete,
                    Target::Motion(Motion::WordForward(Word::Small))
                ),
                None
            )
        );
    }

    #[test]
    fn doubling_an_operator_takes_whole_lines() {
        assert_eq!(
            last("dd"),
            Step::Run(operate(Operator::Delete, Target::Lines), None)
        );
        assert_eq!(
            last("3cc"),
            Step::Run(operate(Operator::Change, Target::Lines), Some(3))
        );
        assert_eq!(
            last("yy"),
            Step::Run(operate(Operator::Yank, Target::Lines), None)
        );
    }

    #[test]
    fn counts_before_and_after_an_operator_multiply() {
        assert_eq!(
            last("2d3w"),
            Step::Run(
                operate(
                    Operator::Delete,
                    Target::Motion(Motion::WordForward(Word::Small))
                ),
                Some(6)
            )
        );
        assert_eq!(
            last("d3w"),
            Step::Run(
                operate(
                    Operator::Delete,
                    Target::Motion(Motion::WordForward(Word::Small))
                ),
                Some(3)
            )
        );
    }

    #[test]
    fn an_operator_reaches_through_finds_and_g_motions() {
        assert_eq!(
            last("ctx"),
            Step::Run(
                operate(
                    Operator::Change,
                    Target::Motion(found('x', true, true, false).motion())
                ),
                None
            )
        );
        assert_eq!(
            last("dgg"),
            Step::Run(operate(Operator::Delete, Target::Motion(Motion::Top)), None)
        );
    }

    #[test]
    fn mismatched_operators_drop_the_command() {
        assert_eq!(last("dc"), Step::Dropped);
        assert_eq!(last("dz"), Step::Dropped);
    }

    #[test]
    fn a_scroll_key_cancels_an_operator() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();
        feed(&mut keys, "d");

        let step = keys.feed(
            &keymap,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Context::COMPOSER,
        );

        assert_eq!(step, Step::Dropped);
    }

    #[test]
    fn shorthands_stand_for_an_operator_and_a_motion() {
        let cases = [
            ("x", Operator::Delete, Target::Motion(Motion::Right)),
            ("X", Operator::Delete, Target::Motion(Motion::Left)),
            ("D", Operator::Delete, Target::Motion(Motion::LineEnd)),
            ("C", Operator::Change, Target::Motion(Motion::LineEnd)),
            ("s", Operator::Change, Target::Motion(Motion::Right)),
            ("S", Operator::Change, Target::Lines),
            ("Y", Operator::Yank, Target::Motion(Motion::LineEnd)),
        ];
        for (typed, operator, target) in cases {
            assert_eq!(
                last(&format!("2{typed}")),
                Step::Run(operate(operator, target), Some(2)),
                "{typed}"
            );
        }
    }

    #[test]
    fn p_and_capital_p_put_after_and_before() {
        assert_eq!(
            last("3p"),
            Step::Run(Command::Put { before: false }, Some(3))
        );
        assert_eq!(last("P"), Step::Run(Command::Put { before: true }, None));
    }

    #[test]
    fn u_and_ctrl_r_undo_and_redo() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();

        assert_eq!(last("2u"), Step::Run(Command::Undo, Some(2)));
        let step = keys.feed(
            &keymap,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            Context::COMPOSER,
        );
        assert_eq!(step, Step::Run(Command::Redo, None));
    }

    #[test]
    fn insert_keys_say_where_typing_starts() {
        let cases = [
            ("i", Entry::Before),
            ("a", Entry::After),
            ("I", Entry::LineStart),
            ("A", Entry::LineEnd),
            ("o", Entry::Below),
            ("O", Entry::Above),
        ];
        for (typed, entry) in cases {
            assert_eq!(
                last(typed),
                Step::Run(Command::Insert(entry), None),
                "{typed}"
            );
        }
    }

    fn object(kind: Kind, around: bool) -> Object {
        Object { kind, around }
    }

    #[test]
    fn an_operator_takes_a_text_object() {
        assert_eq!(
            last("diw"),
            Step::Run(
                operate(
                    Operator::Delete,
                    Target::Object(object(Kind::Word(Word::Small), false))
                ),
                None
            )
        );
        assert_eq!(
            last("2ca("),
            Step::Run(
                operate(
                    Operator::Change,
                    Target::Object(object(Kind::Block('(', ')'), true))
                ),
                Some(2)
            )
        );
        assert_eq!(
            last("yi\""),
            Step::Run(
                operate(
                    Operator::Yank,
                    Target::Object(object(Kind::Quote('"'), false))
                ),
                None
            )
        );
    }

    #[test]
    fn an_unknown_object_drops_the_command() {
        assert_eq!(last("diq"), Step::Dropped);
    }

    #[test]
    fn visual_mode_selects_a_text_object() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();
        let mut steps = Vec::new();
        for c in "iB".chars() {
            let modifiers = if c.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            steps.push(keys.feed(
                &keymap,
                KeyEvent::new(KeyCode::Char(c), modifiers),
                Context::LOG.visual(),
            ));
        }

        assert_eq!(
            steps[1],
            Step::Run(Command::Select(object(Kind::Block('{', '}'), false)), None)
        );
    }

    fn visual(typed: &str, context: Context) -> Step {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();
        let mut step = Step::Dropped;
        for c in typed.chars() {
            let modifiers = if c.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            step = keys.feed(&keymap, KeyEvent::new(KeyCode::Char(c), modifiers), context);
        }
        step
    }

    #[test]
    fn operators_act_on_a_selection_at_once() {
        let selection = Context::COMPOSER.visual();
        let delete = Step::Run(operate(Operator::Delete, Target::Selection), None);
        let change = Step::Run(operate(Operator::Change, Target::Selection), None);

        assert_eq!(visual("d", selection), delete);
        assert_eq!(visual("x", selection), delete);
        assert_eq!(visual("c", selection), change);
        assert_eq!(visual("s", selection), change);
        assert_eq!(
            visual("P", selection),
            Step::Run(Command::Put { before: true }, None)
        );
        assert_eq!(visual("o", selection), Step::Run(Command::SwapEnds, None));
    }

    #[test]
    fn a_log_selection_only_swaps_ends() {
        let selection = Context::LOG.visual();

        assert_eq!(visual("o", selection), Step::Run(Command::SwapEnds, None));
        assert_eq!(visual("d", selection), Step::Dropped);
        assert_eq!(visual("p", selection), Step::Dropped);
    }

    #[test]
    fn r_waits_for_the_replacement() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "3rx");

        assert_eq!(steps[1], Step::Pending);
        assert_eq!(steps[2], Step::Run(Command::Replace('x'), Some(3)));
    }

    #[test]
    fn capital_j_and_tilde_join_and_toggle() {
        assert_eq!(last("3J"), Step::Run(Command::Join, Some(3)));
        assert_eq!(last("~"), Step::Run(Command::ToggleCase, None));
        assert_eq!(
            visual("~", Context::COMPOSER.visual()),
            Step::Run(Command::ToggleCase, None)
        );
        assert_eq!(
            visual("rq", Context::COMPOSER.visual()),
            Step::Run(Command::Replace('q'), None)
        );
    }

    #[test]
    fn dot_repeats_with_its_own_count() {
        assert_eq!(last("."), Step::Run(Command::Repeat, None));
        assert_eq!(last("4."), Step::Run(Command::Repeat, Some(4)));
        assert_eq!(visual(".", Context::COMPOSER.visual()), Step::Dropped);
    }

    #[test]
    fn gv_reselects_but_not_after_an_operator() {
        assert_eq!(last("gv"), Step::Run(Command::Reselect, None));
        assert_eq!(
            visual("gv", Context::LOG.visual()),
            Step::Run(Command::Reselect, None)
        );
        assert_eq!(last("dgv"), Step::Dropped);
    }

    #[test]
    fn marks_are_set_and_reached_by_name() {
        assert_eq!(last("ma"), Step::Run(Command::SetMark('a'), None));
        assert_eq!(last("mA"), Step::Dropped);
        assert_eq!(
            last("`b"),
            Step::Run(
                Command::Move(Motion::Mark {
                    name: 'b',
                    exact: true
                }),
                None
            )
        );
        assert_eq!(
            last("d'c"),
            Step::Run(
                operate(
                    Operator::Delete,
                    Target::Motion(Motion::Mark {
                        name: 'c',
                        exact: false
                    })
                ),
                None
            )
        );
    }

    #[test]
    fn the_log_takes_no_operators() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();

        let step = keys.feed(&keymap, KeyEvent::from(KeyCode::Char('d')), Context::LOG);

        assert_eq!(step, Step::Dropped);
    }

    #[test]
    fn a_huge_count_saturates() {
        assert_eq!(
            last("99999999999999999999999j"),
            Step::Run(Command::Move(Motion::Down), Some(MAX_COUNT))
        );
    }
}
