use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mush_ai::types::ThinkingLevel;
use serde::{Deserialize, Serialize};

pub type ThinkingPrefsSaver = std::sync::Arc<dyn Fn(&ThinkingPrefs) + Send + Sync>;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingPrefs {
    by_dir: HashMap<PathBuf, HashMap<String, ThinkingLevel>>,
    #[serde(default)]
    last_for_model: HashMap<String, ThinkingLevel>,
}

impl ThinkingPrefs {
    #[must_use]
    pub fn get(&self, dir: &Path, model_id: &str) -> Option<ThinkingLevel> {
        self.by_dir
            .get(dir)?
            .get(model_id)
            .map(ThinkingLevel::normalize_visible)
    }

    #[must_use]
    pub fn last_for_model(&self, model_id: &str) -> Option<ThinkingLevel> {
        self.last_for_model
            .get(model_id)
            .map(ThinkingLevel::normalize_visible)
    }

    pub fn set(&mut self, dir: PathBuf, model_id: String, level: ThinkingLevel) {
        let normalised = level.normalize_visible();
        self.by_dir
            .entry(dir)
            .or_default()
            .insert(model_id.clone(), normalised.clone());
        self.last_for_model.insert(model_id, normalised);
    }

    pub fn prune_missing_dirs(&mut self) {
        self.by_dir.retain(|p, _| p.exists());
    }

    #[must_use]
    pub fn directory_count(&self) -> usize {
        self.by_dir.len()
    }
}

#[must_use]
pub fn canonical_dir(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_none_for_unknown_dir_or_model() {
        let prefs = ThinkingPrefs::default();
        assert!(prefs.get(Path::new("/nope"), "gpt-5").is_none());
    }

    #[test]
    fn set_then_get_round_trips_per_directory() {
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::High);
        prefs.set(PathBuf::from("/b"), "gpt-5".into(), ThinkingLevel::Low);
        assert_eq!(
            prefs.get(Path::new("/a"), "gpt-5"),
            Some(ThinkingLevel::High)
        );
        assert_eq!(
            prefs.get(Path::new("/b"), "gpt-5"),
            Some(ThinkingLevel::Low)
        );
    }

    #[test]
    fn set_normalises_minimal_to_low_before_storing() {
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::Minimal);
        assert_eq!(
            prefs.get(Path::new("/a"), "gpt-5"),
            Some(ThinkingLevel::Low)
        );
    }

    #[test]
    fn prune_missing_dirs_keeps_existent_drops_others() {
        let tmp = tempfile::tempdir().unwrap();
        let mut prefs = ThinkingPrefs::default();
        prefs.set(
            tmp.path().to_path_buf(),
            "gpt-5".into(),
            ThinkingLevel::High,
        );
        prefs.set(
            PathBuf::from("/this/does/not/exist/anywhere"),
            "gpt-5".into(),
            ThinkingLevel::Medium,
        );
        prefs.prune_missing_dirs();
        assert_eq!(prefs.directory_count(), 1);
        assert!(prefs.get(tmp.path(), "gpt-5").is_some());
    }

    #[test]
    fn serialises_with_per_dir_and_per_model_sections() {
        let mut prefs = ThinkingPrefs::default();
        prefs.set(
            PathBuf::from("/home/rowan/dev/mush"),
            "gpt-5".into(),
            ThinkingLevel::High,
        );
        let json = serde_json::to_value(&prefs).unwrap();
        let expected = serde_json::json!({
            "by_dir": {
                "/home/rowan/dev/mush": { "gpt-5": "high" }
            },
            "last_for_model": { "gpt-5": "high" }
        });
        assert_eq!(json, expected);
    }

    #[test]
    fn deserialises_legacy_transparent_shape_as_error() {
        let old = r#"{"/home/rowan/dev/mush":{"gpt-5":"high"}}"#;
        assert!(serde_json::from_str::<ThinkingPrefs>(old).is_err());
    }

    #[test]
    fn deserialises_old_flat_shape_as_error() {
        let old = r#"{"gpt-5":"high","claude-opus-4-7":"medium"}"#;
        assert!(serde_json::from_str::<ThinkingPrefs>(old).is_err());
    }

    #[test]
    fn deserialises_without_per_model_section_for_forward_compat() {
        let json = r#"{"by_dir":{"/a":{"gpt-5":"high"}}}"#;
        let prefs: ThinkingPrefs = serde_json::from_str(json).unwrap();
        assert_eq!(
            prefs.get(Path::new("/a"), "gpt-5"),
            Some(ThinkingLevel::High)
        );
        assert!(prefs.last_for_model("gpt-5").is_none());
    }

    #[test]
    fn last_for_model_returns_most_recent_set_anywhere() {
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::High);
        prefs.set(PathBuf::from("/b"), "gpt-5".into(), ThinkingLevel::Low);
        assert_eq!(
            prefs.last_for_model("gpt-5"),
            Some(ThinkingLevel::Low),
            "last_for_model should return the most recent level"
        );
        assert!(prefs.last_for_model("missing-model").is_none());
    }

    #[test]
    fn last_for_model_normalises_minimal_to_low() {
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::Minimal);
        assert_eq!(prefs.last_for_model("gpt-5"), Some(ThinkingLevel::Low));
    }

    #[test]
    fn set_in_new_dir_updates_per_model_fallback_for_old_dir() {
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::High);
        prefs.set(PathBuf::from("/b"), "gpt-5".into(), ThinkingLevel::Low);
        assert_eq!(
            prefs.get(Path::new("/a"), "gpt-5"),
            Some(ThinkingLevel::High)
        );
        assert_eq!(prefs.last_for_model("gpt-5"), Some(ThinkingLevel::Low));
    }
}
