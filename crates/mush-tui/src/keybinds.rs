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
    Session,
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
            Self::Session => "pane",
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
        Self::Session,
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
        self.binding_in_exact(scope, action).or_else(|| {
            if scope == KeyScope::Shared {
                None
            } else {
                self.binding_in_exact(KeyScope::Shared, action)
            }
        })
    }

    /// binding registered for exactly this scope, with no fallback to
    /// `Shared`. only the defaults dump and its guard need this
    #[must_use]
    pub(crate) fn binding_in_exact(&self, scope: KeyScope, action: Action) -> Option<&Binding> {
        self.bindings
            .get(&scope)
            .and_then(|bindings| bindings.get(&action))
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

/// one key combination in the defaults table
type DefaultCombo = (KeyCode, KeyModifiers);

/// one row of the defaults table: which action fires in which scope, and
/// on which combinations
type DefaultBinding = (KeyScope, Action, &'static [DefaultCombo]);

/// the default keybindings, as data. the whole table is walked once at
/// construction, so adding a binding means adding a row
const DEFAULT_BINDINGS: &[DefaultBinding] = &[
    // edit_steering: alt-k always, plus up / ctrl-k as aliases for when
    // the input is empty (the empty check lives at the call site, not in
    // the binding itself)
    (
        KeyScope::Normal,
        Action::EditSteering,
        &[
            (KeyCode::Char('k'), KeyModifiers::ALT),
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Shared,
        Action::Cancel,
        &[
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('['), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Normal,
        Action::CycleFavourite,
        &[(KeyCode::Char('m'), KeyModifiers::ALT)],
    ),
    // crossterm delivers shift-m as KeyCode::Char('M') with the ALT
    // modifier set (no explicit SHIFT), so that's how the binding must be
    // declared to match incoming events
    (
        KeyScope::Normal,
        Action::CycleFavouriteBackward,
        &[(KeyCode::Char('M'), KeyModifiers::ALT)],
    ),
    (
        KeyScope::Normal,
        Action::EnterSearch,
        &[(KeyCode::Char('f'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Normal,
        Action::EnterScroll,
        &[(KeyCode::Char('s'), KeyModifiers::CONTROL)],
    ),
    // shift+End: jump the message log to the bottom without touching the
    // stream. plain End is line-end in the input editor; ctrl+End isn't
    // reliably delivered across terminals
    (
        KeyScope::Shared,
        Action::JumpToBottom,
        &[(KeyCode::End, KeyModifiers::SHIFT)],
    ),
    (
        KeyScope::Shared,
        Action::QuitOrAbort,
        &[(KeyCode::Char('c'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Shared,
        Action::ScrollPageUp,
        &[(KeyCode::PageUp, KeyModifiers::NONE)],
    ),
    (
        KeyScope::Shared,
        Action::ScrollPageDown,
        &[(KeyCode::PageDown, KeyModifiers::NONE)],
    ),
    (
        KeyScope::Shared,
        Action::Confirm,
        &[(KeyCode::Enter, KeyModifiers::NONE)],
    ),
    (
        KeyScope::Shared,
        Action::SelectOrSpace,
        &[
            (KeyCode::Enter, KeyModifiers::NONE),
            (KeyCode::Char(' '), KeyModifiers::NONE),
        ],
    ),
    (
        KeyScope::Shared,
        Action::NextItem,
        &[
            (KeyCode::Tab, KeyModifiers::NONE),
            (KeyCode::Down, KeyModifiers::NONE),
            (KeyCode::Char('j'), KeyModifiers::NONE),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
            (KeyCode::Char('n'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Shared,
        Action::PreviousItem,
        &[
            (KeyCode::BackTab, KeyModifiers::NONE),
            (KeyCode::BackTab, KeyModifiers::SHIFT),
            (KeyCode::Tab, KeyModifiers::SHIFT),
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Char('k'), KeyModifiers::NONE),
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
            (KeyCode::Char('p'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::SessionPicker,
        Action::NextItem,
        &[
            (KeyCode::Down, KeyModifiers::NONE),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::SessionPicker,
        Action::PreviousItem,
        &[
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::SessionPicker,
        Action::SessionPickerToggleScope,
        &[(KeyCode::Tab, KeyModifiers::NONE)],
    ),
    (
        KeyScope::ModelPicker,
        Action::NextItem,
        &[
            (KeyCode::Down, KeyModifiers::NONE),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::ModelPicker,
        Action::PreviousItem,
        &[
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Shared,
        Action::NavigateTop,
        &[
            (KeyCode::Home, KeyModifiers::NONE),
            (KeyCode::Char('g'), KeyModifiers::NONE),
        ],
    ),
    (
        KeyScope::Shared,
        Action::NavigateBottom,
        &[
            (KeyCode::End, KeyModifiers::NONE),
            (KeyCode::Char('G'), KeyModifiers::SHIFT),
        ],
    ),
    (
        KeyScope::Scroll,
        Action::ToggleBlockMode,
        &[(KeyCode::Char('b'), KeyModifiers::NONE)],
    ),
    (
        KeyScope::Scroll,
        Action::ToggleVisualSelection,
        &[(KeyCode::Char('v'), KeyModifiers::NONE)],
    ),
    (
        KeyScope::Scroll,
        Action::CopySelection,
        &[(KeyCode::Char('y'), KeyModifiers::NONE)],
    ),
    (
        KeyScope::Normal,
        Action::CycleThinkingLevel,
        &[(KeyCode::Char('t'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Normal,
        Action::PasteImage,
        &[(KeyCode::Char('v'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Edit,
        Action::DeleteWordBackward,
        &[
            (KeyCode::Backspace, KeyModifiers::CONTROL),
            (KeyCode::Backspace, KeyModifiers::ALT),
            (KeyCode::Char('w'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Edit,
        Action::DeleteWordForward,
        &[(KeyCode::Char('d'), KeyModifiers::ALT)],
    ),
    (
        KeyScope::Edit,
        Action::DeleteCharOrQuit,
        &[(KeyCode::Char('d'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Edit,
        Action::DeleteToStart,
        &[(KeyCode::Char('u'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Edit,
        Action::DeleteToEnd,
        &[(KeyCode::Char('k'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Edit,
        Action::CursorWordLeft,
        &[
            (KeyCode::Left, KeyModifiers::ALT),
            (KeyCode::Left, KeyModifiers::CONTROL),
            (KeyCode::Char('b'), KeyModifiers::ALT),
        ],
    ),
    (
        KeyScope::Edit,
        Action::CursorLeft,
        &[
            (KeyCode::Left, KeyModifiers::NONE),
            (KeyCode::Char('b'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Edit,
        Action::CursorRight,
        &[(KeyCode::Right, KeyModifiers::NONE)],
    ),
    (
        KeyScope::Edit,
        Action::CursorWordRight,
        &[
            (KeyCode::Right, KeyModifiers::ALT),
            (KeyCode::Right, KeyModifiers::CONTROL),
            (KeyCode::Char('f'), KeyModifiers::ALT),
        ],
    ),
    (
        KeyScope::Edit,
        Action::CursorLineStart,
        &[
            (KeyCode::Home, KeyModifiers::NONE),
            (KeyCode::Char('a'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Edit,
        Action::CursorLineEnd,
        &[
            (KeyCode::End, KeyModifiers::NONE),
            (KeyCode::Char('e'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::Normal,
        Action::InsertNewline,
        &[
            (KeyCode::Enter, KeyModifiers::ALT),
            (KeyCode::Enter, KeyModifiers::SHIFT),
            (KeyCode::Char('j'), KeyModifiers::CONTROL),
        ],
    ),
    (
        KeyScope::ModelPicker,
        Action::ModelPickerToggleFavourite,
        &[(KeyCode::Char('f'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::ModelPicker,
        Action::ModelPickerDeleteSelectedStale,
        &[(KeyCode::Char('d'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::ModelPicker,
        Action::ModelPickerDeleteAllStale,
        &[
            (
                KeyCode::Char('D'),
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            ),
            (KeyCode::Char('d'), KeyModifiers::ALT),
        ],
    ),
];

impl Default for KeyMap {
    fn default() -> Self {
        let mut bindings: HashMap<KeyScope, HashMap<Action, Binding>> = HashMap::new();
        let mut bind = |scope: KeyScope, action: Action, combos: Vec<KeyCombination>| {
            bindings
                .entry(scope)
                .or_default()
                .insert(action, Binding(combos));
        };

        for (scope, action, combos) in DEFAULT_BINDINGS {
            let combos = combos
                .iter()
                .map(|(code, modifiers)| KeyCombination::new(*code, *modifiers))
                .collect();
            bind(*scope, *action, combos);
        }

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
            KeyScope::Session.config_key().to_string(),
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

#[cfg(test)]
mod default_table_snapshot {
    use super::*;

    /// every default binding, flattened and sorted, as one string.
    /// used as a characterisation guard when the defaults table changes
    /// shape: the dump has to stay byte-identical
    fn dump(map: &KeyMap) -> String {
        let mut rows: Vec<String> = Vec::new();
        for scope in KeyScope::ALL {
            for action in Action::ALL {
                if let Some(binding) = map.binding_in_exact(*scope, *action) {
                    let combos: Vec<String> = binding
                        .0
                        .iter()
                        .map(|c| c.normalized().to_string())
                        .collect();
                    rows.push(format!(
                        "{}::{} = {}",
                        scope.config_key(),
                        action.config_key(),
                        combos.join(" | ")
                    ));
                }
            }
        }
        rows.sort();
        rows.join("\n")
    }

    #[test]
    fn default_bindings_are_unchanged() {
        let dump = dump(&KeyMap::default());
        let expected = include_str!("snapshots/default_keybindings.txt");
        assert_eq!(
            dump.trim(),
            expected.trim(),
            "default bindings changed. if deliberate, regenerate \
             src/snapshots/default_keybindings.txt from this dump"
        );
    }
}
