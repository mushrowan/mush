use std::sync::Arc;
use std::sync::nonpoison::Mutex;

#[derive(Debug, Clone, Default)]
pub struct ModelSelection(Arc<Mutex<Selection>>);

#[derive(Debug, Default)]
struct Selection {
    current: String,
    subagent: Option<String>,
    default_subagent: Option<String>,
    thinking: Option<mush_ai::types::ThinkingLevel>,
    accounts: mush_ai::types::SessionAccounts,
}

impl ModelSelection {
    #[must_use]
    pub fn new(current: String, subagent: Option<String>) -> Self {
        Self(Arc::new(Mutex::new(Selection {
            current,
            subagent,
            default_subagent: None,
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

    pub fn set_default_subagent(&self, id: Option<String>) {
        self.0.lock().default_subagent = id;
    }

    #[must_use]
    pub fn default_subagent(&self) -> Option<String> {
        self.0.lock().default_subagent.clone()
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
            .or_else(|| selection.default_subagent.clone())
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
            if *level == ThinkingLevel::Off {
                return Ok(model.clamp_thinking(level));
            }
            if listed {
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub also_on: Vec<String>,
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
            also_on: Vec::new(),
        }
    }
}

fn spawnable_thinking(model: &mush_ai::types::Model) -> Vec<String> {
    if !model.reasoning {
        return vec!["off".into()];
    }
    let off = (model.clamp_thinking(&mush_ai::types::ThinkingLevel::Off)
        == mush_ai::types::ThinkingLevel::Off)
        .then_some("off");
    off.into_iter()
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

fn provider_rank(provider: &mush_ai::types::Provider, order: &[String]) -> usize {
    let name = provider.to_string();
    order
        .iter()
        .position(|preferred| *preferred == name)
        .unwrap_or(order.len())
}

fn dated(id: &str) -> bool {
    id.rsplit_once('-')
        .is_some_and(|(_, tail)| tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()))
}

fn preference(model: &mush_ai::types::Model, order: &[String]) -> (usize, bool, usize, String) {
    (
        provider_rank(&model.provider, order),
        dated(model.id.as_str()),
        model.id.as_str().len(),
        model.id.to_string(),
    )
}

#[must_use]
pub fn find_models(
    catalogue: &[mush_ai::discovery::MergedModel],
    query: &str,
    limit: usize,
    order: &[String],
    all_providers: bool,
    signed_in: impl Fn(&mush_ai::types::Provider) -> bool,
) -> Vec<ModelChoice> {
    let query = search_text(query);
    let mut providers = std::collections::HashMap::new();
    let mut groups: Vec<(usize, Vec<&mush_ai::types::Model>)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in catalogue {
        if entry.source == mush_ai::discovery::ModelSource::DiscoveredStale {
            continue;
        }
        let Some(rank) = match_rank(&query, &entry.model) else {
            continue;
        };
        let usable = *providers
            .entry(entry.model.provider.to_string())
            .or_insert_with(|| signed_in(&entry.model.provider));
        if !usable {
            continue;
        }
        let key = if all_providers {
            format!("{}/{}", entry.model.provider, entry.model.id)
        } else {
            mush_ai::models::canonical_model_key(entry.model.id.as_str())
        };
        match index.get(&key) {
            Some(&at) => {
                groups[at].0 = groups[at].0.min(rank);
                groups[at].1.push(&entry.model);
            }
            None => {
                index.insert(key, groups.len());
                groups.push((rank, vec![&entry.model]));
            }
        }
    }
    let mut choices: Vec<(usize, &mush_ai::types::Model, Vec<String>)> = groups
        .into_iter()
        .filter_map(|(rank, members)| {
            let best = *members
                .iter()
                .min_by_key(|model| preference(model, order))?;
            let mut also_on: Vec<String> = members
                .iter()
                .map(|model| model.provider.to_string())
                .filter(|provider| *provider != best.provider.to_string())
                .collect();
            also_on.sort();
            also_on.dedup();
            Some((rank, best, also_on))
        })
        .collect();
    choices.sort_by(|(a_rank, a, _), (b_rank, b, _)| {
        a_rank
            .cmp(b_rank)
            .then_with(|| preference(a, order).cmp(&preference(b, order)))
            .then_with(|| a.provider.to_string().cmp(&b.provider.to_string()))
    });
    choices
        .into_iter()
        .take(limit)
        .map(|(_, model, also_on)| ModelChoice {
            also_on,
            ..ModelChoice::of(model)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lineage {
    pub family: String,
    pub version: Vec<u64>,
    pub variant_words: usize,
}

#[must_use]
pub fn lineage(entry: &mush_ai::discovery::MergedModel) -> Option<Lineage> {
    let key = mush_ai::models::canonical_model_key(entry.model.id.as_str());
    if key.contains(':') {
        return None;
    }
    let tokens: Vec<&str> = key.split('-').filter(|token| !token.is_empty()).collect();
    let is_word = |token: &str| token.bytes().all(|b| b.is_ascii_alphabetic());
    let words: Vec<&str> = tokens
        .iter()
        .copied()
        .filter(|token| is_word(token))
        .collect();
    let (parsed, variant_words) = match tokens.first() {
        Some(&"claude") => (words.get(1).copied(), words.len().saturating_sub(2)),
        Some(first) if is_word(first) => (Some(*first), words.len() - 1),
        _ => (None, 0),
    };
    let line = entry
        .raw
        .as_ref()
        .and_then(|raw| raw.get("line"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_ascii_lowercase);
    let family = line.or_else(|| parsed.map(str::to_string))?;
    let version: Vec<u64> = tokens
        .iter()
        .filter_map(|token| token.parse::<u64>().ok())
        .collect();
    Some(Lineage {
        family,
        version,
        variant_words,
    })
}

fn created_at(entry: &mush_ai::discovery::MergedModel) -> Option<&str> {
    entry
        .raw
        .as_ref()
        .and_then(|raw| raw.get("created_at"))
        .and_then(serde_json::Value::as_str)
}

#[must_use]
pub fn newest_of_family<'a>(
    catalogue: &'a [mush_ai::discovery::MergedModel],
    family: &str,
    order: &[String],
    signed_in: impl Fn(&mush_ai::types::Provider) -> bool,
) -> Option<&'a mush_ai::discovery::MergedModel> {
    let family = family.to_ascii_lowercase();
    catalogue
        .iter()
        .filter(|entry| entry.source != mush_ai::discovery::ModelSource::DiscoveredStale)
        .filter_map(|entry| Some((lineage(entry)?, entry)))
        .filter(|(lineage, _)| lineage.family == family && !lineage.version.is_empty())
        .filter(|(_, entry)| signed_in(&entry.model.provider))
        .min_by(|(a, a_entry), (b, b_entry)| {
            b.version
                .cmp(&a.version)
                .then_with(|| a.variant_words.cmp(&b.variant_words))
                .then_with(|| created_at(b_entry).cmp(&created_at(a_entry)))
                .then_with(|| {
                    preference(&a_entry.model, order).cmp(&preference(&b_entry.model, order))
                })
        })
        .map(|(_, entry)| entry)
}

fn family_name(requested: &str) -> Option<String> {
    let lowered = requested.to_ascii_lowercase();
    let name = lowered.strip_prefix("claude-").unwrap_or(&lowered);
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphabetic())).then(|| name.to_string())
}

pub const MODELS_NOTICE_TAG: &str = "[models]";
const DEFAULT_CHILD: &str = "default child model";
const PREFERRED: &str = "preferred providers";
const SIGNED_IN: &str = "providers with credentials";

#[must_use]
pub fn spawn_view(
    catalogue: &[mush_ai::discovery::MergedModel],
    preferred: &[String],
    own_provider: Option<&str>,
    default_child: Option<&str>,
    signed_in: impl Fn(&mush_ai::types::Provider) -> bool,
) -> std::collections::BTreeMap<String, String> {
    let mut providers = std::collections::BTreeMap::new();
    for entry in catalogue {
        providers
            .entry(entry.model.provider.to_string())
            .or_insert_with(|| signed_in(&entry.model.provider));
    }
    let usable = |provider: &mush_ai::types::Provider| {
        providers
            .get(&provider.to_string())
            .copied()
            .unwrap_or(false)
    };
    let order: Vec<String> = if preferred.is_empty() {
        own_provider.map(str::to_string).into_iter().collect()
    } else {
        preferred.to_vec()
    };
    let mut view = std::collections::BTreeMap::new();
    if let Some(child) = default_child {
        view.insert(DEFAULT_CHILD.to_string(), child.to_string());
    }
    if !preferred.is_empty() {
        view.insert(PREFERRED.to_string(), preferred.join(", "));
    }
    let signed: Vec<&str> = providers
        .iter()
        .filter(|(_, usable)| **usable)
        .map(|(name, _)| name.as_str())
        .collect();
    view.insert(SIGNED_IN.to_string(), signed.join(", "));
    let families: std::collections::BTreeSet<String> = catalogue
        .iter()
        .filter(|entry| order.contains(&entry.model.provider.to_string()))
        .filter(|entry| usable(&entry.model.provider))
        .filter_map(|entry| lineage(entry).map(|lineage| lineage.family))
        .collect();
    for family in families {
        if let Some(newest) = newest_of_family(catalogue, &family, &order, usable) {
            view.insert(
                family,
                format!("{}/{}", newest.model.provider, newest.model.id),
            );
        }
    }
    view
}

#[must_use]
pub fn models_notice(
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let lines: Vec<String> = keys
        .into_iter()
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|key| {
            let now = after.get(key);
            match (key.as_str(), now) {
                (DEFAULT_CHILD, Some(model)) => format!("children now default to {model}"),
                (DEFAULT_CHILD, None) => {
                    "children now default to the model their parent runs".into()
                }
                (PREFERRED, Some(order)) => format!("preferred providers: {order}"),
                (PREFERRED, None) => "no preferred providers".into(),
                (SIGNED_IN, Some(list)) if list.is_empty() => "no provider has credentials".into(),
                (SIGNED_IN, Some(list)) => format!("providers with credentials: {list}"),
                (family, Some(model)) => format!("{family} now resolves to {model}"),
                (family, None) => format!("{family} no longer resolves to a model"),
            }
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "{MODELS_NOTICE_TAG}\n\n{}. `await rlm.find_models(query)` lists what a child can run.",
            lines.join("; ")
        )
    })
}

#[must_use]
pub fn provider_order(
    preferred: Vec<String>,
    caller_model: Option<&str>,
    catalogue: &[mush_ai::discovery::MergedModel],
) -> Vec<String> {
    if !preferred.is_empty() {
        return preferred;
    }
    caller_model
        .and_then(|id| catalogue.iter().find(|entry| entry.model.id.as_str() == id))
        .map(|entry| vec![entry.model.provider.to_string()])
        .unwrap_or_default()
}

pub fn resolve_child_model(
    catalogue: &[mush_ai::discovery::MergedModel],
    requested: &str,
    order: &[String],
    signed_in: impl Fn(&mush_ai::types::Provider) -> bool,
) -> Result<mush_ai::types::Model, String> {
    let pinned = requested.split_once('/').and_then(|(provider, id)| {
        catalogue.iter().find(|entry| {
            entry.model.provider.to_string() == provider && entry.model.id.as_str() == id
        })
    });
    let candidates: Vec<&mush_ai::discovery::MergedModel> = match pinned {
        Some(entry) => vec![entry],
        None => {
            let key = mush_ai::models::canonical_model_key(requested);
            catalogue
                .iter()
                .filter(|entry| {
                    mush_ai::models::canonical_model_key(entry.model.id.as_str()) == key
                })
                .collect()
        }
    };
    if candidates.is_empty()
        && let Some(family) = family_name(requested)
        && let Some(newest) = newest_of_family(catalogue, &family, order, &signed_in)
    {
        return Ok(newest.model.clone());
    }
    if let Some(best) = candidates
        .iter()
        .filter(|entry| signed_in(&entry.model.provider))
        .min_by_key(|entry| {
            (
                provider_rank(&entry.model.provider, order),
                entry.model.id.as_str() != requested,
                entry.source == mush_ai::discovery::ModelSource::DiscoveredStale,
                dated(entry.model.id.as_str()),
            )
        })
    {
        return Ok(best.model.clone());
    }
    let close: Vec<String> = std::iter::once(requested)
        .chain(requested.rsplit_once('/').map(|(_, bare)| bare))
        .map(|query| find_models(catalogue, query, CLOSE_MATCHES, order, false, &signed_in))
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
    let mut providers: Vec<String> = candidates
        .iter()
        .map(|entry| entry.model.provider.to_string())
        .collect();
    providers.dedup();
    let problem = if providers.is_empty() {
        format!("unknown model {requested}.")
    } else {
        format!(
            "{requested} runs on {}, which has no credentials here, so a child cannot \
             use it. Sign in with /login or pick another model.",
            providers.join(" or ")
        )
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
        model.supported_thinking_levels = Vec::new();
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
            mush_ai::types::ThinkingLevel::Off,
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
            listed(
                "claude-haiku-4-5-20251001",
                Provider::Anthropic,
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
        let none: &[String] = &[];

        let haiku = find_models(&catalogue, "haiku", 10, none, false, anthropic_only);
        assert_eq!(ids(&haiku), ["claude-haiku-4-5", "claude-haiku-5-5"]);

        let exact = find_models(&catalogue, "Claude Haiku 5.5", 10, none, false, everyone);
        assert_eq!(exact[0].id, "claude-haiku-5-5");

        let all = find_models(&catalogue, "", 10, none, false, everyone);
        assert_eq!(
            all.len(),
            3,
            "one row per model, and the stale one is left out"
        );
        let every_row = find_models(&catalogue, "", 10, none, true, everyone);
        assert_eq!(every_row.len(), 5);
        assert_eq!(
            find_models(&catalogue, "", 2, none, false, everyone).len(),
            2
        );
        assert!(find_models(&catalogue, "gemini", 10, none, false, everyone).is_empty());
    }

    #[test]
    fn one_row_per_model_comes_from_the_preferred_provider_and_names_the_others() {
        let catalogue = catalogue();
        let everyone = |_: &mush_ai::types::Provider| true;
        let row = |order: &[String]| {
            find_models(&catalogue, "claude-haiku-4", 10, order, false, everyone)
                .into_iter()
                .next()
                .unwrap()
        };

        let direct = row(&["anthropic".to_string()]);
        assert_eq!(direct.id, "claude-haiku-4-5");
        assert_eq!(direct.also_on, ["openrouter"]);

        let routed = row(&["openrouter".to_string(), "anthropic".to_string()]);
        assert_eq!(routed.id, "anthropic/claude-haiku-4.5");
        assert_eq!(routed.also_on, ["anthropic"]);

        let only_direct = find_models(&catalogue, "claude-haiku-4", 10, &[], false, anthropic_only);
        assert!(
            only_direct[0].also_on.is_empty(),
            "a provider without credentials is not offered"
        );
    }

    #[test]
    fn a_bare_id_goes_to_the_preferred_provider_and_a_provider_prefix_pins_it() {
        let catalogue = catalogue();
        let everyone = |_: &mush_ai::types::Provider| true;
        let resolve = |requested: &str, order: &[&str]| {
            let order: Vec<String> = order.iter().map(ToString::to_string).collect();
            resolve_child_model(&catalogue, requested, &order, everyone)
                .map(|model| format!("{}/{}", model.provider, model.id))
        };

        assert_eq!(
            resolve("claude-haiku-4-5", &["openrouter", "anthropic"]).unwrap(),
            "openrouter/anthropic/claude-haiku-4.5"
        );
        assert_eq!(
            resolve("claude-haiku-4-5", &["anthropic"]).unwrap(),
            "anthropic/claude-haiku-4-5"
        );
        assert_eq!(
            resolve("claude-haiku-4-5", &[]).unwrap(),
            "anthropic/claude-haiku-4-5",
            "with no order an exact id wins"
        );
        assert_eq!(
            resolve("anthropic/claude-haiku-4.5", &["anthropic"]).unwrap(),
            "anthropic/claude-haiku-4-5"
        );
        assert_eq!(
            resolve("openrouter/anthropic/claude-haiku-4.5", &["anthropic"]).unwrap(),
            "openrouter/anthropic/claude-haiku-4.5"
        );
        assert_eq!(
            resolve("claude-haiku-4-5-20251001", &["anthropic"]).unwrap(),
            "anthropic/claude-haiku-4-5-20251001",
            "a dated snapshot asked for by name is kept"
        );
    }

    #[test]
    fn a_family_name_spawns_the_newest_model_of_the_family() {
        use mush_ai::discovery::ModelSource;
        use mush_ai::types::Provider;
        let mut catalogue = catalogue();
        let everyone = |_: &mush_ai::types::Provider| true;
        let resolve =
            |catalogue: &[mush_ai::discovery::MergedModel], requested: &str, order: &[&str]| {
                let order: Vec<String> = order.iter().map(ToString::to_string).collect();
                resolve_child_model(catalogue, requested, &order, everyone)
                    .map(|model| format!("{}/{}", model.provider, model.id))
            };

        assert_eq!(
            resolve(&catalogue, "haiku", &["anthropic"]).unwrap(),
            "anthropic/claude-haiku-5-5"
        );
        assert_eq!(
            resolve(&catalogue, "Claude-Haiku", &["anthropic"]).unwrap(),
            "anthropic/claude-haiku-5-5"
        );
        assert_eq!(
            resolve(&catalogue, "opus", &[]).unwrap(),
            "anthropic/claude-opus-5-5"
        );

        catalogue.push(listed(
            "anthropic/claude-haiku-5.5",
            Provider::OpenRouter,
            ModelSource::Discovered,
        ));
        catalogue.push(listed(
            "claude-opus-5-5-fast",
            Provider::Anthropic,
            ModelSource::Discovered,
        ));
        assert_eq!(
            resolve(&catalogue, "haiku", &["openrouter", "anthropic"]).unwrap(),
            "openrouter/anthropic/claude-haiku-5.5",
            "the same newest model goes to the preferred provider"
        );
        assert_eq!(
            resolve(&catalogue, "opus", &[]).unwrap(),
            "anthropic/claude-opus-5-5",
            "a variant ranks after the plain model"
        );

        catalogue.push(listed(
            "anthropic/claude-haiku-6",
            Provider::OpenRouter,
            ModelSource::Discovered,
        ));
        assert_eq!(
            resolve(&catalogue, "haiku", &["anthropic"]).unwrap(),
            "openrouter/anthropic/claude-haiku-6",
            "a newer model beats the preferred provider"
        );
        let direct_only = resolve_child_model(&catalogue, "haiku", &[], anthropic_only).unwrap();
        assert_eq!(direct_only.id.as_str(), "claude-haiku-5-5");

        assert!(resolve(&catalogue, "sonet", &[]).is_err());
    }

    #[test]
    fn lineage_reads_the_family_version_and_variant_words() {
        use mush_ai::discovery::ModelSource;
        use mush_ai::types::Provider;
        let lineage_of =
            |id: &str| lineage(&listed(id, Provider::Anthropic, ModelSource::Static)).unwrap();
        assert_eq!(
            lineage_of("claude-haiku-4-5-20251001"),
            Lineage {
                family: "haiku".into(),
                version: vec![4, 5],
                variant_words: 0
            }
        );
        assert_eq!(lineage_of("claude-3-5-sonnet").family, "sonnet");
        assert_eq!(lineage_of("claude-opus-4-8-fast").variant_words, 1);
        assert_eq!(lineage_of("gpt-5.4-mini").family, "gpt");
        assert_eq!(lineage_of("gpt-5.4-mini").version, vec![5, 4]);
        let mut with_line = listed("claude-x-1", Provider::Anthropic, ModelSource::Discovered);
        with_line.raw = Some(serde_json::json!({ "line": "Sonnet" }));
        assert_eq!(lineage(&with_line).unwrap().family, "sonnet");
    }

    #[test]
    fn the_spawn_view_names_defaults_providers_and_what_each_family_resolves_to() {
        let catalogue = catalogue();
        let view = spawn_view(&catalogue, &[], Some("anthropic"), None, anthropic_only);
        assert_eq!(view["providers with credentials"], "anthropic");
        assert_eq!(view["haiku"], "anthropic/claude-haiku-5-5");
        assert_eq!(view["opus"], "anthropic/claude-opus-5-5");
        assert!(!view.contains_key("default child model"));
        assert!(!view.contains_key("preferred providers"));

        let everyone = |_: &mush_ai::types::Provider| true;
        let routed = spawn_view(
            &catalogue,
            &["openrouter".to_string()],
            Some("anthropic"),
            Some("haiku"),
            everyone,
        );
        assert_eq!(
            routed["providers with credentials"],
            "anthropic, openrouter"
        );
        assert_eq!(routed["preferred providers"], "openrouter");
        assert_eq!(routed["default child model"], "haiku");
        assert_eq!(
            routed["haiku"], "anthropic/claude-haiku-5-5",
            "the newest model wins even off the preferred provider"
        );
    }

    #[test]
    fn the_models_notice_lists_only_what_changed() {
        let view = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        };
        let before = view(&[
            ("providers with credentials", "anthropic"),
            ("haiku", "anthropic/claude-haiku-4-5"),
            ("opus", "anthropic/claude-opus-5-5"),
        ]);
        assert_eq!(models_notice(&before, &before), None);

        let after = view(&[
            ("providers with credentials", "anthropic, openrouter"),
            ("haiku", "anthropic/claude-haiku-5-5"),
            ("opus", "anthropic/claude-opus-5-5"),
            ("default child model", "haiku"),
        ]);
        let notice = models_notice(&before, &after).unwrap();
        assert!(notice.starts_with("[models]\n\n"), "{notice}");
        assert!(notice.contains("children now default to haiku"), "{notice}");
        assert!(
            notice.contains("haiku now resolves to anthropic/claude-haiku-5-5"),
            "{notice}"
        );
        assert!(
            notice.contains("providers with credentials: anthropic, openrouter"),
            "{notice}"
        );
        assert!(!notice.contains("opus"), "{notice}");
        assert!(notice.contains("rlm.find_models"), "{notice}");

        let back = models_notice(&after, &before).unwrap();
        assert!(
            back.contains("children now default to the model their parent runs"),
            "{back}"
        );
    }

    #[test]
    fn the_order_falls_back_to_the_provider_the_caller_runs_on() {
        let catalogue = catalogue();
        assert_eq!(
            provider_order(
                vec!["openrouter".into()],
                Some("claude-haiku-5-5"),
                &catalogue
            ),
            ["openrouter"]
        );
        assert_eq!(
            provider_order(Vec::new(), Some("anthropic/claude-haiku-4.5"), &catalogue),
            ["openrouter"]
        );
        assert!(provider_order(Vec::new(), None, &catalogue).is_empty());
    }

    #[test]
    fn a_model_choice_shows_limits_thinking_levels_and_cost() {
        let catalogue = catalogue();
        let found = find_models(
            &catalogue,
            "claude-haiku-5-5",
            1,
            &[],
            false,
            anthropic_only,
        );
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

        let old = &find_models(
            &catalogue,
            "claude-haiku-4-5",
            1,
            &[],
            false,
            anthropic_only,
        )[0];
        assert_eq!(old.max_output_tokens, 64_000);
        assert_eq!(
            old.thinking,
            ["off", "minimal", "low", "medium", "high", "xhigh"]
        );

        let mut always_on = catalogue[0].model.clone();
        always_on.supported_thinking_levels.remove(0);
        assert_eq!(
            ModelChoice::of(&always_on).thinking,
            ["low", "medium", "high", "xhigh", "max"],
            "a model that cannot switch thinking off does not offer off"
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
        let none: &[String] = &[];

        let haiku =
            resolve_child_model(&catalogue, "claude-haiku-5-5", none, anthropic_only).unwrap();
        assert_eq!(haiku.id.as_str(), "claude-haiku-5-5");

        let unknown =
            resolve_child_model(&catalogue, "claude-haiku-4", none, anthropic_only).unwrap_err();
        assert!(
            unknown.starts_with("unknown model claude-haiku-4."),
            "{unknown}"
        );
        assert!(
            unknown.contains("Close matches: claude-haiku-4-5."),
            "{unknown}"
        );
        assert!(
            unknown.contains("await rlm.find_models(query)"),
            "{unknown}"
        );

        let twin = resolve_child_model(
            &catalogue,
            "anthropic/claude-haiku-4.5",
            none,
            anthropic_only,
        )
        .unwrap();
        assert_eq!(
            twin.id.as_str(),
            "claude-haiku-4-5",
            "the openrouter id lands on the twin that has credentials"
        );

        let signed_out = resolve_child_model(
            &catalogue,
            "openrouter/anthropic/claude-haiku-4.5",
            none,
            anthropic_only,
        )
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
            resolve_child_model(&catalogue, "gemini-9", none, anthropic_only).unwrap_err();
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
            Ok(ThinkingLevel::Low),
            "a model that cannot think less runs at its lowest level"
        );
        assert_eq!(
            child_thinking(
                &model,
                Some(&ThinkingLevel::Off),
                &ThinkingLevel::High,
                true
            ),
            Ok(ThinkingLevel::Low)
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

        model
            .supported_thinking_levels
            .insert(0, ThinkingLevel::Off);
        assert_eq!(
            child_thinking(&model, None, &ThinkingLevel::Off, true),
            Ok(ThinkingLevel::Off)
        );
        assert_eq!(
            child_thinking(
                &model,
                Some(&ThinkingLevel::Off),
                &ThinkingLevel::High,
                false
            ),
            Ok(ThinkingLevel::Off)
        );
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

        models.set_default_subagent(Some("settings-default".into()));
        assert_eq!(models.for_child(None).as_deref(), Some("settings-default"));
        models.set_subagent(Some("cheap".into()));
        assert_eq!(models.for_child(None).as_deref(), Some("cheap"));
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
