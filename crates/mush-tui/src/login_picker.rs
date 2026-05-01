//! login picker state and filtering helpers
//!
//! mirrors [`crate::session_picker`] / [`crate::model_picker`]: a centred
//! floating window activated by `/login` with no arg. lists every login
//! provider mush knows about (oauth + api-key based), with a source
//! badge per row so users can see where the active credential lives.
//! enter starts a flow (oauth handshake or api-key entry) when logged
//! out, or arms a y/n logout confirm when the row is mush-managed
//! (oauth credentials or stored api key). env / config sourced rows
//! refuse logout with a toast pointing at the source

use std::cell::RefCell;
use std::collections::HashMap;

use crate::fuzzy::FuzzyFilter;
use mush_ai::credentials::default_store;
use mush_ai::login::{self, LoginMethod, LoginSource, SourceInputs};
use mush_ai::types::ApiKey;

/// one row in the picker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginEntry {
    /// stable picker row id, matches `mush_ai::login::LoginProvider::id`
    pub id: String,
    /// human-readable name shown in the row (e.g. "OpenRouter")
    pub name: String,
    /// auth method this row uses (oauth flow vs api-key paste)
    pub method: LoginMethod,
    /// where the credential currently comes from, or `None` for
    /// logged-out rows. drives the badge column and logout policy
    pub source: Option<LoginSource>,
    /// optional account id shown next to the row when the underlying
    /// credential carries one (anthropic surfaces this for paid plans)
    pub account_id: Option<String>,
}

impl LoginEntry {
    /// shorthand for "any credential is present"
    #[must_use]
    pub fn logged_in(&self) -> bool {
        self.source.is_some()
    }
}

/// state for the centred login picker overlay
#[derive(Debug)]
pub struct LoginPickerState {
    pub entries: Vec<LoginEntry>,
    pub filter: String,
    pub selected: usize,
    /// short transient message shown in the picker footer
    pub toast: Option<String>,
    /// when `Some`, the picker shows a y/n confirmation for logging out
    /// of the row with this id. enter is disabled while armed
    pub confirm_logout: Option<String>,
    /// when `Some`, the picker is collecting an api-key paste for the
    /// row with this id. typing builds up `entry.buffer` (rendered as
    /// `•` characters), enter saves, esc cancels
    pub entry: Option<EntryPrompt>,
    /// snapshot of `[api_keys]` from the user's config.toml taken at
    /// picker open time. used by [`Self::rebuild_sources`] to re-detect
    /// `Config` source rows after a login/logout without re-plumbing
    /// the runner. config edits made during a picker session aren't
    /// reflected, which is fine because the picker is short-lived
    pub config_keys: HashMap<String, ApiKey>,
    /// nucleo matcher reused across renders. `RefCell` because
    /// `filtered_entries` takes `&self` for read-only paths but the
    /// matcher needs `&mut`
    matcher: RefCell<FuzzyFilter>,
}

/// in-progress api-key entry. lives on the picker so the widget can
/// render the prompt in place of the filter row, and the input handler
/// can route keystrokes into the buffer
#[derive(Debug, Clone)]
pub struct EntryPrompt {
    /// row id we're entering a key for
    pub entry_id: String,
    /// display name of the row, for the prompt label
    pub provider_name: String,
    /// typed characters so far. rendered masked with `•`
    pub buffer: String,
}

impl LoginPickerState {
    #[must_use]
    pub fn new(entries: Vec<LoginEntry>) -> Self {
        Self::with_config_keys(entries, HashMap::new())
    }

    #[must_use]
    pub fn with_config_keys(
        entries: Vec<LoginEntry>,
        config_keys: HashMap<String, ApiKey>,
    ) -> Self {
        Self {
            entries,
            filter: String::new(),
            selected: 0,
            toast: None,
            confirm_logout: None,
            entry: None,
            config_keys,
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    /// re-resolve every row's source by reading the current oauth
    /// store, env vars, snapshotted config keys, and credential store.
    /// called after the picker mutates auth state (login or logout) so
    /// the row repaints without a full rebuild from the runner
    pub fn rebuild_sources(&mut self) {
        let oauth_store = mush_ai::oauth::load_credentials().unwrap_or_default();
        let oauth_present: Vec<(String, Option<String>)> = oauth_store
            .providers
            .iter()
            .map(|(id, c)| (id.clone(), c.account_id.clone()))
            .collect();
        let credential_store = default_store();
        let inputs = SourceInputs {
            oauth_present: &oauth_present,
            config_keys: &self.config_keys,
            credential_store: credential_store.as_ref(),
        };
        for entry in &mut self.entries {
            // adapt to login::LoginProvider shape just to call
            // resolve_source. cheap clones on the small hot path
            let provider = login::LoginProvider {
                id: entry.id.clone(),
                name: entry.name.clone(),
                method: entry.method.clone(),
            };
            entry.source = login::resolve_source(&provider, &inputs);
            entry.account_id = match &entry.method {
                LoginMethod::OAuth { oauth_provider_id } if entry.source.is_some() => {
                    login::oauth_account_id(oauth_provider_id)
                }
                _ => None,
            };
        }
    }

    /// arm a logout confirmation for the highlighted row, returning
    /// whether the prompt was armed. logged-out rows skip arming;
    /// env / config sourced rows show a toast pointing at the source
    /// instead since mush doesn't manage those
    pub fn arm_logout(&mut self) -> bool {
        let Some(entry) = self.selected_entry().cloned() else {
            return false;
        };
        let Some(source) = entry.source.as_ref() else {
            self.toast = Some("not logged in".into());
            return false;
        };
        if !source.is_picker_managed() {
            self.toast = Some(match source {
                LoginSource::Env(var) => format!("set via ${var} - unset that to remove"),
                LoginSource::Config => "set in config.toml - edit there to remove".into(),
                _ => "this source isn't managed by the picker".into(),
            });
            return false;
        }
        self.toast = Some(format!("log out of {}? y/n", entry.name));
        self.confirm_logout = Some(entry.id);
        true
    }

    pub fn cancel_logout(&mut self) {
        self.confirm_logout = None;
        self.toast = None;
    }

    /// arm an api-key entry prompt for the highlighted row. only
    /// makes sense for `ApiKey` rows; oauth rows return `false`
    /// without changing state so the caller can keep its own routing
    pub fn arm_entry(&mut self) -> bool {
        let Some(entry) = self.selected_entry() else {
            return false;
        };
        if !matches!(entry.method, LoginMethod::ApiKey { .. }) {
            return false;
        }
        self.entry = Some(EntryPrompt {
            entry_id: entry.id.clone(),
            provider_name: entry.name.clone(),
            buffer: String::new(),
        });
        self.toast = None;
        true
    }

    pub fn cancel_entry(&mut self) {
        self.entry = None;
        self.toast = None;
    }

    /// snapshot of the row currently highlighted, after filtering
    fn selected_entry(&self) -> Option<&LoginEntry> {
        let visible = filtered_entries(self);
        visible.get(self.selected).copied()
    }

    /// rebuild source flags from a fresh snapshot. callers pass the
    /// new entries so we can pick up the latest sources without
    /// rebuilding the picker state from scratch
    pub fn refresh_entries(&mut self, entries: Vec<LoginEntry>) {
        self.entries = entries;
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }
}

/// entries matching the current filter, in render order. empty filter
/// keeps the natural catalogue order; otherwise fuzzy-rank by
/// `id name account` so a typed query can find rows by either label
#[must_use]
pub fn filtered_entries(picker: &LoginPickerState) -> Vec<&LoginEntry> {
    if picker.filter.is_empty() {
        return picker.entries.iter().collect();
    }
    let haystacks: Vec<String> = picker
        .entries
        .iter()
        .map(|e| {
            let acc = e.account_id.as_deref().unwrap_or("");
            format!("{} {} {}", e.id, e.name, acc)
        })
        .collect();
    let mut matcher = picker.matcher.borrow_mut();
    let indices = matcher.filter(&haystacks, &picker.filter, String::as_str);
    indices.into_iter().map(|i| &picker.entries[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth_entry(id: &str, name: &str, source: Option<LoginSource>) -> LoginEntry {
        LoginEntry {
            id: id.into(),
            name: name.into(),
            method: LoginMethod::OAuth {
                oauth_provider_id: id.into(),
            },
            source,
            account_id: None,
        }
    }

    fn api_key_entry(id: &str, name: &str, source: Option<LoginSource>) -> LoginEntry {
        LoginEntry {
            id: id.into(),
            name: name.into(),
            method: LoginMethod::ApiKey {
                storage_key: id.into(),
                env_var: format!("{}_API_KEY", id.to_uppercase().replace('-', "_")),
                config_key: id.into(),
            },
            source,
            account_id: None,
        }
    }

    #[test]
    fn new_picker_starts_with_first_row_selected_and_empty_filter() {
        let picker = LoginPickerState::new(vec![
            oauth_entry("anthropic-pro-max", "Anthropic", Some(LoginSource::OAuth)),
            api_key_entry("openrouter", "OpenRouter", None),
        ]);
        assert_eq!(picker.selected, 0);
        assert!(picker.filter.is_empty());
        assert!(picker.confirm_logout.is_none());
        assert!(picker.entry.is_none());
    }

    #[test]
    fn empty_filter_returns_input_order() {
        let picker = LoginPickerState::new(vec![
            oauth_entry("anthropic-pro-max", "Anthropic", Some(LoginSource::OAuth)),
            api_key_entry("openrouter", "OpenRouter", None),
        ]);
        let ids: Vec<&str> = filtered_entries(&picker)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["anthropic-pro-max", "openrouter"]);
    }

    #[test]
    fn fuzzy_filter_matches_name_or_id() {
        let mut picker = LoginPickerState::new(vec![
            oauth_entry(
                "anthropic-pro-max",
                "Anthropic (Claude Pro/Max)",
                Some(LoginSource::OAuth),
            ),
            api_key_entry("openrouter", "OpenRouter", None),
        ]);
        picker.filter = "openroute".into();
        let ids: Vec<&str> = filtered_entries(&picker)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["openrouter"]);
    }

    #[test]
    fn arm_logout_only_arms_for_logged_in_picker_managed_rows() {
        let mut picker = LoginPickerState::new(vec![
            oauth_entry("anthropic-pro-max", "Anthropic", Some(LoginSource::OAuth)),
            api_key_entry("openrouter", "OpenRouter", None),
        ]);
        picker.selected = 1;
        assert!(!picker.arm_logout(), "logged-out row must not arm");
        assert!(picker.confirm_logout.is_none());
        assert!(
            picker.toast.as_deref().unwrap().contains("not logged in"),
            "toast should explain the no-op"
        );

        picker.selected = 0;
        assert!(picker.arm_logout(), "logged-in oauth row should arm");
        assert_eq!(picker.confirm_logout.as_deref(), Some("anthropic-pro-max"));
        assert!(picker.toast.as_deref().unwrap().contains("Anthropic"));
    }

    #[test]
    fn arm_logout_refuses_env_sourced_rows_with_helpful_toast() {
        // mush doesn't own env vars, so the picker can't "log out" of
        // them. it must point the user at the source instead of
        // silently doing nothing
        let mut picker = LoginPickerState::new(vec![api_key_entry(
            "openrouter",
            "OpenRouter",
            Some(LoginSource::Env("OPENROUTER_API_KEY".into())),
        )]);
        assert!(!picker.arm_logout());
        assert!(picker.confirm_logout.is_none());
        assert!(
            picker
                .toast
                .as_deref()
                .unwrap()
                .contains("OPENROUTER_API_KEY"),
            "toast should name the env var so the user knows what to unset"
        );
    }

    #[test]
    fn arm_logout_refuses_config_sourced_rows_with_helpful_toast() {
        let mut picker = LoginPickerState::new(vec![api_key_entry(
            "openrouter",
            "OpenRouter",
            Some(LoginSource::Config),
        )]);
        assert!(!picker.arm_logout());
        assert!(
            picker.toast.as_deref().unwrap().contains("config.toml"),
            "toast should mention config.toml so the user knows where to look"
        );
    }

    #[test]
    fn arm_entry_arms_for_api_key_rows_only() {
        // entering an api key only makes sense for ApiKey rows. oauth
        // rows go through the browser flow on enter instead
        let mut picker = LoginPickerState::new(vec![
            oauth_entry("anthropic-pro-max", "Anthropic", None),
            api_key_entry("openrouter", "OpenRouter", None),
        ]);
        picker.selected = 0;
        assert!(!picker.arm_entry(), "oauth row must not arm key entry");
        assert!(picker.entry.is_none());

        picker.selected = 1;
        assert!(picker.arm_entry(), "api-key row should arm key entry");
        let prompt = picker.entry.as_ref().unwrap();
        assert_eq!(prompt.entry_id, "openrouter");
        assert!(prompt.buffer.is_empty());
    }

    #[test]
    fn cancel_entry_clears_prompt_and_toast() {
        let mut picker =
            LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter", None)]);
        picker.arm_entry();
        picker.toast = Some("typed".into());
        picker.cancel_entry();
        assert!(picker.entry.is_none());
        assert!(picker.toast.is_none());
    }

    #[test]
    fn refresh_entries_clamps_selection_when_list_shrinks() {
        let mut picker = LoginPickerState::new(vec![
            api_key_entry("a", "A", None),
            api_key_entry("b", "B", None),
            api_key_entry("c", "C", None),
        ]);
        picker.selected = 2;
        picker.refresh_entries(vec![api_key_entry("a", "A", None)]);
        assert_eq!(picker.selected, 0, "must clamp into the new range");
    }

    #[test]
    fn logged_in_helper_reflects_source_presence() {
        assert!(oauth_entry("a", "A", Some(LoginSource::OAuth)).logged_in());
        assert!(!api_key_entry("a", "A", None).logged_in());
    }
}
