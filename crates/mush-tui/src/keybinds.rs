//! configurable keybinds
//!
//! maps user-facing action names to key combinations parsed from the
//! `[keys]` section of config.toml. actions with no entry in config
//! fall back to their compiled-in default(s).
//!
//! the syntax is crokey's: `ctrl-k`, `alt-enter`, `shift-F6`, `esc`,
//! `f12`, and so on. vscode-style `+` separators are accepted too and
//! rewritten to `-` before parsing so users can write `ctrl+k` or
//! `alt+m` if they prefer.
//!
//! ## config example
//!
//! ```toml
//! [keys]
//! cancel = ["esc", "ctrl+["]
//! cycle_favourite = "alt+m"
//! cycle_favourite_backward = "alt+shift+m"
//! ```
//!
//! ## migration status
//!
//! this is the groundwork layer. today it handles the actions listed
//! in [`Action`] only. the rest of `input.rs` still matches key events
//! directly. new actions should land here first, and existing matches
//! should migrate over time (preferably one action per commit so the
//! diff stays readable).

use std::collections::HashMap;
use std::str::FromStr;

use crokey::KeyCombination;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Deserializer};

/// where a binding is active.
///
/// this mode/scoped shape is inspired by zellij's keybind model: each
/// input mode has its own key table, with shared bindings layered on
/// top where useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyScope {
    Shared,
    Normal,
    Edit,
    Pane,
    SessionPicker,
    ModelPicker,
    AtPicker,
    SlotEdit,
    Settings,
    Search,
    Scroll,
}

impl KeyScope {
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Normal => "normal",
            Self::Edit => "edit",
            Self::Pane => "pane",
            Self::SessionPicker => "session_picker",
            Self::ModelPicker => "model_picker",
            Self::AtPicker => "at_picker",
            Self::SlotEdit => "slot_edit",
            Self::Settings => "settings",
            Self::Search => "search",
            Self::Scroll => "scroll",
        }
    }

    pub const ALL: &'static [Self] = &[
        Self::Shared,
        Self::Normal,
        Self::Edit,
        Self::Pane,
        Self::SessionPicker,
        Self::ModelPicker,
        Self::AtPicker,
        Self::SlotEdit,
        Self::Settings,
        Self::Search,
        Self::Scroll,
    ];
}

#[must_use]
pub fn is_plain_enter(key: KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.is_empty()
}

#[must_use]
pub fn is_tab(key: KeyEvent) -> bool {
    key.code == KeyCode::Tab
}

#[must_use]
pub fn is_backtab(key: KeyEvent) -> bool {
    key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT)
}

#[must_use]
pub fn is_backspace(key: KeyEvent) -> bool {
    key.code == KeyCode::Backspace
}

#[must_use]
pub fn is_delete(key: KeyEvent) -> bool {
    key.code == KeyCode::Delete
}

#[must_use]
pub fn typed_char(key: KeyEvent) -> Option<char> {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(ch)) => Some(ch),
        _ => None,
    }
}

#[must_use]
pub fn any_char(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(ch) => Some(ch),
        _ => None,
    }
}

#[must_use]
pub fn control_non_alt_char(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            Some(ch)
        }
        _ => None,
    }
}

#[must_use]
pub fn has_alt(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT)
}

/// a single named action the user can rebind. keep this enum alphabetised
/// so the config key space stays predictable.
///
/// when adding a new variant: give it a default in [`KeyMap::default`] and
/// migrate the matching key-event match in `input.rs` to call
/// [`KeyMap::matches`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// cycle forward through favourite models. default binding: `alt-m`
    CycleFavourite,
    /// cycle backward through favourite models. default binding: `alt-shift-m`
    CycleFavouriteBackward,
    /// cancel the current overlay or abort re-anchored busy output.
    /// default bindings: `esc`, `ctrl-[`
    Cancel,
    /// cycle through thinking levels (off → low → medium → high → xhigh).
    /// default binding: `ctrl-t`
    CycleThinkingLevel,
    Confirm,
    CopySelection,
    CursorDown,
    /// move the input cursor to the end of the line. default bindings:
    /// `end`, `ctrl-e`
    CursorLineEnd,
    /// move the input cursor to the start of the line. default bindings:
    /// `home`, `ctrl-a`
    CursorLineStart,
    /// move the input cursor one character left. default bindings:
    /// `left`, `ctrl-b`
    CursorLeft,
    /// move the input cursor one character right. default binding: `right`
    CursorRight,
    /// move the input cursor one word left. default bindings:
    /// `alt-left`, `ctrl-left`, `alt-b`
    CursorWordLeft,
    /// move the input cursor one word right. default bindings:
    /// `alt-right`, `ctrl-right`, `alt-f`
    CursorWordRight,
    CursorUp,
    /// pop the last queued steering message back into the input for
    /// editing. default bindings: `alt-k`, `up`, `ctrl-k` (the latter
    /// two only fire when the input is empty)
    EditSteering,
    /// enter fullscreen search mode. default binding: `ctrl-f`
    EnterSearch,
    /// enter scroll / selection mode. default binding: `ctrl-s`
    EnterScroll,
    /// focus pane 1. default binding: `alt-1`
    FocusPane1,
    /// focus pane 2. default binding: `alt-2`
    FocusPane2,
    /// focus pane 3. default binding: `alt-3`
    FocusPane3,
    /// focus pane 4. default binding: `alt-4`
    FocusPane4,
    /// focus pane 5. default binding: `alt-5`
    FocusPane5,
    /// focus pane 6. default binding: `alt-6`
    FocusPane6,
    /// focus pane 7. default binding: `alt-7`
    FocusPane7,
    /// focus pane 8. default binding: `alt-8`
    FocusPane8,
    /// focus pane 9. default binding: `alt-9`
    FocusPane9,
    /// jump the message log to the bottom without aborting or
    /// otherwise affecting the stream. default binding: `shift-end`
    JumpToBottom,
    /// paste an image from the system clipboard. default binding: `ctrl-v`
    PasteImage,
    /// clear input, abort a busy turn, or arm/confirm quit. default
    /// binding: `ctrl-c`
    QuitOrAbort,
    /// page the message log down. default binding: `pagedown`
    ScrollPageDown,
    /// page the message log up. default binding: `pageup`
    ScrollPageUp,
    /// delete the word before the cursor. default bindings:
    /// `ctrl-backspace`, `alt-backspace`, `ctrl-w`
    DeleteWordBackward,
    /// delete the word after the cursor. default binding: `alt-d`
    DeleteWordForward,
    /// delete the character under the cursor, or quit when the input is
    /// empty. default binding: `ctrl-d`
    DeleteCharOrQuit,
    /// delete from cursor to end of line. default binding: `ctrl-k`
    DeleteToEnd,
    /// delete from cursor to start of line. default binding: `ctrl-u`
    DeleteToStart,
    /// split the current pane. default binding: `ctrl-shift-enter`
    SplitPane,
    /// focus the next pane. default binding: `ctrl-tab`
    FocusNextPane,
    /// focus the previous pane. default binding: `ctrl-shift-tab`
    FocusPrevPane,
    /// grow the focused pane to the right. default binding: `ctrl-shift-right`
    ResizePaneRight,
    /// grow the focused pane to the left. default binding: `ctrl-shift-left`
    ResizePaneLeft,
    /// insert a literal newline in the input. default bindings:
    /// `alt-enter`, `shift-enter`, `ctrl-j`
    InsertNewline,
    NavigateBottom,
    NavigateTop,
    NextItem,
    PreviousItem,
    /// toggle the session picker between this directory and all
    /// directories. default binding: `tab`
    SessionPickerToggleScope,
    /// arm deletion for every stale model in the model picker. default
    /// bindings: `ctrl-shift-d`, `alt-d`
    ModelPickerDeleteAllStale,
    /// arm deletion for the selected stale model in the model picker.
    /// default binding: `ctrl-d`
    ModelPickerDeleteSelectedStale,
    /// toggle favourite for the selected model in the model picker.
    /// default binding: `ctrl-f`
    ModelPickerToggleFavourite,
    SearchNext,
    SearchPrev,
    SelectOrSpace,
    ToggleBlockMode,
    ToggleVisualSelection,
}

impl Action {
    /// toml-key form: the snake_case string users write in config.toml
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::CycleFavourite => "cycle_favourite",
            Self::CycleFavouriteBackward => "cycle_favourite_backward",
            Self::Cancel => "cancel",
            Self::CycleThinkingLevel => "cycle_thinking_level",
            Self::Confirm => "confirm",
            Self::CopySelection => "copy_selection",
            Self::CursorDown => "cursor_down",
            Self::CursorLineEnd => "cursor_line_end",
            Self::CursorLineStart => "cursor_line_start",
            Self::CursorLeft => "cursor_left",
            Self::CursorRight => "cursor_right",
            Self::CursorWordLeft => "cursor_word_left",
            Self::CursorWordRight => "cursor_word_right",
            Self::CursorUp => "cursor_up",
            Self::DeleteCharOrQuit => "delete_char_or_quit",
            Self::DeleteToEnd => "delete_to_end",
            Self::DeleteToStart => "delete_to_start",
            Self::EditSteering => "edit_steering",
            Self::EnterSearch => "enter_search",
            Self::EnterScroll => "enter_scroll",
            Self::InsertNewline => "insert_newline",
            Self::NavigateBottom => "navigate_bottom",
            Self::NavigateTop => "navigate_top",
            Self::NextItem => "next_item",
            Self::PreviousItem => "previous_item",
            Self::SessionPickerToggleScope => "session_picker_toggle_scope",
            Self::ModelPickerDeleteAllStale => "model_picker_delete_all_stale",
            Self::ModelPickerDeleteSelectedStale => "model_picker_delete_selected_stale",
            Self::ModelPickerToggleFavourite => "model_picker_toggle_favourite",
            Self::SearchNext => "search_next",
            Self::SearchPrev => "search_prev",
            Self::SelectOrSpace => "select_or_space",
            Self::ToggleBlockMode => "toggle_block_mode",
            Self::ToggleVisualSelection => "toggle_visual_selection",
            Self::JumpToBottom => "jump_to_bottom",
            Self::PasteImage => "paste_image",
            Self::QuitOrAbort => "quit_or_abort",
            Self::ScrollPageDown => "scroll_page_down",
            Self::ScrollPageUp => "scroll_page_up",
            Self::DeleteWordBackward => "delete_word_backward",
            Self::DeleteWordForward => "delete_word_forward",
            Self::SplitPane => "split_pane",
            Self::FocusNextPane => "focus_next_pane",
            Self::FocusPane1 => "focus_pane_one",
            Self::FocusPane2 => "focus_pane_two",
            Self::FocusPane3 => "focus_pane_three",
            Self::FocusPane4 => "focus_pane_four",
            Self::FocusPane5 => "focus_pane_five",
            Self::FocusPane6 => "focus_pane_six",
            Self::FocusPane7 => "focus_pane_seven",
            Self::FocusPane8 => "focus_pane_eight",
            Self::FocusPane9 => "focus_pane_nine",
            Self::FocusPrevPane => "focus_prev_pane",
            Self::ResizePaneRight => "resize_pane_right",
            Self::ResizePaneLeft => "resize_pane_left",
        }
    }

    /// every variant, for iteration in default population and tests
    pub const ALL: &'static [Self] = &[
        Self::CycleFavourite,
        Self::CycleFavouriteBackward,
        Self::Cancel,
        Self::CycleThinkingLevel,
        Self::Confirm,
        Self::CopySelection,
        Self::CursorDown,
        Self::CursorLineEnd,
        Self::CursorLineStart,
        Self::CursorLeft,
        Self::CursorRight,
        Self::CursorWordLeft,
        Self::CursorWordRight,
        Self::CursorUp,
        Self::DeleteCharOrQuit,
        Self::DeleteToEnd,
        Self::DeleteToStart,
        Self::EditSteering,
        Self::EnterSearch,
        Self::EnterScroll,
        Self::InsertNewline,
        Self::NavigateBottom,
        Self::NavigateTop,
        Self::NextItem,
        Self::PreviousItem,
        Self::SessionPickerToggleScope,
        Self::ModelPickerDeleteAllStale,
        Self::ModelPickerDeleteSelectedStale,
        Self::ModelPickerToggleFavourite,
        Self::SearchNext,
        Self::SearchPrev,
        Self::SelectOrSpace,
        Self::ToggleBlockMode,
        Self::ToggleVisualSelection,
        Self::JumpToBottom,
        Self::PasteImage,
        Self::QuitOrAbort,
        Self::ScrollPageDown,
        Self::ScrollPageUp,
        Self::DeleteWordBackward,
        Self::DeleteWordForward,
        Self::SplitPane,
        Self::FocusNextPane,
        Self::FocusPane1,
        Self::FocusPane2,
        Self::FocusPane3,
        Self::FocusPane4,
        Self::FocusPane5,
        Self::FocusPane6,
        Self::FocusPane7,
        Self::FocusPane8,
        Self::FocusPane9,
        Self::FocusPrevPane,
        Self::ResizePaneRight,
        Self::ResizePaneLeft,
    ];
}

/// zero or more key combinations bound to a single action
#[derive(Debug, Clone, Default)]
pub struct Binding(pub Vec<KeyCombination>);

impl Binding {
    #[must_use]
    pub fn single(combo: KeyCombination) -> Self {
        Self(vec![combo])
    }

    /// parse "alt+k" / "alt-k" / ["esc", "ctrl+["] style strings into
    /// a binding. the vscode-style `+` separator is normalised to `-`
    /// before handing off to crokey so both flavours work
    pub fn parse_one(raw: &str) -> Result<KeyCombination, crokey::ParseKeyError> {
        let normalised = raw.replace('+', "-");
        crokey::parse(&normalised)
    }

    /// does this binding fire for the given key event?
    ///
    /// both the incoming event and the stored combos are run through
    /// crokey's `normalized()` first so `shift+m` vs uppercase `M`
    /// differences between config syntax and crossterm's delivery
    /// don't cause spurious misses
    #[must_use]
    pub fn matches(&self, key: KeyEvent) -> bool {
        let combo = KeyCombination::from(key).normalized();
        self.0.iter().any(|b| b.normalized() == combo)
    }
}

/// a string or list-of-strings that deserialises into a `Binding`.
/// this is the wire format users see in config.toml
#[derive(Debug, Clone, Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum BindingSource {
    One(String),
    Many(Vec<String>),
}

impl BindingSource {
    fn into_binding(self) -> Result<Binding, crokey::ParseKeyError> {
        let raws = match self {
            Self::One(s) => vec![s],
            Self::Many(v) => v,
        };
        let combos: Result<Vec<_>, _> = raws.iter().map(|r| Binding::parse_one(r)).collect();
        combos.map(Binding)
    }
}

/// the full keymap: scope → action → binding
#[derive(Debug, Clone)]
pub struct KeyMap {
    bindings: HashMap<KeyScope, HashMap<Action, Binding>>,
}

impl KeyMap {
    /// lookup the binding for an action. always returns Some for known
    /// actions because defaults are populated at construction time
    #[must_use]
    pub fn binding(&self, action: Action) -> Option<&Binding> {
        self.binding_in(default_scope_for_action(action), action)
    }

    #[must_use]
    pub fn binding_in(&self, scope: KeyScope, action: Action) -> Option<&Binding> {
        self.bindings
            .get(&scope)
            .and_then(|bindings| bindings.get(&action))
            .or_else(|| {
                if scope == KeyScope::Shared {
                    None
                } else {
                    self.bindings
                        .get(&KeyScope::Shared)
                        .and_then(|bindings| bindings.get(&action))
                }
            })
    }

    /// does the key event fire the given action?
    #[must_use]
    pub fn matches(&self, action: Action, key: KeyEvent) -> bool {
        self.matches_in(default_scope_for_action(action), action, key)
    }

    #[must_use]
    pub fn matches_in(&self, scope: KeyScope, action: Action, key: KeyEvent) -> bool {
        self.binding_in(scope, action)
            .is_some_and(|b| b.matches(key))
    }

    pub fn override_scoped(&mut self, raw: &ScopedKeysConfig) -> Vec<String> {
        let mut warnings = Vec::new();
        for (scope_key, entries) in &raw.scopes {
            let Some(scope) = KeyScope::ALL
                .iter()
                .copied()
                .find(|scope| scope.config_key() == scope_key)
            else {
                warnings.push(format!("unknown keybind scope: {scope_key}"));
                continue;
            };
            for (action_key, src) in entries {
                let Some(action) = Action::ALL
                    .iter()
                    .copied()
                    .find(|action| action.config_key() == action_key)
                else {
                    warnings.push(format!(
                        "unknown keybind action in {scope_key}: {action_key}"
                    ));
                    continue;
                };
                match src.clone().into_binding() {
                    Ok(binding) => {
                        self.bindings
                            .entry(scope)
                            .or_default()
                            .insert(action, binding);
                    }
                    Err(error) => warnings.push(format!(
                        "invalid keybind for {scope_key}.{action_key}: {error}"
                    )),
                }
            }
        }
        warnings
    }
}

const fn default_scope_for_action(action: Action) -> KeyScope {
    match action {
        Action::Cancel
        | Action::Confirm
        | Action::JumpToBottom
        | Action::NavigateBottom
        | Action::NavigateTop
        | Action::NextItem
        | Action::PreviousItem
        | Action::QuitOrAbort
        | Action::ScrollPageDown
        | Action::ScrollPageUp
        | Action::SelectOrSpace => KeyScope::Shared,
        Action::CopySelection | Action::ToggleBlockMode | Action::ToggleVisualSelection => {
            KeyScope::Scroll
        }
        Action::SessionPickerToggleScope => KeyScope::SessionPicker,
        Action::ModelPickerDeleteAllStale
        | Action::ModelPickerDeleteSelectedStale
        | Action::ModelPickerToggleFavourite => KeyScope::ModelPicker,
        Action::SplitPane
        | Action::FocusNextPane
        | Action::FocusPrevPane
        | Action::FocusPane1
        | Action::FocusPane2
        | Action::FocusPane3
        | Action::FocusPane4
        | Action::FocusPane5
        | Action::FocusPane6
        | Action::FocusPane7
        | Action::FocusPane8
        | Action::FocusPane9
        | Action::ResizePaneRight
        | Action::ResizePaneLeft => KeyScope::Pane,
        Action::CursorLineEnd
        | Action::CursorLineStart
        | Action::CursorLeft
        | Action::CursorRight
        | Action::CursorWordLeft
        | Action::CursorWordRight
        | Action::DeleteCharOrQuit
        | Action::DeleteToEnd
        | Action::DeleteToStart
        | Action::DeleteWordBackward
        | Action::DeleteWordForward => KeyScope::Edit,
        _ => KeyScope::Normal,
    }
}

impl Default for KeyMap {
    fn default() -> Self {
        let mut bindings: HashMap<KeyScope, HashMap<Action, Binding>> = HashMap::new();

        macro_rules! bind {
            ($scope:expr, $action:expr, $binding:expr $(,)?) => {
                bindings
                    .entry($scope)
                    .or_default()
                    .insert($action, $binding);
            };
        }

        // edit_steering: alt-k always, plus up / ctrl-k as aliases for
        // when the input is empty (the empty check lives at the call
        // site, not in the binding itself)
        bind!(
            KeyScope::Normal,
            Action::EditSteering,
            Binding(vec![
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::ALT),
                KeyCombination::new(KeyCode::Up, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Shared,
            Action::Cancel,
            Binding(vec![
                KeyCombination::new(KeyCode::Esc, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('['), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Normal,
            Action::CycleFavourite,
            Binding::single(KeyCombination::new(KeyCode::Char('m'), KeyModifiers::ALT)),
        );

        // note: crossterm delivers shift-m as KeyCode::Char('M') with
        // the ALT modifier set (no explicit SHIFT), so that's how the
        // binding must be declared to match incoming events
        bind!(
            KeyScope::Normal,
            Action::CycleFavouriteBackward,
            Binding::single(KeyCombination::new(KeyCode::Char('M'), KeyModifiers::ALT)),
        );

        bind!(
            KeyScope::Normal,
            Action::EnterSearch,
            Binding::single(KeyCombination::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::Normal,
            Action::EnterScroll,
            Binding::single(KeyCombination::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            )),
        );

        // shift+End: jump the message log to the bottom without touching
        // the stream. plain End is line-end in the input editor; ctrl+End
        // isn't reliably delivered across terminals
        bind!(
            KeyScope::Shared,
            Action::JumpToBottom,
            Binding::single(KeyCombination::new(KeyCode::End, KeyModifiers::SHIFT)),
        );

        bind!(
            KeyScope::Shared,
            Action::QuitOrAbort,
            Binding::single(KeyCombination::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::Shared,
            Action::ScrollPageUp,
            Binding::single(KeyCombination::new(KeyCode::PageUp, KeyModifiers::NONE)),
        );

        bind!(
            KeyScope::Shared,
            Action::ScrollPageDown,
            Binding::single(KeyCombination::new(KeyCode::PageDown, KeyModifiers::NONE)),
        );

        bind!(
            KeyScope::Shared,
            Action::Confirm,
            Binding::single(KeyCombination::new(KeyCode::Enter, KeyModifiers::NONE)),
        );

        bind!(
            KeyScope::Shared,
            Action::SelectOrSpace,
            Binding(vec![
                KeyCombination::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char(' '), KeyModifiers::NONE),
            ]),
        );

        bind!(
            KeyScope::Shared,
            Action::NextItem,
            Binding(vec![
                KeyCombination::new(KeyCode::Tab, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Down, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('j'), KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
                KeyCombination::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Shared,
            Action::PreviousItem,
            Binding(vec![
                KeyCombination::new(KeyCode::BackTab, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                KeyCombination::new(KeyCode::Tab, KeyModifiers::SHIFT),
                KeyCombination::new(KeyCode::Up, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
                KeyCombination::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::SessionPicker,
            Action::NextItem,
            Binding(vec![
                KeyCombination::new(KeyCode::Down, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::SessionPicker,
            Action::PreviousItem,
            Binding(vec![
                KeyCombination::new(KeyCode::Up, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::SessionPicker,
            Action::SessionPickerToggleScope,
            Binding(vec![KeyCombination::new(KeyCode::Tab, KeyModifiers::NONE)]),
        );

        bind!(
            KeyScope::ModelPicker,
            Action::NextItem,
            Binding(vec![
                KeyCombination::new(KeyCode::Down, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::ModelPicker,
            Action::PreviousItem,
            Binding(vec![
                KeyCombination::new(KeyCode::Up, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Shared,
            Action::NavigateTop,
            Binding(vec![
                KeyCombination::new(KeyCode::Home, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('g'), KeyModifiers::NONE),
            ]),
        );

        bind!(
            KeyScope::Shared,
            Action::NavigateBottom,
            Binding(vec![
                KeyCombination::new(KeyCode::End, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
            ]),
        );

        bind!(
            KeyScope::Scroll,
            Action::ToggleBlockMode,
            Binding::single(KeyCombination::new(KeyCode::Char('b'), KeyModifiers::NONE)),
        );

        bind!(
            KeyScope::Scroll,
            Action::ToggleVisualSelection,
            Binding::single(KeyCombination::new(KeyCode::Char('v'), KeyModifiers::NONE)),
        );

        bind!(
            KeyScope::Scroll,
            Action::CopySelection,
            Binding::single(KeyCombination::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        );

        bind!(
            KeyScope::Normal,
            Action::CycleThinkingLevel,
            Binding::single(KeyCombination::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::Normal,
            Action::PasteImage,
            Binding::single(KeyCombination::new(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::Edit,
            Action::DeleteWordBackward,
            Binding(vec![
                KeyCombination::new(KeyCode::Backspace, KeyModifiers::CONTROL),
                KeyCombination::new(KeyCode::Backspace, KeyModifiers::ALT),
                KeyCombination::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Edit,
            Action::DeleteWordForward,
            Binding::single(KeyCombination::new(KeyCode::Char('d'), KeyModifiers::ALT)),
        );

        bind!(
            KeyScope::Edit,
            Action::DeleteCharOrQuit,
            Binding::single(KeyCombination::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::Edit,
            Action::DeleteToStart,
            Binding::single(KeyCombination::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::Edit,
            Action::DeleteToEnd,
            Binding::single(KeyCombination::new(
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::Edit,
            Action::CursorWordLeft,
            Binding(vec![
                KeyCombination::new(KeyCode::Left, KeyModifiers::ALT),
                KeyCombination::new(KeyCode::Left, KeyModifiers::CONTROL),
                KeyCombination::new(KeyCode::Char('b'), KeyModifiers::ALT),
            ]),
        );

        bind!(
            KeyScope::Edit,
            Action::CursorLeft,
            Binding(vec![
                KeyCombination::new(KeyCode::Left, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Edit,
            Action::CursorRight,
            Binding::single(KeyCombination::new(KeyCode::Right, KeyModifiers::NONE)),
        );

        bind!(
            KeyScope::Edit,
            Action::CursorWordRight,
            Binding(vec![
                KeyCombination::new(KeyCode::Right, KeyModifiers::ALT),
                KeyCombination::new(KeyCode::Right, KeyModifiers::CONTROL),
                KeyCombination::new(KeyCode::Char('f'), KeyModifiers::ALT),
            ]),
        );

        bind!(
            KeyScope::Edit,
            Action::CursorLineStart,
            Binding(vec![
                KeyCombination::new(KeyCode::Home, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Edit,
            Action::CursorLineEnd,
            Binding(vec![
                KeyCombination::new(KeyCode::End, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::Normal,
            Action::InsertNewline,
            Binding(vec![
                KeyCombination::new(KeyCode::Enter, KeyModifiers::ALT),
                KeyCombination::new(KeyCode::Enter, KeyModifiers::SHIFT),
                KeyCombination::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            ]),
        );

        bind!(
            KeyScope::ModelPicker,
            Action::ModelPickerToggleFavourite,
            Binding::single(KeyCombination::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::ModelPicker,
            Action::ModelPickerDeleteSelectedStale,
            Binding::single(KeyCombination::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )),
        );

        bind!(
            KeyScope::ModelPicker,
            Action::ModelPickerDeleteAllStale,
            Binding(vec![
                KeyCombination::new(
                    KeyCode::Char('D'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                KeyCombination::new(KeyCode::Char('d'), KeyModifiers::ALT),
            ]),
        );

        bind!(
            KeyScope::Pane,
            Action::SplitPane,
            Binding::single(KeyCombination::new(
                KeyCode::Enter,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
        );
        bind!(
            KeyScope::Pane,
            Action::FocusNextPane,
            Binding::single(KeyCombination::new(KeyCode::Tab, KeyModifiers::CONTROL)),
        );
        bind!(
            KeyScope::Pane,
            Action::FocusPrevPane,
            Binding(vec![
                KeyCombination::new(KeyCode::BackTab, KeyModifiers::CONTROL),
                KeyCombination::new(
                    KeyCode::BackTab,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
            ]),
        );
        for (key_char, action) in [
            ('1', Action::FocusPane1),
            ('2', Action::FocusPane2),
            ('3', Action::FocusPane3),
            ('4', Action::FocusPane4),
            ('5', Action::FocusPane5),
            ('6', Action::FocusPane6),
            ('7', Action::FocusPane7),
            ('8', Action::FocusPane8),
            ('9', Action::FocusPane9),
        ] {
            bind!(
                KeyScope::Pane,
                action,
                Binding::single(KeyCombination::new(
                    KeyCode::Char(key_char),
                    KeyModifiers::ALT
                )),
            );
        }
        bind!(
            KeyScope::Pane,
            Action::ResizePaneRight,
            Binding::single(KeyCombination::new(
                KeyCode::Right,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
        );
        bind!(
            KeyScope::Pane,
            Action::ResizePaneLeft,
            Binding::single(KeyCombination::new(
                KeyCode::Left,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
        );

        Self { bindings }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct KeyScopeConfig {
    pub raw: HashMap<String, BindingSource>,
}

#[derive(Debug, Default, Clone, serde::Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ScopedKeysConfig {
    pub scopes: HashMap<String, HashMap<String, BindingSource>>,
}

/// config-side type used by `KeysConfig` below. users put scoped tables
/// under `[keys]`, such as `[keys.normal]` and `[keys.model_picker]`.
#[derive(Debug, Default, Clone, serde::Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct KeysConfig {
    pub shared: KeyScopeConfig,
    pub normal: KeyScopeConfig,
    pub edit: KeyScopeConfig,
    pub pane: KeyScopeConfig,
    pub model_picker: KeyScopeConfig,
}

impl<'de> Deserialize<'de> for KeysConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = HashMap::<String, toml::Value>::deserialize(deserializer)?;
        let mut config = Self::default();
        for (key, value) in raw {
            match key.as_str() {
                "shared" => config.shared = value.try_into().map_err(serde::de::Error::custom)?,
                "normal" => config.normal = value.try_into().map_err(serde::de::Error::custom)?,
                "edit" => config.edit = value.try_into().map_err(serde::de::Error::custom)?,
                "pane" => config.pane = value.try_into().map_err(serde::de::Error::custom)?,
                "model_picker" => {
                    config.model_picker = value.try_into().map_err(serde::de::Error::custom)?;
                }
                old_key => {
                    return Err(serde::de::Error::custom(format!(
                        "old flat [keys] entry `{old_key}` is no longer supported. move it under a scoped table such as [keys.normal], [keys.edit], [keys.pane], [keys.shared], or [keys.model_picker]"
                    )));
                }
            }
        }
        Ok(config)
    }
}

impl KeysConfig {
    pub fn scoped(&self) -> ScopedKeysConfig {
        let mut scopes = HashMap::new();
        scopes.insert(
            KeyScope::Shared.config_key().to_string(),
            self.shared.raw.clone(),
        );
        scopes.insert(
            KeyScope::Normal.config_key().to_string(),
            self.normal.raw.clone(),
        );
        scopes.insert(
            KeyScope::Edit.config_key().to_string(),
            self.edit.raw.clone(),
        );
        scopes.insert(
            KeyScope::Pane.config_key().to_string(),
            self.pane.raw.clone(),
        );
        scopes.insert(
            KeyScope::ModelPicker.config_key().to_string(),
            self.model_picker.raw.clone(),
        );
        ScopedKeysConfig { scopes }
    }
}

#[cfg(test)]
pub(crate) fn scoped_test_config(
    scope: KeyScope,
    entries: impl IntoIterator<Item = (&'static str, BindingSource)>,
) -> ScopedKeysConfig {
    ScopedKeysConfig {
        scopes: HashMap::from([(
            scope.config_key().to_string(),
            entries
                .into_iter()
                .map(|(key, source)| (key.to_string(), source))
                .collect(),
        )]),
    }
}

impl FromStr for Binding {
    type Err = crokey::ParseKeyError;
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse_one(raw).map(Self::single)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn parse_one_accepts_vscode_plus_syntax() {
        // config accepts vscode-style `+` as well as crokey's upstream
        // `-`. the normalisation in parse_one means both land on the
        // same KeyCombination
        let dash = Binding::parse_one("alt-k").unwrap();
        let plus = Binding::parse_one("alt+k").unwrap();
        assert_eq!(dash, plus);
    }

    #[test]
    fn parse_one_accepts_multi_modifier() {
        let parsed = Binding::parse_one("ctrl+shift+p").unwrap();
        assert_eq!(
            parsed,
            KeyCombination::new(
                KeyCode::Char('P'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )
        );
    }

    #[test]
    fn default_keymap_has_edit_steering_triggers() {
        let map = KeyMap::default();
        let alt_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(map.matches(Action::EditSteering, alt_k));
        assert!(map.matches(Action::EditSteering, up));
        assert!(map.matches(Action::EditSteering, ctrl_k));
    }

    #[test]
    fn default_keymap_has_cycle_favourite_triggers() {
        let map = KeyMap::default();
        let alt_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT);
        let alt_shift_m = KeyEvent::new(KeyCode::Char('M'), KeyModifiers::ALT);
        assert!(map.matches(Action::CycleFavourite, alt_m));
        assert!(map.matches(Action::CycleFavouriteBackward, alt_shift_m));
        // not cross-bound
        assert!(!map.matches(Action::CycleFavourite, alt_shift_m));
        assert!(!map.matches(Action::CycleFavouriteBackward, alt_m));
    }

    #[test]
    fn default_keymap_has_toggle_actions() {
        // ctrl+t used to be hardcoded in input.rs and is now
        // configurable so users can rebind it without editing the
        // source. defaults stay the same as before
        let map = KeyMap::default();
        assert!(map.matches(
            Action::CycleThinkingLevel,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn default_keymap_has_mode_switch_triggers() {
        let map = KeyMap::default();
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(map.matches(Action::EnterSearch, ctrl_f));
        assert!(map.matches(Action::EnterScroll, ctrl_s));
    }

    #[test]
    fn default_keymap_has_word_deletion_bindings() {
        let map = KeyMap::default();
        assert!(map.matches(
            Action::DeleteWordBackward,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::DeleteWordBackward,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::DeleteWordForward,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT)
        ));
        assert!(map.matches(
            Action::DeleteCharOrQuit,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn default_keymap_has_text_editing_bindings() {
        let map = KeyMap::default();
        assert!(map.matches(
            Action::DeleteToStart,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::DeleteToEnd,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::CursorWordLeft,
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)
        ));
        assert!(map.matches(
            Action::CursorWordRight,
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::CursorLineStart,
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)
        ));
        assert!(map.matches(
            Action::CursorLineEnd,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::InsertNewline,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        ));
    }

    #[test]
    fn default_keymap_has_cancel_and_model_picker_bindings() {
        let map = KeyMap::default();
        assert!(map.matches(
            Action::Cancel,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        ));
        assert!(map.matches(
            Action::Cancel,
            KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::ModelPickerToggleFavourite,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::ModelPickerDeleteSelectedStale,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::ModelPickerDeleteAllStale,
            KeyEvent::new(
                KeyCode::Char('D'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        ));
        assert!(map.matches(
            Action::ModelPickerDeleteAllStale,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT)
        ));
    }

    #[test]
    fn default_keymap_has_pane_bindings() {
        let map = KeyMap::default();
        assert!(map.matches(
            Action::SplitPane,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        ));
        assert!(map.matches(
            Action::FocusNextPane,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::FocusPrevPane,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::ResizePaneLeft,
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        ));
        assert!(map.matches(
            Action::ResizePaneRight,
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        ));
    }

    #[test]
    fn default_keymap_has_jump_to_bottom_trigger() {
        // shift+End jumps the message log to the bottom without aborting
        // the stream. chosen because plain End is already line-end in the
        // input editor, and ctrl+End isn't consistently delivered across
        // terminals. the action stays opt-in configurable via [keys]
        let map = KeyMap::default();
        let shift_end = KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT);
        assert!(map.matches(Action::JumpToBottom, shift_end));
    }

    #[test]
    fn scoped_override_replaces_default() {
        let mut map = KeyMap::default();
        let raw = scoped_test_config(
            KeyScope::Normal,
            [("edit_steering", BindingSource::One("ctrl+e".into()))],
        );
        let warnings = map.override_scoped(&raw);
        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
        // the default triggers no longer match
        let alt_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        assert!(!map.matches(Action::EditSteering, alt_k));
        // the override does
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(map.matches(Action::EditSteering, ctrl_e));
    }

    #[test]
    fn scoped_override_accepts_list() {
        let mut map = KeyMap::default();
        let raw = scoped_test_config(
            KeyScope::Normal,
            [(
                "edit_steering",
                BindingSource::Many(vec!["ctrl+e".into(), "f4".into()]),
            )],
        );
        let warnings = map.override_scoped(&raw);
        assert!(warnings.is_empty());
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        let f4 = KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE);
        assert!(map.matches(Action::EditSteering, ctrl_e));
        assert!(map.matches(Action::EditSteering, f4));
    }

    #[test]
    fn scoped_override_only_changes_that_scope() {
        let mut map = KeyMap::default();
        let mut model_picker = HashMap::new();
        model_picker.insert("cancel".to_string(), BindingSource::One("ctrl+x".into()));
        let raw = ScopedKeysConfig {
            scopes: HashMap::from([("model_picker".to_string(), model_picker)]),
        };

        let warnings = map.override_scoped(&raw);

        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
        assert!(map.matches_in(
            KeyScope::ModelPicker,
            Action::Cancel,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches_in(
            KeyScope::Normal,
            Action::Cancel,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        ));
    }

    #[test]
    fn scoped_override_unknown_action_warns_but_does_not_panic() {
        let mut map = KeyMap::default();
        let raw = scoped_test_config(
            KeyScope::Normal,
            [("send_a_cat_emoji", BindingSource::One("ctrl+c".into()))],
        );
        let warnings = map.override_scoped(&raw);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("send_a_cat_emoji"));
    }

    #[test]
    fn scoped_override_invalid_keystring_warns() {
        let mut map = KeyMap::default();
        let raw = scoped_test_config(
            KeyScope::Normal,
            [(
                "edit_steering",
                BindingSource::One("totally-not-a-key".into()),
            )],
        );
        let warnings = map.override_scoped(&raw);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("edit_steering"));
    }

    #[test]
    fn flat_keys_config_is_rejected() {
        let err = toml::from_str::<KeysConfig>(r#"edit_steering = "alt+k""#)
            .expect_err("flat keys config should be rejected");
        assert!(
            err.to_string()
                .contains("old flat [keys] entry `edit_steering`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn scoped_keys_config_deserialises() {
        let keys = toml::from_str::<KeysConfig>(
            r#"
            [normal]
            edit_steering = "alt+k"

            [model_picker]
            delete_selected_stale = "ctrl+d"
            "#,
        )
        .expect("scoped keys should parse");

        assert!(keys.normal.raw.contains_key("edit_steering"));
        assert!(keys.model_picker.raw.contains_key("delete_selected_stale"));
    }

    #[test]
    fn action_config_keys_are_unique_and_snake_case() {
        // each action must have its own key, and the keys should be
        // snake_case so TOML parsing without quotes works
        let mut seen = std::collections::HashSet::new();
        for action in Action::ALL {
            let key = action.config_key();
            assert!(
                seen.insert(key),
                "duplicate config key: {key} for {action:?}"
            );
            assert!(
                key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "config key {key} should be snake_case"
            );
        }
    }
}
