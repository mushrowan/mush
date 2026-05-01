//! login picker state and filtering helpers
//!
//! mirrors [`crate::session_picker`] / [`crate::model_picker`]: a centred
//! floating window activated by `/login` with no arg. lists every oauth
//! provider with logged-in/out state, fuzzy filter, ctrl+j/k navigation,
//! enter to start a flow (when logged out) or arm a logout confirmation
//! (when logged in). direct id arg (`/login anthropic`) bypasses the
//! picker like before.

use std::cell::RefCell;

use crate::fuzzy::FuzzyFilter;

/// one row in the picker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginEntry {
    /// stable id from `mush_ai::oauth::list_providers` (e.g. `anthropic`)
    pub provider_id: String,
    /// human-readable name shown in the row
    pub provider_name: String,
    /// whether saved credentials currently exist for this provider
    pub logged_in: bool,
    /// optional account id captured at login time (anthropic surfaces
    /// this for paid plans), shown next to the row when present
    pub account_id: Option<String>,
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
    /// of the named provider id. enter is disabled while armed
    pub confirm_logout: Option<String>,
    /// nucleo matcher reused across renders. `RefCell` because
    /// `filtered_entries` takes `&self` for read-only paths but the
    /// matcher needs `&mut`
    matcher: RefCell<FuzzyFilter>,
}

impl LoginPickerState {
    #[must_use]
    pub fn new(entries: Vec<LoginEntry>) -> Self {
        Self {
            entries,
            filter: String::new(),
            selected: 0,
            toast: None,
            confirm_logout: None,
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    /// arm a logout confirmation for the highlighted row, returning
    /// whether the prompt was armed. fresh (logged-out) rows skip
    /// arming and surface a toast instead so the keypress isn't silent
    pub fn arm_logout(&mut self) -> bool {
        let visible = filtered_entries(self);
        let Some(entry) = visible.get(self.selected) else {
            return false;
        };
        let logged_in = entry.logged_in;
        let provider_id = entry.provider_id.clone();
        let provider_name = entry.provider_name.clone();
        drop(visible);
        if !logged_in {
            self.toast = Some("not logged in".into());
            return false;
        }
        self.toast = Some(format!("log out of {provider_name}? y/n"));
        self.confirm_logout = Some(provider_id);
        true
    }

    pub fn cancel_logout(&mut self) {
        self.confirm_logout = None;
        self.toast = None;
    }

    /// rebuild logged-in flags from a fresh credential snapshot.
    /// callers pass `(provider_id, account_id)` for every provider that
    /// currently has saved credentials. anything missing flips back to
    /// logged-out
    pub fn refresh_logged_in(&mut self, present: &[(String, Option<String>)]) {
        for entry in &mut self.entries {
            let hit = present.iter().find(|(id, _)| id == &entry.provider_id);
            entry.logged_in = hit.is_some();
            entry.account_id = hit.and_then(|(_, acc)| acc.clone());
        }
    }
}

/// entries matching the current filter, in render order. empty filter
/// keeps the natural provider order; otherwise fuzzy-rank by
/// `id name account`
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
            format!("{} {} {}", e.provider_id, e.provider_name, acc)
        })
        .collect();
    let mut matcher = picker.matcher.borrow_mut();
    let indices = matcher.filter(&haystacks, &picker.filter, String::as_str);
    indices.into_iter().map(|i| &picker.entries[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str, logged_in: bool) -> LoginEntry {
        LoginEntry {
            provider_id: id.into(),
            provider_name: name.into(),
            logged_in,
            account_id: None,
        }
    }

    #[test]
    fn new_picker_starts_with_first_row_selected_and_empty_filter() {
        let picker = LoginPickerState::new(vec![
            entry("anthropic", "Anthropic", true),
            entry("openai-codex", "ChatGPT Codex", false),
        ]);
        assert_eq!(picker.selected, 0);
        assert!(picker.filter.is_empty());
        assert!(picker.confirm_logout.is_none());
    }

    #[test]
    fn empty_filter_returns_input_order() {
        let picker = LoginPickerState::new(vec![
            entry("anthropic", "Anthropic", true),
            entry("openai-codex", "ChatGPT Codex", false),
        ]);
        let ids: Vec<&str> = filtered_entries(&picker)
            .iter()
            .map(|e| e.provider_id.as_str())
            .collect();
        assert_eq!(ids, vec!["anthropic", "openai-codex"]);
    }

    #[test]
    fn fuzzy_filter_matches_name_or_id() {
        let mut picker = LoginPickerState::new(vec![
            entry("anthropic", "Anthropic", true),
            entry("openai-codex", "ChatGPT Codex", false),
        ]);
        picker.filter = "codex".into();
        let ids: Vec<&str> = filtered_entries(&picker)
            .iter()
            .map(|e| e.provider_id.as_str())
            .collect();
        assert_eq!(ids, vec!["openai-codex"]);
    }

    #[test]
    fn arm_logout_only_arms_for_logged_in_rows() {
        let mut picker = LoginPickerState::new(vec![
            entry("anthropic", "Anthropic", true),
            entry("openai-codex", "ChatGPT Codex", false),
        ]);
        picker.selected = 1;
        assert!(!picker.arm_logout(), "logged-out row must not arm");
        assert!(picker.confirm_logout.is_none());
        assert!(
            picker.toast.as_deref().unwrap().contains("not logged in"),
            "toast should explain the no-op"
        );

        picker.selected = 0;
        assert!(picker.arm_logout(), "logged-in row should arm");
        assert_eq!(picker.confirm_logout.as_deref(), Some("anthropic"));
        assert!(picker.toast.as_deref().unwrap().contains("Anthropic"));
    }

    #[test]
    fn cancel_logout_clears_prompt_and_toast() {
        let mut picker = LoginPickerState::new(vec![entry("anthropic", "Anthropic", true)]);
        picker.arm_logout();
        picker.cancel_logout();
        assert!(picker.confirm_logout.is_none());
        assert!(picker.toast.is_none());
    }

    #[test]
    fn refresh_logged_in_flips_state_and_account_id() {
        let mut picker = LoginPickerState::new(vec![
            entry("anthropic", "Anthropic", true),
            entry("openai-codex", "ChatGPT Codex", false),
        ]);
        // simulate logging out of anthropic, logging in to codex with an account
        picker.refresh_logged_in(&[("openai-codex".into(), Some("acc-42".into()))]);
        assert!(!picker.entries[0].logged_in);
        assert!(picker.entries[1].logged_in);
        assert_eq!(picker.entries[1].account_id.as_deref(), Some("acc-42"));
    }
}
