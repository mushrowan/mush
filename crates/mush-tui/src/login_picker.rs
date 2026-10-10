use std::cell::RefCell;
use std::collections::HashMap;

use crate::fuzzy::FuzzyFilter;
use mush_ai::credentials::default_store;
use mush_ai::login::{self, LoginMethod, LoginSource, SourceInputs};
use mush_ai::types::ApiKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginEntry {
    pub id: String,
    pub name: String,
    pub method: LoginMethod,
    pub source: Option<LoginSource>,
    pub account_id: Option<String>,
}

impl LoginEntry {
    #[must_use]
    pub fn logged_in(&self) -> bool {
        self.source.is_some()
    }
}

#[derive(Debug)]
pub struct LoginPickerState {
    pub entries: Vec<LoginEntry>,
    pub filter: String,
    pub selected: usize,
    pub toast: Option<String>,
    pub confirm_logout: Option<String>,
    pub entry: Option<EntryPrompt>,
    pub validating: Option<String>,
    pub config_keys: HashMap<String, ApiKey>,
    matcher: RefCell<FuzzyFilter>,
}

#[derive(Debug, Clone)]
pub struct EntryPrompt {
    pub entry_id: String,
    pub provider_name: String,
    pub buffer: String,
    pub error_message: Option<String>,
}

impl EntryPrompt {
    fn new(entry_id: &str, provider_name: &str, error_message: Option<String>) -> Self {
        Self {
            entry_id: entry_id.to_string(),
            provider_name: provider_name.to_string(),
            buffer: String::new(),
            error_message,
        }
    }
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
            validating: None,
            config_keys,
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    pub fn rebuild_sources(&mut self) {
        let oauth_present = login::oauth_logins();
        let credential_store = default_store();
        let inputs = SourceInputs {
            oauth_present: &oauth_present,
            config_keys: &self.config_keys,
            credential_store: credential_store.as_ref(),
        };
        for entry in &mut self.entries {
            let provider = login::LoginProvider {
                id: entry.id.clone(),
                name: entry.name.clone(),
                method: entry.method.clone(),
            };
            entry.source = login::resolve_source(&provider, &inputs);
            entry.account_id = match &entry.method {
                LoginMethod::OAuth { oauth_provider_id } if entry.source.is_some() => {
                    login::oauth_account_id(oauth_provider_id, None)
                }
                _ => None,
            };
        }
    }

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

    pub fn arm_entry(&mut self) -> bool {
        let Some(entry) = self.selected_entry() else {
            return false;
        };
        if !matches!(entry.method, LoginMethod::ApiKey { .. }) {
            return false;
        }
        self.entry = Some(EntryPrompt::new(&entry.id, &entry.name, None));
        self.toast = None;
        true
    }

    pub fn arm_entry_for(&mut self, entry_id: &str) -> bool {
        self.filter.clear();
        let Some(index) = self.entries.iter().position(|e| e.id == entry_id) else {
            return false;
        };
        self.selected = index;
        if !self.arm_entry() {
            return false;
        }
        self.toast = Some(format!(
            "paste {entry_id} api key, enter saves, esc cancels"
        ));
        true
    }

    pub fn cancel_entry(&mut self) {
        self.entry = None;
        self.toast = None;
    }

    pub fn begin_validation(&mut self, entry_id: &str, provider_name: &str) {
        self.entry = Some(EntryPrompt::new(entry_id, provider_name, None));
        self.validating = Some(entry_id.to_string());
        self.toast = None;
    }

    pub fn finish_validation_success(&mut self, toast: impl Into<String>) {
        self.validating = None;
        self.entry = None;
        self.toast = Some(toast.into());
    }

    pub fn finish_validation_failure(
        &mut self,
        entry_id: &str,
        provider_name: &str,
        reason: impl Into<String>,
    ) {
        self.validating = None;
        self.entry = Some(EntryPrompt::new(
            entry_id,
            provider_name,
            Some(reason.into()),
        ));
        self.toast = None;
    }

    fn selected_entry(&self) -> Option<&LoginEntry> {
        let visible = filtered_entries(self);
        visible.get(self.selected).copied()
    }
}

impl crate::filtered_list::FilteredList for LoginPickerState {
    fn filter_and_selected(&mut self) -> (&mut String, &mut usize) {
        (&mut self.filter, &mut self.selected)
    }

    fn visible_len(&self) -> usize {
        filtered_entries(self).len()
    }
}

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
    fn arm_entry_for_jumps_straight_to_the_named_key_prompt() {
        let mut picker = LoginPickerState::new(vec![
            oauth_entry("anthropic-pro-max", "Anthropic", None),
            api_key_entry("zai", "Z.AI", None),
        ]);
        picker.filter = "anth".into();
        assert!(picker.arm_entry_for("zai"));
        assert!(picker.filter.is_empty());
        assert_eq!(picker.selected, 1);
        assert_eq!(picker.entry.as_ref().unwrap().entry_id, "zai");
        assert!(!picker.arm_entry_for("anthropic-pro-max"));
        assert!(!picker.arm_entry_for("nope"));
    }

    #[test]
    fn arm_entry_arms_for_api_key_rows_only() {
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
    fn logged_in_helper_reflects_source_presence() {
        assert!(oauth_entry("a", "A", Some(LoginSource::OAuth)).logged_in());
        assert!(!api_key_entry("a", "A", None).logged_in());
    }

    #[test]
    fn begin_validation_clears_buffer_but_keeps_prompt_alive_for_popover() {
        let mut picker =
            LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter", None)]);
        picker.arm_entry();
        if let Some(prompt) = picker.entry.as_mut() {
            prompt.buffer.push_str("sk-or-v1-secret");
        }
        picker.begin_validation("openrouter", "OpenRouter");
        let prompt = picker
            .entry
            .as_ref()
            .expect("prompt should remain so the popover stays visible");
        assert!(prompt.buffer.is_empty(), "buffer must be wiped");
        assert!(prompt.error_message.is_none());
        assert_eq!(picker.validating.as_deref(), Some("openrouter"));
        assert!(
            picker.toast.is_none(),
            "validating message belongs in the popover, not the bottom toast"
        );
    }

    #[test]
    fn finish_validation_failure_restores_entry_prompt_for_retry() {
        let mut picker =
            LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter", None)]);
        picker.arm_entry();
        picker.begin_validation("openrouter", "OpenRouter");
        picker.finish_validation_failure("openrouter", "OpenRouter", "key was rejected");
        assert!(picker.validating.is_none());
        let prompt = picker.entry.as_ref().expect("prompt should be re-armed");
        assert!(prompt.buffer.is_empty());
        assert_eq!(prompt.error_message.as_deref(), Some("key was rejected"));
        assert!(
            picker.toast.is_none(),
            "rejection lives in the prompt now, not the picker toast"
        );
    }

    #[test]
    fn finish_validation_success_clears_state_and_shows_confirmation() {
        let mut picker =
            LoginPickerState::new(vec![api_key_entry("openrouter", "OpenRouter", None)]);
        picker.arm_entry();
        picker.begin_validation("openrouter", "OpenRouter");
        picker.finish_validation_success("saved api key for openrouter");
        assert!(picker.validating.is_none());
        assert!(
            picker.entry.is_none(),
            "entry prompt should not be re-armed"
        );
        assert!(
            picker.toast.as_deref().unwrap().contains("saved"),
            "toast should mention save, got {:?}",
            picker.toast
        );
    }
}
