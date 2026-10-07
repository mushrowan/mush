use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::DiscoveryReport;
use crate::types::Model;

const CACHE_FILE: &str = "discovered-models.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryCache {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCache>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderCache {
    pub fetched_at: SystemTime,
    pub models: Vec<DiscoveredEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveredEntry {
    pub last_seen_at: SystemTime,
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl DiscoveryCache {
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::private_io::write_atomic(path, json)
    }

    pub fn apply_report(&mut self, report: DiscoveryReport) {
        let provider_key = report.provider.to_string();
        let provider = self
            .providers
            .entry(provider_key)
            .or_insert_with(|| ProviderCache {
                fetched_at: report.fetched_at,
                models: Vec::new(),
            });
        provider.fetched_at = report.fetched_at;

        for fresh in report.models {
            if let Some(existing) = provider
                .models
                .iter_mut()
                .find(|e| e.model.id == fresh.model.id)
            {
                existing.last_seen_at = report.fetched_at;
                existing.model = fresh.model;
                existing.raw = fresh.raw;
            } else {
                provider.models.push(DiscoveredEntry {
                    last_seen_at: report.fetched_at,
                    model: fresh.model,
                    raw: fresh.raw,
                });
            }
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &DiscoveredEntry)> + '_ {
        self.providers
            .iter()
            .flat_map(|(p, pc)| pc.models.iter().map(move |e| (p.as_str(), e)))
    }

    #[must_use]
    pub fn is_stale(&self, provider_key: &str, model_id: &str) -> bool {
        let Some(provider) = self.providers.get(provider_key) else {
            return false;
        };
        provider
            .models
            .iter()
            .find(|e| e.model.id.as_str() == model_id)
            .map(|e| e.last_seen_at < provider.fetched_at)
            .unwrap_or(false)
    }

    pub fn remove_model(&mut self, provider_key: &str, model_id: &str) -> bool {
        let Some(provider) = self.providers.get_mut(provider_key) else {
            return false;
        };
        let len_before = provider.models.len();
        provider.models.retain(|e| e.model.id.as_str() != model_id);
        provider.models.len() != len_before
    }

    pub fn remove_all_stale(&mut self) -> usize {
        let mut removed = 0;
        for provider in self.providers.values_mut() {
            let cutoff = provider.fetched_at;
            let before = provider.models.len();
            provider.models.retain(|e| e.last_seen_at >= cutoff);
            removed += before - provider.models.len();
        }
        removed
    }
}

#[must_use]
pub fn cache_path() -> PathBuf {
    crate::dirs::data_dir().join(CACHE_FILE)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::types::{Api, InputModality, Model, ModelCost, Provider, TokenCount};
    fn make_model(id: &str, name: &str) -> super::super::DiscoveredModel {
        Model {
            id: id.into(),
            name: name.into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
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
        .into()
    }

    #[test]
    fn load_missing_file_returns_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let cache = DiscoveryCache::load(&path).unwrap();
        assert!(cache.providers.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/discovered-models.json");

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            models: vec![make_model("claude-test", "Claude Test")],
        });

        cache.save(&path).unwrap();
        let loaded = DiscoveryCache::load(&path).unwrap();
        assert_eq!(loaded, cache);
    }

    #[test]
    fn apply_report_inserts_new_models() {
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
            models: vec![make_model("a", "A"), make_model("b", "B")],
        });
        let pc = cache.providers.get("anthropic").unwrap();
        assert_eq!(pc.models.len(), 2);
    }

    #[test]
    fn apply_report_updates_last_seen_for_existing_models() {
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(200);

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t1,
            models: vec![make_model("a", "A v1")],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t2,
            models: vec![make_model("a", "A v2")],
        });

        let pc = cache.providers.get("anthropic").unwrap();
        assert_eq!(pc.models.len(), 1);
        assert_eq!(pc.models[0].last_seen_at, t2);
        assert_eq!(pc.models[0].model.name, "A v2");
        assert_eq!(pc.fetched_at, t2);
    }

    #[test]
    fn models_dropped_from_fetch_become_stale() {
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(200);

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t1,
            models: vec![make_model("a", "A"), make_model("b", "B")],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t2,
            models: vec![make_model("a", "A")],
        });

        assert!(!cache.is_stale("anthropic", "a"));
        assert!(cache.is_stale("anthropic", "b"));

        let pc = cache.providers.get("anthropic").unwrap();
        assert_eq!(pc.models.len(), 2, "stale entry must remain in cache");
    }

    #[test]
    fn is_stale_unknown_returns_false() {
        let cache = DiscoveryCache::default();
        assert!(!cache.is_stale("anthropic", "missing"));
    }

    #[test]
    fn remove_model_drops_entry_and_reports_found() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t,
            models: vec![make_model("a", "A"), make_model("b", "B")],
        });

        assert!(cache.remove_model("anthropic", "a"));
        assert!(!cache.remove_model("anthropic", "a"));
        let pc = cache.providers.get("anthropic").unwrap();
        assert_eq!(pc.models.len(), 1);
        assert_eq!(pc.models[0].model.id.as_str(), "b");
    }

    #[test]
    fn remove_all_stale_keeps_fresh_drops_stale_returns_count() {
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(200);

        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t1,
            models: vec![
                make_model("a", "A"),
                make_model("b", "B"),
                make_model("c", "C"),
            ],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t2,
            models: vec![make_model("a", "A")],
        });

        let removed = cache.remove_all_stale();
        assert_eq!(removed, 2);

        let pc = cache.providers.get("anthropic").unwrap();
        assert_eq!(pc.models.len(), 1);
        assert_eq!(pc.models[0].model.id.as_str(), "a");
    }

    #[test]
    fn remove_all_stale_returns_zero_when_nothing_stale() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t,
            models: vec![make_model("a", "A")],
        });
        assert_eq!(cache.remove_all_stale(), 0);
    }

    #[test]
    fn remove_all_stale_works_across_providers() {
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t1,
            models: vec![make_model("a-old", "A")],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t2,
            models: vec![],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::OpenRouter,
            fetched_at: t1,
            models: vec![make_model("b-old", "B")],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::OpenRouter,
            fetched_at: t2,
            models: vec![],
        });
        assert_eq!(cache.remove_all_stale(), 2);
    }

    #[test]
    fn entries_iterates_across_providers() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut cache = DiscoveryCache::default();
        cache.apply_report(DiscoveryReport {
            provider: Provider::Anthropic,
            fetched_at: t,
            models: vec![make_model("a", "A")],
        });
        cache.apply_report(DiscoveryReport {
            provider: Provider::OpenRouter,
            fetched_at: t,
            models: vec![make_model("b", "B")],
        });
        let collected: Vec<_> = cache
            .entries()
            .map(|(p, e)| (p, e.model.id.as_str()))
            .collect();
        assert_eq!(collected.len(), 2);
        assert!(collected.contains(&("anthropic", "a")));
        assert!(collected.contains(&("openrouter", "b")));
    }

    #[test]
    fn cache_path_uses_data_dir_env() {
        let dir = tempfile::tempdir().unwrap();
        let saved = std::env::var("MUSH_DATA_DIR").ok();
        unsafe {
            std::env::set_var("MUSH_DATA_DIR", dir.path());
        }
        let path = cache_path();
        assert_eq!(path, dir.path().join(CACHE_FILE));
        unsafe {
            match saved {
                Some(v) => std::env::set_var("MUSH_DATA_DIR", v),
                None => std::env::remove_var("MUSH_DATA_DIR"),
            }
        }
    }
}
