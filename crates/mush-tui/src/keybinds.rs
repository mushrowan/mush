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
use crossterm::event::KeyEvent;
use serde::{Deserialize, Deserializer};

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
    /// pop the last queued steering message back into the input for
    /// editing. default bindings: `alt-k`, `up`, `ctrl-k` (the latter
    /// two only fire when the input is empty)
    EditSteering,
    /// enter fullscreen search mode. default binding: `ctrl-f`
    EnterSearch,
    /// enter scroll / selection mode. default binding: `ctrl-s`
    EnterScroll,
}

impl Action {
    /// toml-key form: the snake_case string users write in config.toml
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::CycleFavourite => "cycle_favourite",
            Self::CycleFavouriteBackward => "cycle_favourite_backward",
            Self::EditSteering => "edit_steering",
            Self::EnterSearch => "enter_search",
            Self::EnterScroll => "enter_scroll",
        }
    }

    /// every variant, for iteration in default population and tests
    pub const ALL: &'static [Self] = &[
        Self::CycleFavourite,
        Self::CycleFavouriteBackward,
        Self::EditSteering,
        Self::EnterSearch,
        Self::EnterScroll,
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
#[derive(Debug, Clone, Deserialize)]
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

/// the full keymap: action → binding
#[derive(Debug, Clone)]
pub struct KeyMap {
    bindings: HashMap<Action, Binding>,
}

impl KeyMap {
    /// lookup the binding for an action. always returns Some for known
    /// actions because defaults are populated at construction time
    #[must_use]
    pub fn binding(&self, action: Action) -> Option<&Binding> {
        self.bindings.get(&action)
    }

    /// does the key event fire the given action?
    #[must_use]
    pub fn matches(&self, action: Action, key: KeyEvent) -> bool {
        self.binding(action).is_some_and(|b| b.matches(key))
    }

    /// apply user overrides on top of the defaults. unknown action names
    /// in `raw` are ignored (returned as warnings for the caller to log)
    pub fn override_from(&mut self, raw: &HashMap<String, BindingSource>) -> Vec<String> {
        let mut warnings = Vec::new();
        for (key, src) in raw {
            let Some(action) = Action::ALL.iter().copied().find(|a| a.config_key() == key) else {
                warnings.push(format!("unknown keybind action: {key}"));
                continue;
            };
            match src.clone().into_binding() {
                Ok(binding) => {
                    self.bindings.insert(action, binding);
                }
                Err(e) => warnings.push(format!("invalid keybind for {key}: {e}")),
            }
        }
        warnings
    }
}

impl Default for KeyMap {
    fn default() -> Self {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut bindings: HashMap<Action, Binding> = HashMap::new();

        // edit_steering: alt-k always, plus up / ctrl-k as aliases for
        // when the input is empty (the empty check lives at the call
        // site, not in the binding itself)
        bindings.insert(
            Action::EditSteering,
            Binding(vec![
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::ALT),
                KeyCombination::new(KeyCode::Up, KeyModifiers::NONE),
                KeyCombination::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            ]),
        );

        bindings.insert(
            Action::CycleFavourite,
            Binding::single(KeyCombination::new(KeyCode::Char('m'), KeyModifiers::ALT)),
        );

        // note: crossterm delivers shift-m as KeyCode::Char('M') with
        // the ALT modifier set (no explicit SHIFT), so that's how the
        // binding must be declared to match incoming events
        bindings.insert(
            Action::CycleFavouriteBackward,
            Binding::single(KeyCombination::new(KeyCode::Char('M'), KeyModifiers::ALT)),
        );

        bindings.insert(
            Action::EnterSearch,
            Binding::single(KeyCombination::new(
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
            )),
        );

        bindings.insert(
            Action::EnterScroll,
            Binding::single(KeyCombination::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            )),
        );

        Self { bindings }
    }
}

/// config-side type used by `KeysConfig` below. users put these in
/// `[keys]` in config.toml. `deserialise_keys` turns the toml table
/// into raw strings so `KeyMap::override_from` can layer them on top
/// of the defaults without losing context on which actions are known
#[derive(Debug, Default, Clone)]
pub struct KeysConfig {
    pub raw: HashMap<String, BindingSource>,
}

impl<'de> Deserialize<'de> for KeysConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = HashMap::<String, BindingSource>::deserialize(d)?;
        Ok(Self { raw })
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
    fn default_keymap_has_mode_switch_triggers() {
        let map = KeyMap::default();
        let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(map.matches(Action::EnterSearch, ctrl_f));
        assert!(map.matches(Action::EnterScroll, ctrl_s));
    }

    #[test]
    fn override_replaces_default() {
        let mut map = KeyMap::default();
        let raw: HashMap<String, BindingSource> = [(
            "edit_steering".to_string(),
            BindingSource::One("ctrl+e".into()),
        )]
        .into_iter()
        .collect();
        let warnings = map.override_from(&raw);
        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
        // the default triggers no longer match
        let alt_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT);
        assert!(!map.matches(Action::EditSteering, alt_k));
        // the override does
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(map.matches(Action::EditSteering, ctrl_e));
    }

    #[test]
    fn override_accepts_list() {
        let mut map = KeyMap::default();
        let raw: HashMap<String, BindingSource> = [(
            "edit_steering".to_string(),
            BindingSource::Many(vec!["ctrl+e".into(), "f4".into()]),
        )]
        .into_iter()
        .collect();
        let warnings = map.override_from(&raw);
        assert!(warnings.is_empty());
        let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        let f4 = KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE);
        assert!(map.matches(Action::EditSteering, ctrl_e));
        assert!(map.matches(Action::EditSteering, f4));
    }

    #[test]
    fn override_unknown_action_warns_but_does_not_panic() {
        let mut map = KeyMap::default();
        let raw: HashMap<String, BindingSource> = [(
            "send_a_cat_emoji".to_string(),
            BindingSource::One("ctrl+c".into()),
        )]
        .into_iter()
        .collect();
        let warnings = map.override_from(&raw);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("send_a_cat_emoji"));
    }

    #[test]
    fn override_invalid_keystring_warns() {
        let mut map = KeyMap::default();
        let raw: HashMap<String, BindingSource> = [(
            "edit_steering".to_string(),
            BindingSource::One("totally-not-a-key".into()),
        )]
        .into_iter()
        .collect();
        let warnings = map.override_from(&raw);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("edit_steering"));
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
