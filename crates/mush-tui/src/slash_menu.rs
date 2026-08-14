#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ModelCompletion {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub stale: bool,
    pub description: Option<String>,
    pub speed_tiers: Vec<String>,
    pub priority: i32,
    pub visibility: Option<String>,
}

pub const CODEX_PROVIDER: &str = "openai-codex";

#[must_use]
pub fn is_hidden_visibility(model: &ModelCompletion) -> bool {
    if model.provider != CODEX_PROVIDER {
        return false;
    }
    model
        .visibility
        .as_deref()
        .is_some_and(|v| !v.is_empty() && v != "default" && v != "list")
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteConfirm {
    Single { provider: String, id: String },
    AllStale,
}

#[derive(Debug, Clone)]
pub struct SlashMenuState {
    pub matches: Vec<SlashCommand>,
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
        assert!(!is_hidden_visibility(&codex("a", 0, None)));
        assert!(!is_hidden_visibility(&codex("a", 0, Some("default"))));
        assert!(!is_hidden_visibility(&codex("a", 0, Some("list"))));
        assert!(is_hidden_visibility(&codex("a", 0, Some("internal"))));
        assert!(is_hidden_visibility(&codex("a", 0, Some("experimental"))));
    }

    #[test]
    fn sort_codex_priority_orders_codex_run_by_priority_desc() {
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
        let models = vec![
            codex("public", 5, Some("list")),
            codex("internal-thing", 9, Some("internal")),
            codex("experimental", 3, Some("experimental")),
        ];
        let prepared = prepare_picker_models(&models, false);
        let ids: Vec<&str> = prepared.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["public"]);
    }

    #[test]
    fn prepare_picker_models_with_show_all_keeps_hidden_entries() {
        let models = vec![
            codex("public-low", 1, Some("default")),
            codex("internal-high", 9, Some("internal")),
            codex("public-mid", 5, Some("default")),
        ];
        let prepared = prepare_picker_models(&models, true);
        let ids: Vec<&str> = prepared.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["internal-high", "public-mid", "public-low"]);
    }

    #[test]
    fn prepare_picker_models_does_not_filter_non_codex_visibility() {
        let mut anthropic = other("opus", "anthropic");
        anthropic.visibility = Some("internal".into());
        let prepared = prepare_picker_models(&[anthropic], false);
        assert_eq!(prepared.len(), 1, "non-codex models bypass the filter");
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TabState {
    pub(crate) matches: Vec<String>,
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
