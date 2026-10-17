use std::sync::Arc;
use std::sync::nonpoison::Mutex;

#[derive(Debug, Clone, Default)]
pub struct ModelSelection(Arc<Mutex<Selection>>);

#[derive(Debug, Default)]
struct Selection {
    current: String,
    subagent: Option<String>,
    thinking: Option<mush_ai::types::ThinkingLevel>,
    accounts: mush_ai::types::SessionAccounts,
}

impl ModelSelection {
    #[must_use]
    pub fn new(current: String, subagent: Option<String>) -> Self {
        Self(Arc::new(Mutex::new(Selection {
            current,
            subagent,
            thinking: None,
            accounts: mush_ai::types::SessionAccounts::default(),
        })))
    }

    pub fn set_current(&self, id: String) {
        self.0.lock().current = id;
    }

    #[must_use]
    pub fn current(&self) -> String {
        self.0.lock().current.clone()
    }

    pub fn set_thinking(&self, level: mush_ai::types::ThinkingLevel) {
        self.0.lock().thinking = Some(level);
    }

    #[must_use]
    pub fn thinking(&self) -> Option<mush_ai::types::ThinkingLevel> {
        self.0.lock().thinking.clone()
    }

    pub fn set_subagent(&self, id: Option<String>) {
        self.0.lock().subagent = id;
    }

    #[must_use]
    pub fn subagent(&self) -> Option<String> {
        self.0.lock().subagent.clone()
    }

    pub fn set_accounts(&self, accounts: mush_ai::types::SessionAccounts) {
        self.0.lock().accounts = accounts;
    }

    #[must_use]
    pub fn accounts(&self) -> mush_ai::types::SessionAccounts {
        self.0.lock().accounts.clone()
    }

    #[must_use]
    pub fn for_child(&self, requested: Option<&str>) -> Option<String> {
        let selection = self.0.lock();
        requested
            .map(str::to_string)
            .or_else(|| selection.subagent.clone())
    }
}

pub fn child_thinking(
    model: &mush_ai::types::Model,
    requested: Option<&mush_ai::types::ThinkingLevel>,
    parent: &mush_ai::types::ThinkingLevel,
    same_model: bool,
) -> Result<mush_ai::types::ThinkingLevel, String> {
    use mush_ai::types::ThinkingLevel;
    let supported = &model.supported_thinking_levels;
    match requested {
        Some(level) => {
            let listed = if supported.is_empty() {
                ThinkingLevel::LADDER.contains(level)
            } else {
                supported.contains(level)
            };
            if *level == ThinkingLevel::Off || listed {
                return Ok(level.clone());
            }
            let names: Vec<&str> = std::iter::once("off")
                .chain(thinking_ladder(model).filter_map(ThinkingLevel::effort_name))
                .collect();
            Err(format!(
                "thinking level {} is not supported by {}, supported levels: {}",
                level.effort_name().unwrap_or("off"),
                model.id,
                names.join(", ")
            ))
        }
        None if !same_model => Ok(middle_thinking(model)),
        None if *parent == ThinkingLevel::Off => Ok(ThinkingLevel::Off),
        None => Ok(model.clamp_thinking(parent)),
    }
}

fn thinking_ladder(
    model: &mush_ai::types::Model,
) -> impl Iterator<Item = &mush_ai::types::ThinkingLevel> {
    use mush_ai::types::ThinkingLevel;
    let listed = &model.supported_thinking_levels;
    let ladder: &[ThinkingLevel] = if listed.is_empty() {
        &ThinkingLevel::LADDER
    } else {
        listed
    };
    ladder.iter().filter(|level| **level != ThinkingLevel::Off)
}

#[must_use]
pub fn middle_thinking(model: &mush_ai::types::Model) -> mush_ai::types::ThinkingLevel {
    use mush_ai::types::ThinkingLevel;
    if !model.reasoning {
        return ThinkingLevel::Off;
    }
    let ladder: Vec<&ThinkingLevel> = thinking_ladder(model).collect();
    ladder
        .get(ladder.len().saturating_sub(1) / 2)
        .map_or(ThinkingLevel::Off, |level| (*level).clone())
}

pub const SPAWN_THINKING: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
pub const MODEL_SEARCH_LIMIT: usize = 10;
pub const MAX_MODEL_SEARCH_LIMIT: usize = 50;
const CLOSE_MATCHES: usize = 3;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub thinking: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelChoiceCost>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct ModelChoiceCost {
    pub input: f64,
    pub output: f64,
}

impl ModelChoice {
    fn of(model: &mush_ai::types::Model) -> Self {
        let cost = (model.cost.input > 0.0 || model.cost.output > 0.0).then_some(ModelChoiceCost {
            input: model.cost.input,
            output: model.cost.output,
        });
        Self {
            id: model.id.to_string(),
            name: model.name.clone(),
            provider: model.provider.to_string(),
            context_window: model.context_window.get(),
            max_output_tokens: model.max_output_tokens.get(),
            thinking: spawnable_thinking(model),
            cost,
        }
    }
}

fn spawnable_thinking(model: &mush_ai::types::Model) -> Vec<String> {
    if !model.reasoning {
        return vec!["off".into()];
    }
    std::iter::once("off")
        .chain(
            thinking_ladder(model)
                .filter_map(mush_ai::types::ThinkingLevel::effort_name)
                .filter(|name| SPAWN_THINKING.contains(name)),
        )
        .map(str::to_string)
        .collect()
}

fn search_text(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn match_rank(query: &str, model: &mush_ai::types::Model) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let fields = [search_text(model.id.as_str()), search_text(&model.name)];
    let found = |hit: fn(&str, &str) -> bool| fields.iter().position(|field| hit(field, query));
    found(|field, wanted| field == wanted)
        .or_else(|| found(|field, wanted| field.starts_with(wanted)).map(|at| at + 2))
        .or_else(|| found(|field, wanted| field.contains(wanted)).map(|at| at + 4))
}

#[must_use]
pub fn find_models(
    catalogue: &[mush_ai::discovery::MergedModel],
    query: &str,
    limit: usize,
    signed_in: impl Fn(&mush_ai::types::Provider) -> bool,
) -> Vec<ModelChoice> {
    let query = search_text(query);
    let mut providers = std::collections::HashMap::new();
    let mut ranked: Vec<(usize, &mush_ai::types::Model)> = catalogue
        .iter()
        .filter(|entry| entry.source != mush_ai::discovery::ModelSource::DiscoveredStale)
        .filter_map(|entry| match_rank(&query, &entry.model).map(|rank| (rank, &entry.model)))
        .filter(|(_, model)| {
            *providers
                .entry(model.provider.to_string())
                .or_insert_with(|| signed_in(&model.provider))
        })
        .collect();
    ranked.sort_by(|(a_rank, a), (b_rank, b)| {
        a_rank
            .cmp(b_rank)
            .then_with(|| a.provider.to_string().cmp(&b.provider.to_string()))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, model)| ModelChoice::of(model))
        .collect()
}

pub fn resolve_child_model(
    catalogue: &[mush_ai::discovery::MergedModel],
    id: &str,
    signed_in: impl Fn(&mush_ai::types::Provider) -> bool,
) -> Result<mush_ai::types::Model, String> {
    let found = catalogue.iter().find(|entry| entry.model.id.as_str() == id);
    if let Some(entry) = found
        && signed_in(&entry.model.provider)
    {
        return Ok(entry.model.clone());
    }
    let close: Vec<String> = std::iter::once(id)
        .chain(id.rsplit_once('/').map(|(_, bare)| bare))
        .map(|query| find_models(catalogue, query, CLOSE_MATCHES, &signed_in))
        .find(|found| !found.is_empty())
        .unwrap_or_default()
        .into_iter()
        .map(|choice| choice.id)
        .collect();
    let close = if close.is_empty() {
        String::new()
    } else {
        format!(" Close matches: {}.", close.join(", "))
    };
    let problem = match found {
        Some(entry) => format!(
            "{id} runs on {provider}, which has no credentials here, so a child cannot \
             use it. Sign in with /login or pick another model.",
            provider = entry.model.provider
        ),
        None => format!("unknown model {id}."),
    };
    Err(format!(
        "{problem}{close} `await rlm.find_models(query)` lists the models a child can run."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(
        id: &str,
        provider: mush_ai::types::Provider,
        source: mush_ai::discovery::ModelSource,
    ) -> mush_ai::discovery::MergedModel {
        let mut model = crate::testing::echo_model();
        model.id = id.into();
        model.name = id.replace('-', " ");
        model.provider = provider;
        mush_ai::discovery::MergedModel {
            model,
            source,
            raw: None,
        }
    }

    fn catalogue() -> Vec<mush_ai::discovery::MergedModel> {
        use mush_ai::discovery::ModelSource;
        use mush_ai::types::Provider;
        let mut haiku_4_5 = listed("claude-haiku-4-5", Provider::Anthropic, ModelSource::Static);
        haiku_4_5.model.max_output_tokens = mush_ai::types::TokenCount::new(64_000);
        let mut haiku_5_5 = listed("claude-haiku-5-5", Provider::Anthropic, ModelSource::Static);
        haiku_5_5.model.supported_thinking_levels = vec![
            mush_ai::types::ThinkingLevel::Low,
            mush_ai::types::ThinkingLevel::Medium,
            mush_ai::types::ThinkingLevel::High,
            mush_ai::types::ThinkingLevel::Xhigh,
            mush_ai::types::ThinkingLevel::Other("max".into()),
        ];
        vec![
            haiku_5_5,
            haiku_4_5,
            listed(
                "claude-opus-5-5",
                Provider::Anthropic,
                ModelSource::Discovered,
            ),
            listed(
                "claude-haiku-3",
                Provider::Anthropic,
                ModelSource::DiscoveredStale,
            ),
            listed(
                "anthropic/claude-haiku-4.5",
                Provider::OpenRouter,
                ModelSource::Discovered,
            ),
        ]
    }

    fn anthropic_only(provider: &mush_ai::types::Provider) -> bool {
        *provider == mush_ai::types::Provider::Anthropic
    }

    fn ids(choices: &[ModelChoice]) -> Vec<&str> {
        choices.iter().map(|choice| choice.id.as_str()).collect()
    }

    #[test]
    fn model_search_ranks_exact_then_prefix_then_substring_and_skips_what_cannot_run() {
        let catalogue = catalogue();
        let everyone = |_: &mush_ai::types::Provider| true;

        let haiku = find_models(&catalogue, "haiku", 10, anthropic_only);
        assert_eq!(ids(&haiku), ["claude-haiku-4-5", "claude-haiku-5-5"]);

        let exact = find_models(&catalogue, "Claude Haiku 5.5", 10, everyone);
        assert_eq!(exact[0].id, "claude-haiku-5-5");
        let prefix = find_models(&catalogue, "claude-haiku-4", 10, everyone);
        assert_eq!(
            ids(&prefix),
            ["claude-haiku-4-5", "anthropic/claude-haiku-4.5"]
        );

        let all = find_models(&catalogue, "", 10, everyone);
        assert_eq!(all.len(), 4, "the stale model is left out");
        assert_eq!(find_models(&catalogue, "", 2, everyone).len(), 2);
        assert!(find_models(&catalogue, "gemini", 10, everyone).is_empty());
    }

    #[test]
    fn a_model_choice_shows_limits_thinking_levels_and_cost() {
        let catalogue = catalogue();
        let found = find_models(&catalogue, "claude-haiku-5-5", 1, anthropic_only);
        let haiku = &found[0];
        assert_eq!(haiku.provider, "anthropic");
        assert_eq!(
            haiku.context_window,
            catalogue[0].model.context_window.get()
        );
        assert_eq!(
            haiku.thinking,
            ["off", "low", "medium", "high", "xhigh", "max"]
        );
        assert!(haiku.cost.is_some());

        let old = &find_models(&catalogue, "claude-haiku-4-5", 1, anthropic_only)[0];
        assert_eq!(old.max_output_tokens, 64_000);
        assert_eq!(
            old.thinking,
            ["off", "minimal", "low", "medium", "high", "xhigh"]
        );

        let mut plain = catalogue[1].model.clone();
        plain.reasoning = false;
        plain.cost = mush_ai::types::ModelCost::default();
        let plain = ModelChoice::of(&plain);
        assert_eq!(plain.thinking, ["off"]);
        assert_eq!(plain.cost, None);
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("cost").is_none(), "{json}");
    }

    #[test]
    fn a_child_model_must_be_known_and_have_credentials() {
        let catalogue = catalogue();

        let haiku = resolve_child_model(&catalogue, "claude-haiku-5-5", anthropic_only).unwrap();
        assert_eq!(haiku.id.as_str(), "claude-haiku-5-5");

        let unknown = resolve_child_model(&catalogue, "haiku", anthropic_only).unwrap_err();
        assert!(unknown.starts_with("unknown model haiku."), "{unknown}");
        assert!(
            unknown.contains("Close matches: claude-haiku-4-5, claude-haiku-5-5."),
            "{unknown}"
        );
        assert!(
            unknown.contains("await rlm.find_models(query)"),
            "{unknown}"
        );

        let signed_out =
            resolve_child_model(&catalogue, "anthropic/claude-haiku-4.5", anthropic_only)
                .unwrap_err();
        assert!(
            signed_out.contains("runs on openrouter, which has no credentials here"),
            "{signed_out}"
        );
        assert!(
            signed_out.contains("Close matches: claude-haiku-4-5"),
            "{signed_out}"
        );

        let nothing_close =
            resolve_child_model(&catalogue, "gemini-9", anthropic_only).unwrap_err();
        assert!(!nothing_close.contains("Close matches"), "{nothing_close}");
    }

    #[test]
    fn a_child_on_its_parents_model_follows_the_parent_thinking_clamped_to_the_model() {
        use mush_ai::types::ThinkingLevel;
        let mut model = crate::testing::echo_model();
        model.supported_thinking_levels = vec![ThinkingLevel::Low, ThinkingLevel::Medium];
        assert_eq!(
            child_thinking(&model, None, &ThinkingLevel::Xhigh, true),
            Ok(ThinkingLevel::Medium)
        );
        assert_eq!(
            child_thinking(&model, None, &ThinkingLevel::Off, true),
            Ok(ThinkingLevel::Off)
        );
        assert_eq!(
            child_thinking(
                &model,
                Some(&ThinkingLevel::Low),
                &ThinkingLevel::High,
                true
            ),
            Ok(ThinkingLevel::Low)
        );
        let refused = child_thinking(
            &model,
            Some(&ThinkingLevel::High),
            &ThinkingLevel::Off,
            true,
        )
        .unwrap_err();
        assert!(refused.contains("off, low, medium"), "{refused}");
    }

    #[test]
    fn a_child_on_another_model_starts_at_the_middle_of_its_ladder() {
        use mush_ai::types::ThinkingLevel;
        let max = ThinkingLevel::Other("max".into());
        let mut model = crate::testing::echo_model();
        let middle = |model: &mush_ai::types::Model| {
            child_thinking(model, None, &ThinkingLevel::Off, false).unwrap()
        };

        model.supported_thinking_levels = vec![
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
            max.clone(),
        ];
        assert_eq!(middle(&model), ThinkingLevel::High);

        model.supported_thinking_levels = vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            max.clone(),
        ];
        assert_eq!(middle(&model), ThinkingLevel::Medium, "off is not a rung");

        model.supported_thinking_levels = Vec::new();
        assert_eq!(middle(&model), ThinkingLevel::Medium);

        model.reasoning = false;
        assert_eq!(middle(&model), ThinkingLevel::Off);
    }

    #[test]
    fn a_child_can_ask_for_max_only_on_a_model_that_lists_it() {
        use mush_ai::types::ThinkingLevel;
        let max = ThinkingLevel::Other("max".into());
        let mut model = crate::testing::echo_model();
        model.supported_thinking_levels = vec![ThinkingLevel::High, max.clone()];
        assert_eq!(
            child_thinking(&model, Some(&max), &ThinkingLevel::Off, false),
            Ok(max.clone())
        );

        model.supported_thinking_levels = Vec::new();
        let refused = child_thinking(&model, Some(&max), &ThinkingLevel::Off, false).unwrap_err();
        assert!(
            refused.contains("off, minimal, low, medium, high, xhigh"),
            "{refused}"
        );
    }

    #[test]
    fn a_child_takes_what_it_asked_for_then_the_session_override() {
        let models = ModelSelection::new("parent-model".into(), None);
        assert_eq!(models.for_child(Some("asked")).as_deref(), Some("asked"));
        assert!(models.for_child(None).is_none());

        models.set_subagent(Some("cheap".into()));
        assert_eq!(models.for_child(None).as_deref(), Some("cheap"));
        assert_eq!(models.for_child(Some("asked")).as_deref(), Some("asked"));

        models.set_subagent(None);
        assert!(models.for_child(None).is_none());
    }

    #[test]
    fn a_child_sees_the_accounts_the_session_holds() {
        let models = ModelSelection::new("one".into(), None);
        assert!(models.accounts().is_empty());
        let mut accounts = mush_ai::types::SessionAccounts::default();
        accounts.set(
            "anthropic",
            Some(mush_ai::types::AccountLabel::parse("work").unwrap()),
        );
        models.set_accounts(accounts.clone());
        assert_eq!(models.accounts(), accounts);
    }

    #[test]
    fn the_current_model_follows_the_session() {
        let models = ModelSelection::new("one".into(), None);
        assert_eq!(models.current(), "one");
        models.set_current("two".into());
        assert_eq!(models.current(), "two");
        assert!(models.thinking().is_none());
        models.set_thinking(mush_ai::types::ThinkingLevel::High);
        assert_eq!(models.thinking(), Some(mush_ai::types::ThinkingLevel::High));
    }
}
