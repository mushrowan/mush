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
        let tab = if favourite_models.is_empty() {
            ModelPickerTab::All
        } else {
            ModelPickerTab::Favourites
        };
        Self {
            models,
            favourite_models,
            favourites_locked,
            tab,
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
            .favourite_models
            .iter()
            .filter_map(|fav| picker.models.iter().find(|m| &m.id == fav))
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

/// max number of model ids kept in the recency list so it can't grow
/// without bound
const MAX_RECENT_MODELS: usize = 50;

/// move `model_id` to the front of `recent` (most-recent first),
/// de-duplicating and capping the length
#[must_use]
pub fn promote_recent_model(recent: &[String], model_id: &str) -> Vec<String> {
    let mut next = Vec::with_capacity(recent.len() + 1);
    next.push(model_id.to_string());
    for id in recent {
        if id != model_id {
            next.push(id.clone());
        }
    }
    next.truncate(MAX_RECENT_MODELS);
    next
}

/// order `favourites` by most-recently-used. `recent` lists model ids
/// most-recent first. favourites present in `recent` lead in that order,
/// then any favourites never used follow in their original order. recent
/// ids that aren't favourites are ignored.
#[must_use]
pub fn favourites_by_recency(favourites: &[String], recent: &[String]) -> Vec<String> {
    let mut ordered: Vec<String> = recent
        .iter()
        .filter(|id| favourites.iter().any(|f| f == *id))
        .cloned()
        .collect();
    for fav in favourites {
        if !ordered.iter().any(|id| id == fav) {
            ordered.push(fav.clone());
        }
    }
    ordered
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
    fn new_picker_starts_on_all_tab_when_no_favourites() {
        // with nothing favourited the favourites tab would be empty, so
        // a fresh picker falls back to the all tab
        let picker =
            ModelPickerState::new(vec![model("a", "A", "anthropic")], vec![], false, false);
        assert_eq!(picker.tab, ModelPickerTab::All);
        assert!(picker.filter.is_empty());
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn new_picker_starts_on_favourites_tab_when_favourites_present() {
        // the favourites tab is the default landing view whenever the
        // user has any favourites
        let picker = ModelPickerState::new(
            vec![model("a", "A", "anthropic")],
            vec!["a".into()],
            false,
            false,
        );
        assert_eq!(picker.tab, ModelPickerTab::Favourites);
    }

    #[test]
    fn favourites_tab_renders_in_favourite_list_order_not_catalogue_order() {
        // the favourites tab must honour the order of `favourite_models`
        // (recency-sorted by the caller) rather than the catalogue order
        let picker = ModelPickerState {
            tab: ModelPickerTab::Favourites,
            ..ModelPickerState::new(
                vec![
                    model("opus", "Claude Opus", "anthropic"),
                    model("haiku", "Claude Haiku", "anthropic"),
                    model("gpt-5", "GPT 5", "openai"),
                ],
                vec!["gpt-5".into(), "opus".into()],
                false,
                false,
            )
        };
        let ids: Vec<&str> = filtered_models(&picker)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["gpt-5", "opus"]);
    }

    #[test]
    fn favourites_by_recency_orders_used_first_then_unused_in_original_order() {
        // recent ids (most-recent first) that are favourites lead, then
        // the remaining favourites follow in their existing order
        let favs = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let recent = vec!["c".to_string(), "a".to_string()];
        assert_eq!(
            favourites_by_recency(&favs, &recent),
            vec![
                "c".to_string(),
                "a".to_string(),
                "b".to_string(),
                "d".to_string()
            ]
        );
    }

    #[test]
    fn favourites_by_recency_ignores_recent_non_favourites() {
        // a recently used model that isn't favourited must not appear
        let favs = vec!["a".to_string(), "b".to_string()];
        let recent = vec!["zzz".to_string(), "b".to_string()];
        assert_eq!(
            favourites_by_recency(&favs, &recent),
            vec!["b".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn promote_recent_model_moves_to_front_and_dedups() {
        let recent = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // existing id jumps to the front, no duplicate left behind
        assert_eq!(
            promote_recent_model(&recent, "c"),
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
        // new id is prepended, existing order preserved after it
        assert_eq!(
            promote_recent_model(&recent, "new"),
            vec![
                "new".to_string(),
                "a".to_string(),
                "b".to_string(),
                "c".to_string()
            ]
        );
    }

    #[test]
    fn promote_recent_model_caps_length() {
        let recent: Vec<String> = (0..MAX_RECENT_MODELS).map(|i| i.to_string()).collect();
        let promoted = promote_recent_model(&recent, "fresh");
        assert_eq!(promoted.len(), MAX_RECENT_MODELS);
        assert_eq!(promoted.first().map(String::as_str), Some("fresh"));
    }

    #[test]
    fn toggle_tab_resets_selection() {
        let mut picker = ModelPickerState::new(
            vec![model("a", "A", "anthropic"), model("b", "B", "anthropic")],
            vec!["a".into()],
            false,
            false,
        );
        // favourites present, so the picker starts on the favourites tab
        assert_eq!(picker.tab, ModelPickerTab::Favourites);
        picker.selected = 1;
        picker.toggle_tab();
        assert_eq!(picker.tab, ModelPickerTab::All);
        assert_eq!(picker.selected, 0);
        picker.toggle_tab();
        assert_eq!(picker.tab, ModelPickerTab::Favourites);
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
