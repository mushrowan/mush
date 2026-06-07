//! merge static catalogue with discovered models.
//!
//! the static catalogue (in [`crate::models`]) is hand-maintained and
//! contains every provider/model mush ships with. discovery adds models
//! we learnt about from upstream `/v1/models` endpoints.
//!
//! three-tier precedence (highest wins):
//! 1. **overrides** - user `models.json` plus shipped corrections for
//!    routes whose upstream metadata is wrong. win over discovery
//!    wholesale (the override is authoritative), but inherit the
//!    discovered `raw` payload so provider extras still render.
//! 2. **discovery** - the live `/v1/models` view. wins over the seed for
//!    capability fields so upgrades (e.g. anthropic's 1M beta) show up.
//!    cost is the exception: anthropic and openai `/v1/models` don't
//!    return pricing, so a zero discovered cost keeps the seed's pricing.
//! 3. **seed / stale** - the shipped static catalogue, plus
//!    previously-discovered models absent from the latest fetch.
//!
//! provenance:
//! - discovered-only models surface with [`ModelSource::Discovered`]
//! - static-only models surface as [`ModelSource::Static`] - they're not
//!   stale (they may be from a non-discoverable provider like groq)
//! - models that were discovered before but absent from the latest fetch
//!   surface with [`ModelSource::DiscoveredStale`]; they remain selectable
//!   but the picker can highlight them and offer a delete action
//! - overridden models surface with [`ModelSource::Override`]

use std::collections::BTreeMap;

use super::cache::DiscoveryCache;
use crate::types::{Model, ModelCost};

/// where a [`MergedModel`] came from in the merged catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// only in the static catalogue (no discovery saw it - this is the
    /// default for providers we don't run discovery against, like groq)
    Static,
    /// returned by the most recent successful discovery fetch
    Discovered,
    /// in the cache but absent from the most recent fetch for its
    /// provider. likely deprecated/removed upstream.
    DiscoveredStale,
    /// a manual or curated override that wins over discovery. covers user
    /// `models.json` entries and the shipped corrections for routes whose
    /// upstream metadata is wrong (e.g. gpt-5.5 codex reporting a 272K
    /// input budget as its whole window when the marketed window is 400K)
    Override,
}

/// a model entry in the merged catalogue, paired with provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedModel {
    pub model: Model,
    pub source: ModelSource,
    /// raw upstream JSON for discovered entries; `None` for static-only
    /// models. consumers reach for this through per-provider typed
    /// accessors (e.g. [`crate::discovery::codex::extras`])
    pub raw: Option<serde_json::Value>,
}

/// merge a static catalogue with a discovery cache and a set of overrides.
///
/// see module docs for the three-tier precedence.
#[must_use]
pub fn merge(
    static_models: Vec<Model>,
    cache: &DiscoveryCache,
    overrides: Vec<Model>,
) -> Vec<MergedModel> {
    type Key = (String, String);
    let mut merged: BTreeMap<Key, MergedModel> = BTreeMap::new();

    // seed with the static catalogue
    for model in static_models {
        let key = (model.provider.to_string(), model.id.as_str().to_string());
        merged.insert(
            key,
            MergedModel {
                model,
                source: ModelSource::Static,
                raw: None,
            },
        );
    }

    // overlay discovered entries
    for (provider_key, entry) in cache.entries() {
        let key = (
            provider_key.to_string(),
            entry.model.id.as_str().to_string(),
        );
        let stale = cache.is_stale(provider_key, entry.model.id.as_str());

        let mut model = entry.model.clone();
        if let Some(existing) = merged.get(&key) {
            model.cost = pick_cost(&existing.model.cost, &model.cost);
        }

        merged.insert(
            key,
            MergedModel {
                model,
                source: if stale {
                    ModelSource::DiscoveredStale
                } else {
                    ModelSource::Discovered
                },
                raw: entry.raw.clone(),
            },
        );
    }

    // overlay overrides on top of discovery. the override is
    // authoritative (full win, including cost), but inherits the
    // discovered `raw` payload so provider-specific extras (e.g. codex
    // reasoning levels) still surface in the picker
    for model in overrides {
        let key = (model.provider.to_string(), model.id.as_str().to_string());
        let raw = merged.get(&key).and_then(|existing| existing.raw.clone());
        merged.insert(
            key,
            MergedModel {
                model,
                source: ModelSource::Override,
                raw,
            },
        );
    }

    merged.into_values().collect()
}

/// when discovery returns zero cost (anthropic, openai), prefer the static catalogue's cost.
/// when discovery has real numbers (openrouter), discovery wins.
fn pick_cost(static_cost: &ModelCost, discovered_cost: &ModelCost) -> ModelCost {
    let pick = |static_v: f64, discovered_v: f64| {
        if discovered_v == 0.0 && static_v != 0.0 {
            static_v
        } else {
            discovered_v
        }
    };
    ModelCost {
        input: pick(static_cost.input, discovered_cost.input),
        output: pick(static_cost.output, discovered_cost.output),
        cache_read: pick(static_cost.cache_read, discovered_cost.cache_read),
        cache_write: pick(static_cost.cache_write, discovered_cost.cache_write),
    }
}

/// convenience: merge the *current* user catalogue (built-ins + user
/// `models.json`) with the on-disk discovery cache.
///
/// fails open: if the cache file is unreadable, we just return the
/// static catalogue wrapped in [`MergedModel`].
#[must_use]
pub fn merged_catalogue() -> Vec<MergedModel> {
    let seed = crate::models::all_models();
    let overrides = crate::models::model_overrides();
    let cache = DiscoveryCache::load(&super::cache::cache_path()).unwrap_or_default();
    merge(seed, &cache, overrides)
}

/// helper: extract just the [`Model`]s from a merge result.
#[must_use]
pub fn merged_models() -> Vec<Model> {
    merged_catalogue().into_iter().map(|m| m.model).collect()
}

/// look up a single merged entry by id.
#[must_use]
pub fn find_merged_model_by_id(id: &str) -> Option<MergedModel> {
    merged_catalogue()
        .into_iter()
        .find(|m| m.model.id.as_str() == id)
}

/// convenience wrapper around [`find_merged_model_by_id`] that returns
/// just the [`Model`] without provenance metadata.
///
/// use this when you only need the resolved model to drive a request
/// (most runtime paths). callers that surface "discovered vs static" or
/// stale entries should use [`find_merged_model_by_id`] directly.
#[must_use]
pub fn resolve_model_by_id(id: &str) -> Option<Model> {
    find_merged_model_by_id(id).map(|m| m.model)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::discovery::DiscoveryReport;
    use crate::types::{Api, InputModality, ModelCost, Provider, TokenCount};

    fn anthropic_model(id: &str, name: &str, cost_input: f64) -> Model {
        Model {
            id: id.into(),
            name: name.into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: cost_input,
                output: cost_input * 5.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }

    #[test]
    fn empty_cache_yields_static_only_entries() {
        let static_models = vec![anthropic_model("a", "A", 3.0)];
        let cache = DiscoveryCache::default();
        let merged = merge(static_models, &cache, vec![]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, ModelSource::Static);
    }

    #[test]
    fn discovered_overrides_static_capability_fields() {
        let mut static_a = anthropic_model("a", "Old Name", 3.0);
        static_a.context_window = TokenCount::new(100_000);

        let mut discovered_a = anthropic_model("a", "New Name", 0.0);
        discovered_a.context_window = TokenCount::new(1_000_000);

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
            models: vec![discovered_a.into()],
        });

        let merged = merge(vec![static_a], &cache, vec![]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, ModelSource::Discovered);
        assert_eq!(merged[0].model.name, "New Name");
        assert_eq!(merged[0].model.context_window, TokenCount::new(1_000_000));
    }

    #[test]
    fn anthropic_zero_cost_falls_back_to_static_cost() {
        // anthropic /v1/models returns no pricing so cost is zero.
        // merge must keep the static catalogue's pricing.
        let static_a = anthropic_model("a", "A", 3.0);
        let discovered_a = anthropic_model("a", "A", 0.0);

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![discovered_a.into()],
        });

        let merged = merge(vec![static_a], &cache, vec![]);
        assert_eq!(merged[0].model.cost.input, 3.0);
        assert_eq!(merged[0].model.cost.output, 15.0);
    }

    #[test]
    fn openrouter_discovered_pricing_wins_over_static() {
        // openrouter discovery has real pricing - it should override.
        let static_a = Model {
            cost: ModelCost {
                input: 99.0,
                output: 99.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            ..anthropic_model("a", "A", 99.0)
        };
        let discovered_a = Model {
            cost: ModelCost {
                input: 1.0,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            ..anthropic_model("a", "A", 0.0)
        };

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![discovered_a.into()],
        });

        let merged = merge(vec![static_a], &cache, vec![]);
        assert_eq!(merged[0].model.cost.input, 1.0);
        assert_eq!(merged[0].model.cost.output, 5.0);
        assert_eq!(merged[0].model.cost.cache_read, 0.1);
    }

    #[test]
    fn discovered_only_models_appear_with_discovered_source() {
        let cache = {
            let mut c = DiscoveryCache::default();
            c.apply_report(DiscoveryReport {
                provider: Provider::Anthropic,
                fetched_at: SystemTime::UNIX_EPOCH,
                models: vec![anthropic_model("brand-new", "Brand New", 0.0).into()],
            });
            c
        };

        let merged = merge(vec![], &cache, vec![]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, ModelSource::Discovered);
        assert_eq!(merged[0].model.id.as_str(), "brand-new");
    }

    #[test]
    fn stale_models_marked_as_discovered_stale() {
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(200);

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t1,
            models: vec![
                anthropic_model("a", "A", 0.0).into(),
                anthropic_model("b", "B", 0.0).into(),
            ],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t2,
            models: vec![anthropic_model("a", "A", 0.0).into()],
        });

        let merged = merge(vec![], &cache, vec![]);
        let by_id: BTreeMap<_, _> = merged
            .iter()
            .map(|m| (m.model.id.as_str().to_string(), m))
            .collect();
        assert_eq!(by_id["a"].source, ModelSource::Discovered);
        assert_eq!(by_id["b"].source, ModelSource::DiscoveredStale);
    }

    #[test]
    fn merge_preserves_unrelated_static_providers() {
        // groq (no discovery) and anthropic (discovered) should both surface
        let groq = Model {
            provider: Provider::Custom("groq".into()),
            ..anthropic_model("groq-llama", "Llama (Groq)", 0.5)
        };
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![anthropic_model("a", "A", 0.0).into()],
        });

        let merged = merge(vec![groq], &cache, vec![]);
        assert_eq!(merged.len(), 2);
        let by_id: BTreeMap<_, _> = merged
            .iter()
            .map(|m| (m.model.id.as_str().to_string(), m))
            .collect();
        assert_eq!(by_id["groq-llama"].source, ModelSource::Static);
        assert_eq!(by_id["a"].source, ModelSource::Discovered);
    }

    #[test]
    fn override_wins_over_discovery() {
        // a curated/manual override must beat a live discovered value.
        // codex reports gpt-5.5's 272K input budget as its whole window;
        // the override re-asserts the 400K marketed window.
        let mut discovered = anthropic_model("a", "Discovered", 0.0);
        discovered.context_window = TokenCount::new(272_000);
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![discovered.into()],
        });

        let mut override_a = anthropic_model("a", "Override", 0.0);
        override_a.context_window = TokenCount::new(400_000);

        let merged = merge(vec![], &cache, vec![override_a]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, ModelSource::Override);
        assert_eq!(merged[0].model.name, "Override");
        assert_eq!(merged[0].model.context_window, TokenCount::new(400_000));
    }

    #[test]
    fn non_overridden_models_still_take_discovery_over_seed() {
        // anthropic's 1M beta is a discovered upgrade over the 200K seed.
        // models absent from the override set must keep taking discovery's
        // value, so we don't regress that behaviour by adding the tier.
        let mut seed = anthropic_model("a", "Seed", 3.0);
        seed.context_window = TokenCount::new(200_000);
        let mut discovered = anthropic_model("a", "Discovered", 0.0);
        discovered.context_window = TokenCount::new(1_000_000);
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![discovered.into()],
        });

        let merged = merge(vec![seed], &cache, vec![]);
        assert_eq!(merged[0].source, ModelSource::Discovered);
        assert_eq!(merged[0].model.context_window, TokenCount::new(1_000_000));
    }

    #[test]
    fn gpt55_codex_resolves_to_marketed_window_over_discovery() {
        // end to end against the real catalogue: even when codex discovery
        // reports gpt-5.5 at its 272K input budget, the shipped override
        // surfaces the 400K marketed window for display while keeping the
        // 272K effective budget for compaction.
        let mut discovered = Model {
            provider: Provider::Custom("openai-codex".into()),
            ..anthropic_model("gpt-5.5", "GPT-5.5", 0.0)
        };
        discovered.context_window = TokenCount::new(272_000);
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Custom("openai-codex".into()),
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![discovered.into()],
        });

        let merged = merge(
            crate::models::all_models(),
            &cache,
            crate::models::model_overrides(),
        );
        let gpt55 = merged
            .iter()
            .find(|m| m.model.id.as_str() == "gpt-5.5")
            .expect("gpt-5.5 present in catalogue");
        assert_eq!(gpt55.source, ModelSource::Override);
        assert_eq!(gpt55.model.context_window, TokenCount::new(400_000));
        assert_eq!(
            gpt55.model.effective_context_window,
            Some(TokenCount::new(272_000))
        );
    }
}
