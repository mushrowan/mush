//! on-disk cache for discovery results.
//!
//! the cache is keyed by provider name and stores, for each model:
//! - the [`Model`] as discovered (rich fields from the upstream `/v1/models`)
//! - `last_seen_at` for stale detection
//!
//! per provider we track `fetched_at` - the timestamp of the most recent
//! successful fetch. a cached model is "stale" if its `last_seen_at` is
//! older than the provider's `fetched_at`, meaning the upstream stopped
//! returning it (deprecation, removal).
//!
//! cache file lives at `${data_dir}/discovered-models.json`. data_dir
//! resolution mirrors [`mush_session::data_dir`] so we share the
//! `~/.local/share/mush/` (or `$XDG_DATA_HOME/mush/`) state directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::DiscoveryReport;
use crate::types::Model;

/// where the cache file lives within the mush data dir
const CACHE_FILE: &str = "discovered-models.json";

/// in-memory representation of the on-disk cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryCache {
    /// per-provider sub-cache, keyed by [`crate::types::Provider`] display string.
    /// using a string key keeps the file tolerant to provider variants we
    /// haven't enumerated yet (custom providers).
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCache>,
}

/// snapshot of a single provider's discovery state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderCache {
    /// timestamp of the most recent successful fetch for this provider
    pub fetched_at: SystemTime,
    /// every model we've ever seen from this provider (even if a later
    /// fetch dropped it - those become "stale" entries)
    pub models: Vec<DiscoveredEntry>,
}

/// one cached model entry plus discovery metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveredEntry {
    /// last time this exact model id was returned by a successful fetch
    pub last_seen_at: SystemTime,
    pub model: Model,
    /// verbatim upstream entry; see [`super::DiscoveredModel::raw`]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl DiscoveryCache {
    /// load the cache from disk, or return an empty cache when the file
    /// is missing/unreadable. errors during deserialisation are surfaced.
    pub fn load(path: &Path) -> Result<Self, std::io::Error> {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// persist the cache to disk, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// merge a fresh discovery report into the cache.
    ///
    /// - models present in the report bump their `last_seen_at` to the
    ///   report's `fetched_at` and have their `model` field overwritten
    /// - models in the cache but absent from the report are kept (they
    ///   become stale because their `last_seen_at` lags the new
    ///   `fetched_at`).
    /// - new models are appended.
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

    /// iterator over (provider key, model entry) for every discovered model.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &DiscoveredEntry)> + '_ {
        self.providers
            .iter()
            .flat_map(|(p, pc)| pc.models.iter().map(move |e| (p.as_str(), e)))
    }

    /// whether a given entry is stale - the upstream stopped returning it
    /// in the most recent fetch for that provider.
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

    /// remove a single model from the cache. returns true if it was found.
    pub fn remove_model(&mut self, provider_key: &str, model_id: &str) -> bool {
        let Some(provider) = self.providers.get_mut(provider_key) else {
            return false;
        };
        let len_before = provider.models.len();
        provider.models.retain(|e| e.model.id.as_str() != model_id);
        provider.models.len() != len_before
    }

    /// remove every stale entry from every provider. returns how many
    /// entries were dropped.
    pub fn remove_all_stale(&mut self) -> usize {
        let mut removed = 0;
        for (_, provider) in self.providers.iter_mut() {
            let cutoff = provider.fetched_at;
            let before = provider.models.len();
            provider.models.retain(|e| e.last_seen_at >= cutoff);
            removed += before - provider.models.len();
        }
        removed
    }
}

/// path to the cache file inside the mush data dir.
#[must_use]
pub fn cache_path() -> PathBuf {
    data_dir().join(CACHE_FILE)
}

/// resolve the mush data directory, mirroring `mush_session::data_dir`.
///
/// duplicated here because mush-ai cannot depend on mush-session
/// (mush-session already depends on mush-ai).
fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_DATA_DIR") {
        PathBuf::from(dir)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/mush")
    } else {
        PathBuf::from(".mush")
    }
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
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: false,
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
        // second fetch only returns A - B is now stale
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
        // second fetch only returns A; B and C are stale
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
        // unsafe set_var is acceptable in tests, racy across-test but serial-ish in practice
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
