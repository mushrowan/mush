//! slash completion state and filtering helpers

/// slash command menu item
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

/// model completion menu item
#[derive(Debug, Clone)]
pub struct ModelCompletion {
    pub id: String,
    pub name: String,
    /// provider key (`Provider::to_string()` of the source) — used by the
    /// picker's delete-stale flow to tell `DiscoveryCache::remove_model`
    /// which provider's sub-cache to mutate
    pub provider: String,
    /// the model is in the discovery cache but absent from the latest
    /// fetch for its provider — likely deprecated. picker renders a
    /// `[stale]` marker so the user can spot dropped entries.
    pub stale: bool,
    /// optional human-readable blurb (codex returns these per model).
    /// rendered after the display name to help users tell similar slugs apart.
    pub description: Option<String>,
    /// codex-style speed tier hints (`fast`, …). rendered as `[tier]` badges
    /// so the user can spot snappy models at a glance.
    pub speed_tiers: Vec<String>,
}

/// pending delete confirmation in the model picker. armed by ctrl+d (one
/// row) or ctrl+shift+d (every stale row), confirmed with `y`, cancelled
/// with `n`/`esc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteConfirm {
    /// delete one specific model from a single provider sub-cache
    Single { provider: String, id: String },
    /// delete every stale entry across every provider
    AllStale,
}

/// state for the slash command completion menu
#[derive(Debug, Clone)]
pub struct SlashMenuState {
    /// filtered commands matching current input
    pub matches: Vec<SlashCommand>,
    /// filtered models matching current /model query
    pub model_matches: Vec<ModelCompletion>,
    /// whether this menu is showing models
    pub model_mode: bool,
    /// which match is selected
    pub selected: usize,
    /// effective favourites at open time. model_mode renders a ★ marker for
    /// ids listed here. empty vec = no favourites
    pub favourite_models: Vec<String>,
    /// whether imperative add/remove (ctrl+f in the picker) should be
    /// refused because the user declared favourites in config.toml
    pub favourites_locked: bool,
    /// toast shown at the bottom of the popup (e.g. locked notice). cleared
    /// on next keystroke
    pub toast: Option<String>,
    /// active delete-stale confirmation. when `Some`, the picker is
    /// rendering the toast prompt and `y`/`n`/`esc` keys drive the flow
    /// instead of normal navigation
    pub confirm_delete: Option<DeleteConfirm>,
}

impl SlashMenuState {
    #[must_use]
    pub fn for_commands(matches: Vec<SlashCommand>) -> Self {
        Self {
            matches,
            model_matches: Vec::new(),
            model_mode: false,
            selected: 0,
            favourite_models: Vec::new(),
            favourites_locked: false,
            toast: None,
            confirm_delete: None,
        }
    }

    #[must_use]
    pub fn for_models(model_matches: Vec<ModelCompletion>) -> Self {
        Self::for_models_with_favourites(model_matches, Vec::new(), false)
    }

    /// construct a model-mode menu carrying the effective favourites list so
    /// the picker can render ★ markers and reject imperative edits when
    /// favourites are locked by config
    #[must_use]
    pub fn for_models_with_favourites(
        model_matches: Vec<ModelCompletion>,
        favourite_models: Vec<String>,
        favourites_locked: bool,
    ) -> Self {
        Self {
            matches: Vec::new(),
            model_matches,
            model_mode: true,
            selected: 0,
            favourite_models,
            favourites_locked,
            toast: None,
            confirm_delete: None,
        }
    }

    /// whether `model_id` is in the effective favourites list
    #[must_use]
    pub fn is_favourite(&self, model_id: &str) -> bool {
        self.favourite_models.iter().any(|f| f == model_id)
    }

    /// arm a single-model delete confirmation. returns false (no-op) when
    /// the supplied row isn't actually stale, so callers can blindly try.
    pub fn arm_delete_selected(&mut self) -> bool {
        if !self.model_mode {
            return false;
        }
        let Some(model) = self.model_matches.get(self.selected) else {
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

    /// arm a delete-all-stale confirmation. returns the count of stale
    /// rows the prompt will affect, or zero when there's nothing to
    /// delete (in which case no prompt is shown).
    pub fn arm_delete_all_stale(&mut self) -> usize {
        if !self.model_mode {
            return 0;
        }
        let stale_count = self.model_matches.iter().filter(|m| m.stale).count();
        if stale_count == 0 {
            self.toast = Some("no stale models to delete".into());
            return 0;
        }
        self.confirm_delete = Some(DeleteConfirm::AllStale);
        self.toast = Some(format!("delete all {stale_count} stale model(s)? y/n"));
        stale_count
    }

    /// clear any pending delete prompt and its toast.
    pub fn cancel_delete(&mut self) {
        self.confirm_delete = None;
        self.toast = None;
    }
}

#[cfg(test)]
mod confirm_delete_tests {
    use super::*;

    fn fresh(id: &str, provider: &str) -> ModelCompletion {
        ModelCompletion {
            id: id.into(),
            name: id.into(),
            provider: provider.into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
        }
    }

    fn stale(id: &str, provider: &str) -> ModelCompletion {
        ModelCompletion {
            id: id.into(),
            name: id.into(),
            provider: provider.into(),
            stale: true,
            description: None,
            speed_tiers: Vec::new(),
        }
    }

    #[test]
    fn arm_delete_selected_succeeds_only_for_stale_rows() {
        let mut menu =
            SlashMenuState::for_models(vec![fresh("a", "anthropic"), stale("b", "anthropic")]);
        // selected is 0 (fresh) → no-op
        assert!(!menu.arm_delete_selected());
        assert!(menu.confirm_delete.is_none());

        // bump to the stale row
        menu.selected = 1;
        assert!(menu.arm_delete_selected());
        assert_eq!(
            menu.confirm_delete,
            Some(DeleteConfirm::Single {
                provider: "anthropic".into(),
                id: "b".into()
            })
        );
        assert!(menu.toast.as_deref().unwrap().contains("delete b"));
    }

    #[test]
    fn arm_delete_all_stale_returns_count_and_arms() {
        let mut menu = SlashMenuState::for_models(vec![
            fresh("a", "anthropic"),
            stale("b", "anthropic"),
            stale("c", "openrouter"),
        ]);
        let n = menu.arm_delete_all_stale();
        assert_eq!(n, 2);
        assert_eq!(menu.confirm_delete, Some(DeleteConfirm::AllStale));
        assert!(menu.toast.as_deref().unwrap().contains("delete all 2"));
    }

    #[test]
    fn arm_delete_all_stale_when_none_returns_zero_and_toasts() {
        let mut menu =
            SlashMenuState::for_models(vec![fresh("a", "anthropic"), fresh("b", "anthropic")]);
        let n = menu.arm_delete_all_stale();
        assert_eq!(n, 0);
        assert!(menu.confirm_delete.is_none());
        assert!(menu.toast.as_deref().unwrap().contains("no stale"));
    }

    #[test]
    fn cancel_delete_clears_state() {
        let mut menu = SlashMenuState::for_models(vec![stale("b", "anthropic")]);
        menu.arm_delete_selected();
        menu.cancel_delete();
        assert!(menu.confirm_delete.is_none());
        assert!(menu.toast.is_none());
    }

    #[test]
    fn arm_methods_no_op_when_not_in_model_mode() {
        let mut menu = SlashMenuState::for_commands(Vec::new());
        assert!(!menu.arm_delete_selected());
        assert_eq!(menu.arm_delete_all_stale(), 0);
        assert!(menu.confirm_delete.is_none());
    }
}

/// tracks an in-progress tab completion cycle
#[derive(Debug, Clone)]
pub(crate) struct TabState {
    /// matching candidates
    pub(crate) matches: Vec<String>,
    /// which match we're showing (cycles on repeated tab)
    pub(crate) index: usize,
}

#[must_use]
pub(crate) fn filter_command_matches(commands: &[SlashCommand], prefix: &str) -> Vec<SlashCommand> {
    commands
        .iter()
        .filter(|cmd| {
            let full = format!("/{}", cmd.name);
            full.starts_with(prefix)
        })
        .cloned()
        .collect()
}

#[must_use]
pub(crate) fn filter_model_matches(
    model_completions: &[ModelCompletion],
    prefix: &str,
) -> Vec<ModelCompletion> {
    use crate::fuzzy::FuzzyFilter;
    if prefix.is_empty() {
        return model_completions.to_vec();
    }
    // match against the id + name concatenation so either field can
    // contribute to the score. fuzzy gives subsequence + typo tolerance
    let haystacks: Vec<String> = model_completions
        .iter()
        .map(|m| format!("{} {}", m.id, m.name))
        .collect();
    let mut matcher = FuzzyFilter::new();
    let indices = matcher.filter(&haystacks, prefix, String::as_str);
    indices
        .into_iter()
        .map(|i| model_completions[i].clone())
        .collect()
}

/// build a [`ModelCompletion`] from a merged catalogue entry, pulling
/// codex-only fields (description, speed tiers) through the codex extras
/// accessor. callers feed `mush_ai::discovery::merged_catalogue()` rows
/// straight in.
#[must_use]
pub fn model_completion_from_merged(entry: &mush_ai::discovery::MergedModel) -> ModelCompletion {
    let stale = matches!(
        entry.source,
        mush_ai::discovery::ModelSource::DiscoveredStale
    );
    let extras = if matches!(&entry.model.provider, mush_ai::types::Provider::Custom(name) if name == "openai-codex")
    {
        mush_ai::discovery::codex::extras(entry.raw.as_ref())
    } else {
        None
    };
    let (description, speed_tiers) = match extras {
        Some(extras) => (extras.description, extras.additional_speed_tiers),
        None => (None, Vec::new()),
    };
    ModelCompletion {
        id: entry.model.id.to_string(),
        name: entry.model.name.clone(),
        provider: entry.model.provider.to_string(),
        stale,
        description,
        speed_tiers,
    }
}
