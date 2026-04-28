//! per-directory + per-model thinking-level preferences.
//!
//! the persisted shape is a nested map keyed by canonical cwd, then by
//! model id. lookups are scoped to the directory the user is currently
//! working in so the same model can carry different preferences across
//! projects without polluting the project tree (everything lives under
//! `data_dir()`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mush_ai::types::ThinkingLevel;
use serde::{Deserialize, Serialize};

/// callback to persist thinking prefs when they change. takes the whole
/// nested map so the writer can serialise atomically without juggling
/// per-dir slices.
pub type ThinkingPrefsSaver = std::sync::Arc<dyn Fn(&ThinkingPrefs) + Send + Sync>;

/// nested thinking-level preferences keyed by canonical directory then
/// by model id. designed to be loaded from disk once at startup and
/// mutated via [`Self::set`] as the user cycles levels at runtime.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThinkingPrefs {
    by_dir: HashMap<PathBuf, HashMap<String, ThinkingLevel>>,
}

impl ThinkingPrefs {
    /// look up the saved level for `model_id` under `dir`. returns
    /// `None` when no entry exists. the level is normalised to the
    /// visible-controls form (legacy `Minimal` collapses to `Low`).
    #[must_use]
    pub fn get(&self, dir: &Path, model_id: &str) -> Option<ThinkingLevel> {
        self.by_dir
            .get(dir)?
            .get(model_id)
            .copied()
            .map(ThinkingLevel::normalize_visible)
    }

    /// record a per-dir+model preference. the value is normalised before
    /// storage so we never persist `Minimal`.
    pub fn set(&mut self, dir: PathBuf, model_id: String, level: ThinkingLevel) {
        self.by_dir
            .entry(dir)
            .or_default()
            .insert(model_id, level.normalize_visible());
    }

    /// drop entries pointing at directories that no longer exist on
    /// disk. called on load so the file doesn't grow forever as users
    /// retire old projects.
    pub fn prune_missing_dirs(&mut self) {
        self.by_dir.retain(|p, _| p.exists());
    }

    /// number of directories with at least one stored preference. used
    /// by tests; production code should reach for [`Self::get`] /
    /// [`Self::set`] instead.
    #[must_use]
    pub fn directory_count(&self) -> usize {
        self.by_dir.len()
    }
}

/// canonicalise the supplied directory if possible, falling back to the
/// raw path on filesystems that refuse the lookup (network mounts, race
/// conditions). canonical paths collapse symlinks so `~/dev/mush` and
/// the underlying `/home/<user>/dev/mush` map to the same key across
/// invocations.
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
        // the same model can carry different levels in different cwds; the
        // get path must scope on directory, not model id alone
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
        // legacy ThinkingLevel::Minimal collapses to Low in visible
        // controls, so we never persist it (matches the global flat-file
        // behaviour from before the per-dir migration)
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::Minimal);
        assert_eq!(
            prefs.get(Path::new("/a"), "gpt-5"),
            Some(ThinkingLevel::Low)
        );
    }

    #[test]
    fn prune_missing_dirs_keeps_existent_drops_others() {
        // entries pointing at deleted projects shouldn't keep the file
        // bloating forever; we drop them at load time
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
    fn serialises_as_nested_map() {
        // disk shape mirrors the in-memory nesting; user can hand-edit
        // the file without learning a new schema
        let mut prefs = ThinkingPrefs::default();
        prefs.set(
            PathBuf::from("/home/rowan/dev/mush"),
            "gpt-5".into(),
            ThinkingLevel::High,
        );
        let json = serde_json::to_value(&prefs).unwrap();
        let expected = serde_json::json!({
            "/home/rowan/dev/mush": { "gpt-5": "high" }
        });
        assert_eq!(json, expected);
    }

    #[test]
    fn deserialises_old_flat_shape_as_error() {
        // the pre-migration flat shape `{model_id: level}` is a different
        // schema; deserialising must fail so callers can fall back to a
        // fresh empty prefs map (data drop is intentional, see commit msg)
        let old = r#"{"gpt-5":"high","claude-opus-4-7":"medium"}"#;
        assert!(serde_json::from_str::<ThinkingPrefs>(old).is_err());
    }
}
