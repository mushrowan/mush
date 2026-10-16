use crossterm::event::{KeyCode, KeyEvent};

use crate::keybinds::{Action, KeyMap, KeyScope};
use crate::motion::object::{Kind, Object};
use crate::motion::{Find, Motion, Screen, Word};

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
    (Action::SearchNext, Motion::Search { reverse: false }),
    (Action::SearchPrev, Motion::Search { reverse: true }),
    (
        Action::SearchWordForward,
        Motion::SearchWord { forward: true },
    ),
    (
        Action::SearchWordBack,
        Motion::SearchWord { forward: false },
    ),
    (Action::ScreenTop, Motion::Screen(Screen::Top)),
    (Action::ScreenMiddle, Motion::Screen(Screen::Middle)),
    (Action::ScreenBottom, Motion::Screen(Screen::Bottom)),
];

const FINDS: &[(Action, bool, bool)] = &[
    (Action::FindChar, true, false),
    (Action::FindCharBack, false, false),
    (Action::TillChar, true, true),
    (Action::TillCharBack, false, true),
];

const SEARCHES: &[(Action, bool)] = &[(Action::SearchForward, true), (Action::SearchBack, false)];

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
pub(crate) struct Order {
    pub(crate) command: Command,
    pub(crate) count: Option<usize>,
    pub(crate) register: Option<char>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Pending,
    Run(Order),
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
    Register,
    Search { forward: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Search {
    pub(crate) pattern: String,
    pub(crate) forward: bool,
}

enum Reach {
    Motion(Motion),
    Object(Object),
    Command(Command),
    Register(Option<char>),
}

#[derive(Debug, Default)]
pub(crate) struct Keys {
    count: Option<usize>,
    operator: Option<(Operator, Option<usize>)>,
    prefix: Option<Prefix>,
    register: Option<char>,
    prompt: String,
    last_find: Option<Find>,
    last_search: Option<Search>,
}

impl Keys {
    pub(crate) fn is_pending(&self) -> bool {
        self.prefix.is_some() || self.operator.is_some() || self.register.is_some()
    }

    pub(crate) fn clear(&mut self) -> bool {
        let pending = self.count.is_some() || self.is_pending();
        self.count = None;
        self.operator = None;
        self.prefix = None;
        self.register = None;
        self.prompt.clear();
        pending
    }

    pub(crate) fn prompt(&self) -> Option<String> {
        let Some(Prefix::Search { forward }) = self.prefix else {
            return None;
        };
        Some(format!(
            "{}{}",
            if forward { '/' } else { '?' },
            self.prompt
        ))
    }

    pub(crate) fn last_search(&self) -> Option<&Search> {
        self.last_search.as_ref()
    }

    pub(crate) fn remember_search(&mut self, search: Search) {
        self.last_search = Some(search);
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
        if let Some(Prefix::Search { forward }) = self.prefix {
            return self.type_search(key, forward);
        }
        if let Some(prefix) = self.prefix.take() {
            return match self.complete(prefix, key, bound) {
                Some(Reach::Register(name)) => {
                    self.register = name;
                    Step::Pending
                }
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
        if let Some((_, forward)) = SEARCHES.iter().find(|(action, _)| bound(*action)) {
            self.prefix = Some(Prefix::Search { forward: *forward });
            return Step::Pending;
        }
        if let Some((_, exact)) = MARK_JUMPS.iter().find(|(action, _)| bound(*action)) {
            self.prefix = Some(Prefix::Mark { exact: *exact });
            return Step::Pending;
        }
        if self.operator.is_none() && context.editable && bound(Action::SelectRegister) {
            self.prefix = Some(Prefix::Register);
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
        let command = if bound(Action::Yank) || bound(Action::YankRaw) {
            Command::Operate(Operator::Yank, Target::Selection)
        } else if bound(Action::Delete) || bound(Action::DeleteUnderCursor) {
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

    fn type_search(&mut self, key: KeyEvent, forward: bool) -> Step {
        if crate::keybinds::is_plain_enter(key) {
            self.prefix = None;
            let pattern = std::mem::take(&mut self.prompt);
            match (pattern.is_empty(), self.last_search.as_mut()) {
                (false, _) => self.last_search = Some(Search { pattern, forward }),
                (true, Some(last)) => last.forward = forward,
                (true, None) => return Step::Dropped,
            }
            return self.reach(Motion::Search { reverse: false });
        }
        if crate::keybinds::is_backspace(key) {
            return if self.prompt.pop().is_some() {
                Step::Pending
            } else {
                Step::Dropped
            };
        }
        if let Some(c) = crate::keybinds::typed_char(key) {
            self.prompt.push(c);
        }
        Step::Pending
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
            Prefix::Search { .. } => return None,
            Prefix::Register => {
                return match crate::keybinds::typed_char(key)? {
                    '"' => Some(Reach::Register(None)),
                    name if name.is_ascii_alphabetic() || matches!(name, '+' | '_') => {
                        Some(Reach::Register(Some(name)))
                    }
                    _ => None,
                };
            }
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
        Step::Run(Order {
            command,
            count,
            register: self.register.take(),
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
                keys.feed(
                    &keymap,
                    KeyEvent::new(KeyCode::Char(c), modifiers),
                    Context::COMPOSER,
                )
            })
            .collect()
    }

    fn ran(command: Command, count: Option<usize>) -> Step {
        Step::Run(Order {
            command,
            count,
            register: None,
        })
    }

    fn last(typed: &str) -> Step {
        let mut keys = Keys::default();
        feed(&mut keys, typed).pop().unwrap_or(Step::Dropped)
    }

    #[test]
    fn a_motion_key_runs_at_once() {
        assert_eq!(
            last("w"),
            ran(Command::Move(Motion::WordForward(Word::Small)), None)
        );
    }

    #[test]
    fn digits_build_a_count_for_the_motion() {
        assert_eq!(last("12j"), ran(Command::Move(Motion::Down), Some(12)));
    }

    #[test]
    fn a_leading_zero_is_the_line_start() {
        assert_eq!(last("0"), ran(Command::Move(Motion::LineStart), None));
        assert_eq!(last("10l"), ran(Command::Move(Motion::Right), Some(10)));
    }

    #[test]
    fn an_unbound_key_drops_the_count() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "4zw");

        assert_eq!(steps[1], Step::Dropped);
        assert_eq!(
            steps[2],
            ran(Command::Move(Motion::WordForward(Word::Small)), None)
        );
    }

    #[test]
    fn gg_waits_for_the_second_g() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "gg");

        assert_eq!(steps[0], Step::Pending);
        assert_eq!(steps[1], ran(Command::Move(Motion::Top), None));
    }

    #[test]
    fn a_count_before_gg_or_g_names_a_line() {
        assert_eq!(last("12gg"), ran(Command::Move(Motion::Top), Some(12)));
        assert_eq!(last("7G"), ran(Command::Move(Motion::Bottom), Some(7)));
    }

    #[test]
    fn ge_and_g_e_go_back_to_word_ends() {
        assert_eq!(
            last("2ge"),
            ran(Command::Move(Motion::WordEndBack(Word::Small)), Some(2))
        );
        assert_eq!(
            last("gE"),
            ran(Command::Move(Motion::WordEndBack(Word::Big)), None)
        );
    }

    #[test]
    fn g_then_an_unknown_key_drops_everything() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "3gzj");

        assert_eq!(steps[2], Step::Dropped);
        assert_eq!(steps[3], ran(Command::Move(Motion::Down), None));
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
        assert_eq!(steps[2], ran(found('x', true, false, false), Some(3)));
    }

    #[test]
    fn each_find_key_has_its_direction_and_reach() {
        assert_eq!(last("Fx"), ran(found('x', false, false, false), None));
        assert_eq!(last("tx"), ran(found('x', true, true, false), None));
        assert_eq!(last("TX"), ran(found('X', false, true, false), None));
    }

    #[test]
    fn semicolon_and_comma_repeat_the_last_find() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "tx;2,");

        assert_eq!(steps[2], ran(found('x', true, true, true), None));
        assert_eq!(steps[4], ran(found('x', false, true, true), Some(2)));
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
            ran(
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
            ran(operate(Operator::Delete, Target::Lines), None)
        );
        assert_eq!(
            last("3cc"),
            ran(operate(Operator::Change, Target::Lines), Some(3))
        );
        assert_eq!(
            last("yy"),
            ran(operate(Operator::Yank, Target::Lines), None)
        );
    }

    #[test]
    fn counts_before_and_after_an_operator_multiply() {
        assert_eq!(
            last("2d3w"),
            ran(
                operate(
                    Operator::Delete,
                    Target::Motion(Motion::WordForward(Word::Small))
                ),
                Some(6)
            )
        );
        assert_eq!(
            last("d3w"),
            ran(
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
            ran(
                operate(
                    Operator::Change,
                    Target::Motion(found('x', true, true, false).motion())
                ),
                None
            )
        );
        assert_eq!(
            last("dgg"),
            ran(operate(Operator::Delete, Target::Motion(Motion::Top)), None)
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
                ran(operate(operator, target), Some(2)),
                "{typed}"
            );
        }
    }

    #[test]
    fn p_and_capital_p_put_after_and_before() {
        assert_eq!(last("3p"), ran(Command::Put { before: false }, Some(3)));
        assert_eq!(last("P"), ran(Command::Put { before: true }, None));
    }

    #[test]
    fn u_and_ctrl_r_undo_and_redo() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();

        assert_eq!(last("2u"), ran(Command::Undo, Some(2)));
        let step = keys.feed(
            &keymap,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            Context::COMPOSER,
        );
        assert_eq!(step, ran(Command::Redo, None));
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
            assert_eq!(last(typed), ran(Command::Insert(entry), None), "{typed}");
        }
    }

    fn object(kind: Kind, around: bool) -> Object {
        Object { kind, around }
    }

    #[test]
    fn an_operator_takes_a_text_object() {
        assert_eq!(
            last("diw"),
            ran(
                operate(
                    Operator::Delete,
                    Target::Object(object(Kind::Word(Word::Small), false))
                ),
                None
            )
        );
        assert_eq!(
            last("2ca("),
            ran(
                operate(
                    Operator::Change,
                    Target::Object(object(Kind::Block('(', ')'), true))
                ),
                Some(2)
            )
        );
        assert_eq!(
            last("yi\""),
            ran(
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
            ran(Command::Select(object(Kind::Block('{', '}'), false)), None)
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
        let delete = ran(operate(Operator::Delete, Target::Selection), None);
        let change = ran(operate(Operator::Change, Target::Selection), None);

        assert_eq!(visual("d", selection), delete);
        assert_eq!(visual("x", selection), delete);
        assert_eq!(visual("c", selection), change);
        assert_eq!(visual("s", selection), change);
        assert_eq!(
            visual("P", selection),
            ran(Command::Put { before: true }, None)
        );
        assert_eq!(visual("o", selection), ran(Command::SwapEnds, None));
    }

    #[test]
    fn a_log_selection_only_swaps_ends() {
        let selection = Context::LOG.visual();

        assert_eq!(visual("o", selection), ran(Command::SwapEnds, None));
        assert_eq!(visual("d", selection), Step::Dropped);
        assert_eq!(visual("p", selection), Step::Dropped);
    }

    #[test]
    fn r_waits_for_the_replacement() {
        let mut keys = Keys::default();

        let steps = feed(&mut keys, "3rx");

        assert_eq!(steps[1], Step::Pending);
        assert_eq!(steps[2], ran(Command::Replace('x'), Some(3)));
    }

    #[test]
    fn capital_j_and_tilde_join_and_toggle() {
        assert_eq!(last("3J"), ran(Command::Join, Some(3)));
        assert_eq!(last("~"), ran(Command::ToggleCase, None));
        assert_eq!(
            visual("~", Context::COMPOSER.visual()),
            ran(Command::ToggleCase, None)
        );
        assert_eq!(
            visual("rq", Context::COMPOSER.visual()),
            ran(Command::Replace('q'), None)
        );
    }

    #[test]
    fn dot_repeats_with_its_own_count() {
        assert_eq!(last("."), ran(Command::Repeat, None));
        assert_eq!(last("4."), ran(Command::Repeat, Some(4)));
        assert_eq!(visual(".", Context::COMPOSER.visual()), Step::Dropped);
    }

    #[test]
    fn gv_reselects_but_not_after_an_operator() {
        assert_eq!(last("gv"), ran(Command::Reselect, None));
        assert_eq!(
            visual("gv", Context::LOG.visual()),
            ran(Command::Reselect, None)
        );
        assert_eq!(last("dgv"), Step::Dropped);
    }

    #[test]
    fn marks_are_set_and_reached_by_name() {
        assert_eq!(last("ma"), ran(Command::SetMark('a'), None));
        assert_eq!(last("mA"), Step::Dropped);
        assert_eq!(
            last("`b"),
            ran(
                Command::Move(Motion::Mark {
                    name: 'b',
                    exact: true
                }),
                None
            )
        );
        assert_eq!(
            last("d'c"),
            ran(
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
    fn a_register_name_rides_along_with_the_next_command() {
        assert_eq!(
            last("\"a2yy"),
            Step::Run(Order {
                command: operate(Operator::Yank, Target::Lines),
                count: Some(2),
                register: Some('a'),
            })
        );
        assert_eq!(
            last("3\"_dd"),
            Step::Run(Order {
                command: operate(Operator::Delete, Target::Lines),
                count: Some(3),
                register: Some('_'),
            })
        );
        assert_eq!(last("\"\"p"), ran(Command::Put { before: false }, None));
        assert_eq!(feed(&mut Keys::default(), "\"!")[1], Step::Dropped);
    }

    #[test]
    fn a_register_works_on_a_selection_too() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();
        let mut step = Step::Dropped;
        for c in "\"+y".chars() {
            step = keys.feed(
                &keymap,
                KeyEvent::from(KeyCode::Char(c)),
                Context::COMPOSER.visual(),
            );
        }

        assert_eq!(
            step,
            Step::Run(Order {
                command: operate(Operator::Yank, Target::Selection),
                count: None,
                register: Some('+'),
            })
        );
    }

    fn typed(keys: &mut Keys, text: &str) -> Step {
        let keymap = KeyMap::default();
        let mut step = Step::Dropped;
        for c in text.chars() {
            step = keys.feed(&keymap, KeyEvent::from(KeyCode::Char(c)), Context::COMPOSER);
        }
        step
    }

    #[test]
    fn slash_types_a_pattern_until_enter() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();

        assert_eq!(typed(&mut keys, "d/fo"), Step::Pending);
        assert_eq!(keys.prompt().as_deref(), Some("/fo"));
        let step = keys.feed(&keymap, KeyEvent::from(KeyCode::Enter), Context::COMPOSER);

        assert_eq!(
            step,
            ran(
                operate(
                    Operator::Delete,
                    Target::Motion(Motion::Search { reverse: false })
                ),
                None
            )
        );
        assert_eq!(
            keys.last_search(),
            Some(&Search {
                pattern: "fo".into(),
                forward: true
            })
        );
        assert_eq!(keys.prompt(), None);
    }

    #[test]
    fn backspace_edits_the_pattern_and_leaves_when_it_is_empty() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();
        typed(&mut keys, "?ab");
        let backspace = KeyEvent::from(KeyCode::Backspace);

        assert_eq!(
            keys.feed(&keymap, backspace, Context::COMPOSER),
            Step::Pending
        );
        assert_eq!(keys.prompt().as_deref(), Some("?a"));
        keys.feed(&keymap, backspace, Context::COMPOSER);
        assert_eq!(
            keys.feed(&keymap, backspace, Context::COMPOSER),
            Step::Dropped
        );
        assert_eq!(keys.prompt(), None);
    }

    #[test]
    fn an_empty_pattern_reuses_the_last_one_in_the_new_direction() {
        let mut keys = Keys::default();
        let keymap = KeyMap::default();
        typed(&mut keys, "/x");
        keys.feed(&keymap, KeyEvent::from(KeyCode::Enter), Context::COMPOSER);
        typed(&mut keys, "?");

        keys.feed(&keymap, KeyEvent::from(KeyCode::Enter), Context::COMPOSER);

        assert_eq!(
            keys.last_search(),
            Some(&Search {
                pattern: "x".into(),
                forward: false
            })
        );
    }

    #[test]
    fn n_star_and_hash_are_search_motions() {
        assert_eq!(
            last("n"),
            ran(Command::Move(Motion::Search { reverse: false }), None)
        );
        assert_eq!(
            last("3N"),
            ran(Command::Move(Motion::Search { reverse: true }), Some(3))
        );
        assert_eq!(
            last("*"),
            ran(Command::Move(Motion::SearchWord { forward: true }), None)
        );
        assert_eq!(
            last("#"),
            ran(Command::Move(Motion::SearchWord { forward: false }), None)
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
            ran(Command::Move(Motion::Down), Some(MAX_COUNT))
        );
    }
}
