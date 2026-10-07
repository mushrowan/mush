use std::cell::RefCell;

use crate::fuzzy::FuzzyFilter;
use crate::slash_menu::{DeleteConfirm, ModelCompletion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPickerTab {
    All,
    Favourites,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelPickerPurpose {
    #[default]
    Session,
    Subagent,
}

pub const INHERIT_ID: &str = "";

#[must_use]
pub fn inherit_row() -> crate::slash_menu::ModelCompletion {
    crate::slash_menu::ModelCompletion {
        id: INHERIT_ID.to_string(),
        name: "inherit the model this session is on".into(),
        provider: "unset".into(),
        stale: false,
        description: Some("spawns follow the parent, changing when you change model".into()),
        speed_tiers: Vec::new(),
        priority: i32::MAX,
        visibility: None,
    }
}

#[derive(Debug)]
pub struct ModelPickerState {
    pub models: Vec<ModelCompletion>,
    pub favourite_models: Vec<String>,
    pub favourites_locked: bool,
    pub tab: ModelPickerTab,
    pub filter: String,
    pub selected: usize,
    pub toast: Option<String>,
    pub confirm_delete: Option<DeleteConfirm>,
    pub show_all: bool,
    pub purpose: ModelPickerPurpose,
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
            purpose: ModelPickerPurpose::default(),
            tab,
            filter: String::new(),
            selected: 0,
            toast: None,
            confirm_delete: None,
            show_all,
            matcher: RefCell::new(FuzzyFilter::new()),
        }
    }

    #[must_use]
    pub fn is_favourite(&self, model_id: &str) -> bool {
        self.favourite_models.iter().any(|f| f == model_id)
    }

    pub fn toggle_tab(&mut self) {
        self.tab = match self.tab {
            ModelPickerTab::All => ModelPickerTab::Favourites,
            ModelPickerTab::Favourites => ModelPickerTab::All,
        };
        self.selected = 0;
    }

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

impl crate::filtered_list::FilteredList for ModelPickerState {
    fn filter_and_selected(&mut self) -> (&mut String, &mut usize) {
        (&mut self.filter, &mut self.selected)
    }

    fn visible_len(&self) -> usize {
        filtered_models(self).len()
    }
}

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

    let haystacks: Vec<String> = tab_filtered
        .iter()
        .map(|m| format!("{} {} {}", m.id, m.name, m.provider))
        .collect();
    let mut matcher = picker.matcher.borrow_mut();
    let indices = matcher.filter(&haystacks, &picker.filter, String::as_str);
    indices.into_iter().map(|i| tab_filtered[i]).collect()
}

const MAX_RECENT_MODELS: usize = 50;

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
        let picker =
            ModelPickerState::new(vec![model("a", "A", "anthropic")], vec![], false, false);
        assert_eq!(picker.tab, ModelPickerTab::All);
        assert!(picker.filter.is_empty());
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn new_picker_starts_on_favourites_tab_when_favourites_present() {
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
        assert_eq!(
            promote_recent_model(&recent, "c"),
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );
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
        picker.filter = "openai".into();
        let ids: Vec<&str> = filtered_models(&picker)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["gpt-5"]);

        picker.filter = "haiku".into();
        let ids: Vec<&str> = filtered_models(&picker)
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["claude-haiku-4"]);
    }

    #[test]
    fn fuzzy_filter_respects_tab() {
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
