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
    /// codex-only intra-catalogue priority (higher = preferred). drives
    /// the picker sort within the codex provider group; defaults to 0 for
    /// non-codex models (which keep their natural ordering)
    pub priority: i32,
    /// codex-only visibility marker (`default`, `internal`, `experimental`,
    /// …). `None` or `"default"` is shown by default; anything else is
    /// hidden unless the user opts in via `/model --all`
    pub visibility: Option<String>,
}

/// codex provider key. used as the discriminant for codex-specific picker
/// behaviour (priority sort, hidden visibility, speed-tier badges).
pub const CODEX_PROVIDER: &str = "openai-codex";

/// is this model entry hidden from the default picker view?
///
/// codex marks models with non-default visibility (`internal`,
/// `experimental`, …) so the picker can keep the default list focused on
/// production-ready entries. callers gate display on this and surface the
/// rest behind `/model --all`. visibility is a codex-only field, so
/// non-codex providers always pass even if the value happens to be set.
#[must_use]
pub fn is_hidden_visibility(model: &ModelCompletion) -> bool {
    if model.provider != CODEX_PROVIDER {
        return false;
    }
    model
        .visibility
        .as_deref()
        .is_some_and(|v| !v.is_empty() && v != "default")
}

/// stable-sort codex models by priority desc within their contiguous
/// provider run, leaving non-codex models in their input order.
///
/// codex's `priority` field is curated to put preferred models on top.
/// the merged catalogue groups by `(provider, id)` already, so we just
/// reorder within each codex run. ties break alphabetically by id for
/// determinism.
pub fn sort_codex_priority(items: &mut [ModelCompletion]) {
    let mut i = 0;
    while i < items.len() {
        if items[i].provider == CODEX_PROVIDER {
            let mut j = i + 1;
            while j < items.len() && items[j].provider == CODEX_PROVIDER {
                j += 1;
            }
            items[i..j].sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
            i = j;
        } else {
            i += 1;
        }
    }
}

/// prepare the catalogue for the picker: drop hidden codex entries
/// unless `show_all`, then apply codex priority ordering. callers feed
/// the unfiltered catalogue and get back the slice the picker should
/// actually render.
#[must_use]
pub fn prepare_picker_models(models: &[ModelCompletion], show_all: bool) -> Vec<ModelCompletion> {
    let mut out: Vec<ModelCompletion> = if show_all {
        models.to_vec()
    } else {
        models
            .iter()
            .filter(|m| !is_hidden_visibility(m))
            .cloned()
            .collect()
    };
    sort_codex_priority(&mut out);
    out
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
    /// which match is selected
    pub selected: usize,
}

impl SlashMenuState {
    #[must_use]
    pub fn for_commands(matches: Vec<SlashCommand>) -> Self {
        Self {
            matches,
            selected: 0,
        }
    }
}

#[cfg(test)]
mod picker_ordering_tests {
    use super::*;

    fn codex(id: &str, priority: i32, visibility: Option<&str>) -> ModelCompletion {
        ModelCompletion {
            id: id.into(),
            name: id.into(),
            provider: CODEX_PROVIDER.into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority,
            visibility: visibility.map(String::from),
        }
    }

    fn other(id: &str, provider: &str) -> ModelCompletion {
        ModelCompletion {
            id: id.into(),
            name: id.into(),
            provider: provider.into(),
            stale: false,
            description: None,
            speed_tiers: Vec::new(),
            priority: 0,
            visibility: None,
        }
    }

    #[test]
    fn is_hidden_visibility_only_flags_non_default_codex_values() {
        // None and "default" stay visible; everything else is hidden so the
        // picker can keep its default view focused on production-ready models
        assert!(!is_hidden_visibility(&codex("a", 0, None)));
        assert!(!is_hidden_visibility(&codex("a", 0, Some("default"))));
        assert!(is_hidden_visibility(&codex("a", 0, Some("internal"))));
        assert!(is_hidden_visibility(&codex("a", 0, Some("experimental"))));
    }

    #[test]
    fn sort_codex_priority_orders_codex_run_by_priority_desc() {
        // priority is curated to put preferred models on top; the codex
        // run gets reordered, ties broken alphabetically by id
        let mut items = vec![
            codex("low", 1, None),
            codex("high", 9, None),
            codex("mid-b", 5, None),
            codex("mid-a", 5, None),
        ];
        sort_codex_priority(&mut items);
        let order: Vec<&str> = items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(order, vec!["high", "mid-a", "mid-b", "low"]);
    }

    #[test]
    fn sort_codex_priority_leaves_non_codex_models_in_place() {
        // anthropic and openrouter rows must keep their input order even when
        // a codex run sits between them - only the codex slice is reordered
        let mut items = vec![
            other("opus", "anthropic"),
            other("sonnet", "anthropic"),
            codex("low", 1, None),
            codex("high", 9, None),
            other("mistral", "openrouter"),
        ];
        sort_codex_priority(&mut items);
        let order: Vec<&str> = items.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(order, vec!["opus", "sonnet", "high", "low", "mistral"]);
    }

    #[test]
    fn prepare_picker_models_drops_hidden_codex_entries_by_default() {
        // /model with no flags hides experimental/internal codex entries so
        // the picker doesn't drown the user in upstream test models
        let models = vec![
            codex("public", 5, Some("default")),
            codex("internal-thing", 9, Some("internal")),
            codex("experimental", 3, Some("experimental")),
        ];
        let prepared = prepare_picker_models(&models, false);
        let ids: Vec<&str> = prepared.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["public"]);
    }

    #[test]
    fn prepare_picker_models_with_show_all_keeps_hidden_entries() {
        // /model --all is the opt-in revealing every entry; visibility filter
        // is bypassed but the codex priority sort still runs
        let models = vec![
            codex("public-low", 1, Some("default")),
            codex("internal-high", 9, Some("internal")),
            codex("public-mid", 5, Some("default")),
        ];
        let prepared = prepare_picker_models(&models, true);
        let ids: Vec<&str> = prepared.iter().map(|m| m.id.as_str()).collect();
        // priority desc within the codex run: 9, 5, 1
        assert_eq!(ids, vec!["internal-high", "public-mid", "public-low"]);
    }

    #[test]
    fn prepare_picker_models_does_not_filter_non_codex_visibility() {
        // visibility is a codex-only field; non-codex providers don't set it
        // and must never be filtered even when it happens to be populated
        let mut anthropic = other("opus", "anthropic");
        anthropic.visibility = Some("internal".into());
        let prepared = prepare_picker_models(&[anthropic], false);
        assert_eq!(prepared.len(), 1, "non-codex models bypass the filter");
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

/// build a [`ModelCompletion`] from a merged catalogue entry, pulling
/// codex-only fields (description, speed tiers, priority, visibility)
/// through the codex extras accessor. callers feed
/// `mush_ai::discovery::merged_catalogue()` rows straight in.
#[must_use]
pub fn model_completion_from_merged(entry: &mush_ai::discovery::MergedModel) -> ModelCompletion {
    let stale = matches!(
        entry.source,
        mush_ai::discovery::ModelSource::DiscoveredStale
    );
    let extras = if matches!(&entry.model.provider, mush_ai::types::Provider::Custom(name) if name == CODEX_PROVIDER)
    {
        mush_ai::discovery::codex::extras(entry.raw.as_ref())
    } else {
        None
    };
    let (description, speed_tiers, priority, visibility) = match extras {
        Some(extras) => (
            extras.description,
            extras.additional_speed_tiers,
            extras.priority,
            extras.visibility,
        ),
        None => (None, Vec::new(), 0, None),
    };
    ModelCompletion {
        id: entry.model.id.to_string(),
        name: entry.model.name.clone(),
        provider: entry.model.provider.to_string(),
        stale,
        description,
        speed_tiers,
        priority,
        visibility,
    }
}
