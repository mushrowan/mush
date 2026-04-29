//! model picker state and filtering helpers
//!
//! parallels [`crate::session_picker`]: a centred floating window
//! activated by `/model` with no arg. supports two tabs (all /
//! favourites), fuzzy filter, ctrl+j/k navigation, ctrl+f to toggle
//! favourites, ctrl+d to delete stale entries. direct id arg
//! (`/model claude-opus-4-6`) still bypasses the picker entirely.

use std::cell::RefCell;

use crate::fuzzy::FuzzyFilter;
use crate::slash_menu::{DeleteConfirm, ModelCompletion};

/// which tab is active in the picker
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPickerTab {
    /// every model from the prepared catalogue (after visibility +
    /// codex priority filtering)
    All,
    /// only models present in `favourite_models`. shown in the order
    /// they appear in `models` so the picker is stable across opens
    Favourites,
}

/// state for the centred model picker overlay
#[derive(Debug)]
pub struct ModelPickerState {
    /// every model the picker may show, already prepared for codex
    /// priority and visibility (see `slash_menu::prepare_picker_models`)
    pub models: Vec<ModelCompletion>,
    /// model ids the user marked as favourites. the favourites tab
    /// renders this subset, and ★ markers next to ids reflect membership
    pub favourite_models: Vec<String>,
    /// when the user pinned favourites in config.toml the picker
    /// refuses imperative add / remove via ctrl+f and shows a toast
    pub favourites_locked: bool,
    pub tab: ModelPickerTab,
    pub filter: String,
    pub selected: usize,
    /// short transient message shown in the picker footer (e.g. when
    /// favourites are locked or after arming a delete-stale prompt)
    pub toast: Option<String>,
    /// active delete-stale confirmation. when `Some`, the picker
    /// refuses normal navigation and routes y/n/esc into the prompt
    pub confirm_delete: Option<DeleteConfirm>,
    /// whether the picker was opened with `/model --all` (codex
    /// `internal` / `experimental` entries kept in `models`)
    pub show_all: bool,
    /// nucleo matcher reused across renders. `RefCell` because
    /// `filtered_models` takes `&self` for read-only paths but the
    /// matcher needs `&mut`
    matcher: RefCell<FuzzyFilter>,
}

impl ModelPickerState {
    #[must_use]
    pub fn new(
        models: Vec<ModelCompletion>,
        favourite_models: Vec<String>,
        favourites_locked: bool,
        show_all: bool,
    ) -> Self {
        Self {
            models,
            favourite_models,
            favourites_locked,
            tab: ModelPickerTab::All,
            filter: String::new(),
            selected: 0,
            toast: None,
            confirm_delete: None,
            show_all,
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    /// whether `model_id` is starred in the active favourites list
    #[must_use]
    pub fn is_favourite(&self, model_id: &str) -> bool {
        self.favourite_models.iter().any(|f| f == model_id)
    }

    /// move to the next tab, resetting selection so the highlight
    /// sits on the first row of the new view
    pub fn toggle_tab(&mut self) {
        self.tab = match self.tab {
            ModelPickerTab::All => ModelPickerTab::Favourites,
            ModelPickerTab::Favourites => ModelPickerTab::All,
        };
        self.selected = 0;
    }

    /// arm a single-model delete confirmation if the highlighted row
    /// is stale, returning whether the prompt was armed
    pub fn arm_delete_selected(&mut self) -> bool {
        let visible = filtered_models(self);
        let Some(model) = visible.get(self.selected) else {
            return false;
        };
        if !model.stale {
            return false;
        }
        let display = format!("{} ({})", model.id, model.provider);
        self.confirm_delete = Some(DeleteConfirm::Single {
            provider: model.provider.clone(),
            id: model.id.clone(),
        });
        self.toast = Some(format!("delete {display}? y/n"));
        true
    }

    /// arm the delete-all-stale confirmation. returns the count of
    /// rows that would be affected. with no stale rows, sets a toast
    /// and returns 0 without arming
    pub fn arm_delete_all_stale(&mut self) -> usize {
        let stale_count = self.models.iter().filter(|m| m.stale).count();
        if stale_count == 0 {
            self.toast = Some("no stale models to delete".into());
            return 0;
        }
        self.confirm_delete = Some(DeleteConfirm::AllStale);
        self.toast = Some(format!("delete all {stale_count} stale model(s)? y/n"));
        stale_count
    }

    pub fn cancel_delete(&mut self) {
        self.confirm_delete = None;
        self.toast = None;
    }
}

/// models matching the active tab + filter, in the order the picker
/// should render them. empty filter keeps catalogue order; otherwise
/// fuzzy-rank by `id name provider`
#[must_use]
pub fn filtered_models(picker: &ModelPickerState) -> Vec<&ModelCompletion> {
    let tab_filtered: Vec<&ModelCompletion> = match picker.tab {
        ModelPickerTab::All => picker.models.iter().collect(),
        ModelPickerTab::Favourites => picker
            .models
            .iter()
            .filter(|m| picker.favourite_models.iter().any(|f| f == &m.id))
            .collect(),
    };

    if picker.filter.is_empty() {
        return tab_filtered;
    }

    // score against id + name + provider so a match in any of those
    // contributes to the ranking. fuzzy gives subsequence + typo
    // tolerance like the session picker
    let haystacks: Vec<String> = tab_filtered
        .iter()
        .map(|m| format!("{} {} {}", m.id, m.name, m.provider))
        .collect();
    let mut matcher = picker.matcher.borrow_mut();
    let indices = matcher.filter(&haystacks, &picker.filter, String::as_str);
    indices.into_iter().map(|i| tab_filtered[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, name: &str, provider: &str) -> ModelCompletion {
        ModelCompletion {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        }
    }

    fn stale_model(id: &str, provider: &str) -> ModelCompletion {
        ModelCompletion {
            stale: true,
            ..model(id, id, provider)
        }
    }

    #[test]
    fn new_picker_starts_on_all_tab_with_empty_filter() {
        let picker =
            ModelPickerState::new(vec![model("a", "A", "anthropic")], vec![], false, false);
        assert_eq!(picker.tab, ModelPickerTab::All);
        assert!(picker.filter.is_empty());
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn toggle_tab_resets_selection() {
        let mut picker = ModelPickerState::new(
            vec![model("a", "A", "anthropic"), model("b", "B", "anthropic")],
            vec!["a".into()],
            false,
            false,
        );
        picker.selected = 1;
        picker.toggle_tab();
        assert_eq!(picker.tab, ModelPickerTab::Favourites);
        assert_eq!(picker.selected, 0);
        picker.toggle_tab();
        assert_eq!(picker.tab, ModelPickerTab::All);
    }

    #[test]
    fn favourites_tab_filters_to_starred_only() {
        let picker = ModelPickerState {
            tab: ModelPickerTab::Favourites,
            ..ModelPickerState::new(
                vec![
                    model("opus", "Claude Opus", "anthropic"),
                    model("haiku", "Claude Haiku", "anthropic"),
                    model("gpt-5", "GPT 5", "openai"),
                ],
                vec!["opus".into(), "gpt-5".into()],
                false,
                false,
            )
        };
        let ids: Vec<&str> = filtered_models(&picker)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["opus", "gpt-5"]);
    }

    #[test]
    fn all_tab_with_empty_filter_returns_input_order() {
        let picker = ModelPickerState::new(
            vec![
                model("opus", "Claude Opus", "anthropic"),
                model("haiku", "Claude Haiku", "anthropic"),
                model("gpt-5", "GPT 5", "openai"),
            ],
            vec![],
            false,
            false,
        );
        let ids: Vec<&str> = filtered_models(&picker)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["opus", "haiku", "gpt-5"]);
    }

    #[test]
    fn fuzzy_filter_ranks_by_id_name_or_provider() {
        let mut picker = ModelPickerState::new(
            vec![
                model("claude-opus-4", "Claude Opus 4", "anthropic"),
                model("claude-haiku-4", "Claude Haiku 4", "anthropic"),
                model("gpt-5", "GPT 5", "openai"),
            ],
            vec![],
            false,
            false,
        );
        // matches against provider name
        picker.filter = "openai".into();
        let ids: Vec<&str> = filtered_models(&picker)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["gpt-5"]);

        // matches against display name (subsequence)
        picker.filter = "haiku".into();
        let ids: Vec<&str> = filtered_models(&picker)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["claude-haiku-4"]);
    }

    #[test]
    fn fuzzy_filter_respects_tab() {
        // even with a query that would match a non-favourite, the
        // favourites tab keeps non-favourites out of the result
        let mut picker = ModelPickerState::new(
            vec![
                model("opus", "Claude Opus", "anthropic"),
                model("haiku", "Claude Haiku", "anthropic"),
            ],
            vec!["opus".into()],
            false,
            false,
        );
        picker.tab = ModelPickerTab::Favourites;
        picker.filter = "haiku".into();
        assert!(
            filtered_models(&picker).is_empty(),
            "favourites tab must not surface non-favourited matches"
        );
    }

    #[test]
    fn arm_delete_selected_skips_fresh_rows() {
        let mut picker = ModelPickerState::new(
            vec![
                model("fresh", "Fresh", "anthropic"),
                stale_model("old", "anthropic"),
            ],
            vec![],
            false,
            false,
        );
        picker.selected = 0;
        assert!(!picker.arm_delete_selected());
        assert!(picker.confirm_delete.is_none());
        picker.selected = 1;
        assert!(picker.arm_delete_selected());
        assert_eq!(
            picker.confirm_delete,
            Some(DeleteConfirm::Single {
                provider: "anthropic".into(),
                id: "old".into()
            })
        );
    }

    #[test]
    fn arm_delete_all_stale_with_none_returns_zero_with_toast() {
        let mut picker = ModelPickerState::new(
            vec![model("fresh", "Fresh", "anthropic")],
            vec![],
            false,
            false,
        );
        assert_eq!(picker.arm_delete_all_stale(), 0);
        assert!(picker.confirm_delete.is_none());
        assert!(picker.toast.as_deref().unwrap().contains("no stale"));
    }

    #[test]
    fn cancel_delete_clears_prompt_and_toast() {
        let mut picker =
            ModelPickerState::new(vec![stale_model("old", "anthropic")], vec![], false, false);
        picker.arm_delete_selected();
        picker.cancel_delete();
        assert!(picker.confirm_delete.is_none());
        assert!(picker.toast.is_none());
    }
}
