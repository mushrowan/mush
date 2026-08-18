use std::collections::HashMap;
use std::str::FromStr;

use crokey::KeyCombination;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Deserializer};

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
    Navigate,
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
            Self::Navigate => "navigate",
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
        Self::Navigate,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    CycleFavourite,
    CycleFavouriteBackward,
    Cancel,
    CycleThinkingLevel,
    Confirm,
    CursorDown,
    CursorLineEnd,
    CursorLineStart,
    CursorLeft,
    CursorRight,
    CursorWordLeft,
    CursorWordRight,
    CursorUp,
    EditSteering,
    EnterSearch,
    FocusLog,
    PasteImage,
    QuitOrAbort,
    SelectAll,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteChar,
    DeleteToEnd,
    DeleteToStart,
    InsertNewline,
    NavigateBottom,
    NavigateTop,
    NextItem,
    PreviousItem,
    SessionPickerToggleScope,
    ModelPickerDeleteAllStale,
    ModelPickerDeleteSelectedStale,
    ModelPickerToggleFavourite,
    HalfPageDown,
    LineDown,
    LineUp,
    HalfPageUp,
    PageDown,
    PageUp,
    SearchNext,
    SearchPrev,
    SelectOrSpace,
    StartSelection,
    Yank,
}

impl Action {
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::CycleFavourite => "cycle_favourite",
            Self::CycleFavouriteBackward => "cycle_favourite_backward",
            Self::Cancel => "cancel",
            Self::CycleThinkingLevel => "cycle_thinking_level",
            Self::Confirm => "confirm",
            Self::CursorDown => "cursor_down",
            Self::CursorLineEnd => "cursor_line_end",
            Self::CursorLineStart => "cursor_line_start",
            Self::CursorLeft => "cursor_left",
            Self::CursorRight => "cursor_right",
            Self::CursorWordLeft => "cursor_word_left",
            Self::CursorWordRight => "cursor_word_right",
            Self::CursorUp => "cursor_up",
            Self::DeleteChar => "delete_char",
            Self::DeleteToEnd => "delete_to_end",
            Self::DeleteToStart => "delete_to_start",
            Self::EditSteering => "edit_steering",
            Self::EnterSearch => "enter_search",
            Self::FocusLog => "focus_log",
            Self::InsertNewline => "insert_newline",
            Self::NavigateBottom => "navigate_bottom",
            Self::NavigateTop => "navigate_top",
            Self::NextItem => "next_item",
            Self::PreviousItem => "previous_item",
            Self::SessionPickerToggleScope => "session_picker_toggle_scope",
            Self::ModelPickerDeleteAllStale => "model_picker_delete_all_stale",
            Self::ModelPickerDeleteSelectedStale => "model_picker_delete_selected_stale",
            Self::ModelPickerToggleFavourite => "model_picker_toggle_favourite",
            Self::HalfPageDown => "half_page_down",
            Self::LineDown => "line_down",
            Self::LineUp => "line_up",
            Self::HalfPageUp => "half_page_up",
            Self::PageDown => "page_down",
            Self::PageUp => "page_up",
            Self::SearchNext => "search_next",
            Self::SearchPrev => "search_prev",
            Self::SelectOrSpace => "select_or_space",
            Self::StartSelection => "start_selection",
            Self::Yank => "yank",
            Self::PasteImage => "paste_image",
            Self::QuitOrAbort => "quit_or_abort",
            Self::SelectAll => "select_all",
            Self::DeleteWordBackward => "delete_word_backward",
            Self::DeleteWordForward => "delete_word_forward",
        }
    }

    pub const ALL: &'static [Self] = &[
        Self::CycleFavourite,
        Self::CycleFavouriteBackward,
        Self::Cancel,
        Self::CycleThinkingLevel,
        Self::Confirm,
        Self::CursorDown,
        Self::CursorLineEnd,
        Self::CursorLineStart,
        Self::CursorLeft,
        Self::CursorRight,
        Self::CursorWordLeft,
        Self::CursorWordRight,
        Self::CursorUp,
        Self::DeleteChar,
        Self::DeleteToEnd,
        Self::DeleteToStart,
        Self::EditSteering,
        Self::EnterSearch,
        Self::FocusLog,
        Self::InsertNewline,
        Self::NavigateBottom,
        Self::NavigateTop,
        Self::NextItem,
        Self::PreviousItem,
        Self::SessionPickerToggleScope,
        Self::ModelPickerDeleteAllStale,
        Self::ModelPickerDeleteSelectedStale,
        Self::ModelPickerToggleFavourite,
        Self::HalfPageDown,
        Self::LineDown,
        Self::LineUp,
        Self::HalfPageUp,
        Self::PageDown,
        Self::PageUp,
        Self::SearchNext,
        Self::SearchPrev,
        Self::SelectOrSpace,
        Self::StartSelection,
        Self::Yank,
        Self::PasteImage,
        Self::QuitOrAbort,
        Self::SelectAll,
        Self::DeleteWordBackward,
        Self::DeleteWordForward,
    ];
}

#[derive(Debug, Clone, Default)]
pub struct Binding(pub Vec<KeyCombination>);

impl Binding {
    #[must_use]
    pub fn single(combo: KeyCombination) -> Self {
        Self(vec![combo])
    }

    pub fn parse_one(raw: &str) -> Result<KeyCombination, crokey::ParseKeyError> {
        let normalised = raw.replace('+', "-");
        crokey::parse(&normalised)
    }

    #[must_use]
    pub fn matches(&self, key: KeyEvent) -> bool {
        let combo = KeyCombination::from(key).normalized();
        self.0.iter().any(|b| b.normalized() == combo)
    }
}

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

#[derive(Debug, Clone)]
pub struct KeyMap {
    bindings: HashMap<KeyScope, HashMap<Action, Binding>>,
}

impl KeyMap {
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

    #[must_use]
    pub(crate) fn binding_in_exact(&self, scope: KeyScope, action: Action) -> Option<&Binding> {
        self.bindings
            .get(&scope)
            .and_then(|bindings| bindings.get(&action))
    }

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
        | Action::FocusLog
        | Action::NavigateBottom
        | Action::NavigateTop
        | Action::NextItem
        | Action::PreviousItem
        | Action::QuitOrAbort
        | Action::SelectOrSpace => KeyScope::Shared,
        Action::HalfPageDown
        | Action::HalfPageUp
        | Action::LineDown
        | Action::LineUp
        | Action::PageDown
        | Action::PageUp
        | Action::StartSelection
        | Action::Yank => KeyScope::Navigate,
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
        | Action::DeleteChar
        | Action::DeleteToEnd
        | Action::DeleteToStart
        | Action::DeleteWordBackward
        | Action::DeleteWordForward => KeyScope::Edit,
        _ => KeyScope::Normal,
    }
}

type DefaultCombo = (KeyCode, KeyModifiers);

type DefaultBinding = (KeyScope, Action, &'static [DefaultCombo]);

const DEFAULT_BINDINGS: &[DefaultBinding] = &[
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
        Action::SelectAll,
        &[(KeyCode::Char('a'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Shared,
        Action::FocusLog,
        &[(KeyCode::Char('o'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Shared,
        Action::QuitOrAbort,
        &[(KeyCode::Char('c'), KeyModifiers::CONTROL)],
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
        KeyScope::Navigate,
        Action::StartSelection,
        &[(KeyCode::Char('v'), KeyModifiers::NONE)],
    ),
    (
        KeyScope::Navigate,
        Action::Yank,
        &[(KeyCode::Char('y'), KeyModifiers::NONE)],
    ),
    (
        KeyScope::Navigate,
        Action::LineDown,
        &[(KeyCode::Char('j'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Navigate,
        Action::LineUp,
        &[(KeyCode::Char('k'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Navigate,
        Action::CursorLeft,
        &[
            (KeyCode::Char('h'), KeyModifiers::NONE),
            (KeyCode::Left, KeyModifiers::NONE),
        ],
    ),
    (
        KeyScope::Navigate,
        Action::CursorRight,
        &[
            (KeyCode::Char('l'), KeyModifiers::NONE),
            (KeyCode::Right, KeyModifiers::NONE),
        ],
    ),
    (
        KeyScope::Navigate,
        Action::HalfPageDown,
        &[(KeyCode::Char('d'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Navigate,
        Action::HalfPageUp,
        &[(KeyCode::Char('u'), KeyModifiers::CONTROL)],
    ),
    (
        KeyScope::Navigate,
        Action::PageDown,
        &[
            (KeyCode::Char('f'), KeyModifiers::CONTROL),
            (KeyCode::PageDown, KeyModifiers::NONE),
        ],
    ),
    (
        KeyScope::Navigate,
        Action::PageUp,
        &[
            (KeyCode::Char('b'), KeyModifiers::CONTROL),
            (KeyCode::PageUp, KeyModifiers::NONE),
        ],
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
        Action::DeleteChar,
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
        &[(KeyCode::Home, KeyModifiers::NONE)],
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
        assert!(!map.matches(Action::CycleFavourite, alt_shift_m));
        assert!(!map.matches(Action::CycleFavouriteBackward, alt_m));
    }

    #[test]
    fn default_keymap_has_toggle_actions() {
        let map = KeyMap::default();
        assert!(map.matches(
            Action::CycleThinkingLevel,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)
        ));
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
            Action::DeleteChar,
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
    fn scoped_override_replaces_default() {
        let mut map = KeyMap::default();
        let raw = scoped_test_config(
            KeyScope::Normal,
            [("edit_steering", BindingSource::One("ctrl+e".into()))],
        );
        let warnings = map.override_scoped(&raw);
        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
        let alt_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        assert!(!map.matches(Action::EditSteering, alt_k));
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
