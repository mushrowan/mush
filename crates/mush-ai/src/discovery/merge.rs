use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

use super::cache::DiscoveryCache;
use crate::models::FileStamp;
use crate::types::{Model, ModelCost};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Static,
    Discovered,
    DiscoveredStale,
    Override,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergedModel {
    pub model: Model,
    pub source: ModelSource,
    pub raw: Option<serde_json::Value>,
}

#[must_use]
pub fn merge(
    static_models: Vec<Model>,
    cache: &DiscoveryCache,
    overrides: Vec<Model>,
) -> Vec<MergedModel> {
    type Key = (String, String);
    let mut merged: BTreeMap<Key, MergedModel> = BTreeMap::new();

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

struct Catalogue {
    stamps: (FileStamp, FileStamp),
    models: Arc<[MergedModel]>,
}

static CATALOGUE: LazyLock<Mutex<Option<Catalogue>>> = LazyLock::new(|| Mutex::new(None));

fn catalogue() -> Arc<[MergedModel]> {
    let stamps = (
        FileStamp::of(super::cache::cache_path()),
        crate::models::user_models_stamp(),
    );
    if let Ok(cached) = CATALOGUE.lock()
        && let Some(cached) = cached.as_ref()
        && cached.stamps == stamps
    {
        return Arc::clone(&cached.models);
    }
    let seed = crate::models::all_models();
    let overrides = crate::models::model_overrides();
    let cache = DiscoveryCache::load(&stamps.0.path).unwrap_or_default();
    let models: Arc<[MergedModel]> = merge(seed, &cache, overrides).into();
    if let Ok(mut cached) = CATALOGUE.lock() {
        *cached = Some(Catalogue {
            stamps,
            models: Arc::clone(&models),
        });
    }
    models
}

#[must_use]
pub fn merged_catalogue() -> Vec<MergedModel> {
    catalogue().to_vec()
}

#[must_use]
pub fn shared_catalogue() -> Arc<[MergedModel]> {
    catalogue()
}

#[must_use]
pub fn merged_models() -> Vec<Model> {
    catalogue().iter().map(|m| m.model.clone()).collect()
}

#[must_use]
pub fn find_merged_model_by_id(id: &str) -> Option<MergedModel> {
    catalogue()
        .iter()
        .find(|m| m.model.id.as_str() == id)
        .cloned()
}

#[must_use]
pub fn resolve_model_by_id(id: &str) -> Option<Model> {
    catalogue()
        .iter()
        .find(|m| m.model.id.as_str() == id)
        .map(|m| m.model.clone())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::discovery::DiscoveryReport;
    use crate::types::{Api, ModelCost, Provider, TokenCount};

    fn anthropic_model(id: &str, name: &str, cost_input: f64) -> Model {
        Model::builder()
            .id(id.into())
            .name(name.into())
            .api(Api::AnthropicMessages)
            .provider(Provider::Anthropic)
            .base_url("https://api.anthropic.com".into())
            .context_window(TokenCount::new(200_000))
            .max_output_tokens(TokenCount::new(64_000))
            .cost(ModelCost {
                input: cost_input,
                output: cost_input * 5.0,
                cache_read: 0.0,
                cache_write: 0.0,
            })
            .build()
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
        let custom = Model {
            provider: Provider::Custom("custom".into()),
            ..anthropic_model("custom-model", "Custom", 0.5)
        };
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH,
            models: vec![anthropic_model("a", "A", 0.0).into()],
        });

        let merged = merge(vec![custom], &cache, vec![]);
        assert_eq!(merged.len(), 2);
        let by_id: BTreeMap<_, _> = merged
            .iter()
            .map(|m| (m.model.id.as_str().to_string(), m))
            .collect();
        assert_eq!(by_id["custom-model"].source, ModelSource::Static);
        assert_eq!(by_id["a"].source, ModelSource::Discovered);
    }

    #[test]
    fn override_wins_over_discovery() {
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
    fn resolving_a_model_follows_a_rewritten_discovery_cache() {
        let dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", dir.path());
            std::env::set_var("MUSH_CONFIG_DIR", dir.path());
        }
        assert!(resolve_model_by_id("found-later").is_none());

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
            models: vec![anthropic_model("found-later", "Found Later", 1.0).into()],
        });
        cache.save(&super::super::cache::cache_path()).unwrap();

        assert_eq!(
            resolve_model_by_id("found-later").map(|model| model.name),
            Some("Found Later".to_string())
        );
    }
}
