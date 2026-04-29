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
/// by model id, plus a per-model fallback that records the most recent
/// level chosen for each model regardless of dir. designed to be loaded
/// from disk once at startup and mutated via [`Self::set`] as the user
/// cycles levels at runtime.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingPrefs {
    /// per-(dir, model) explicit picks. takes precedence over the
    /// per-model fallback when both exist for the active dir
    by_dir: HashMap<PathBuf, HashMap<String, ThinkingLevel>>,
    /// per-model fallback. tracks the most recent level set anywhere
    /// (any dir) so a model the user has touched in another project
    /// doesn't snap back to upstream defaults when opened elsewhere.
    /// always synchronised with the latest [`Self::set`] call
    #[serde(default)]
    last_for_model: HashMap<String, ThinkingLevel>,
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

    /// per-model fallback: the most recent level the user has chosen
    /// for `model_id` in *any* directory. used by `resolve_thinking`
    /// when the active `(cwd, model)` has no explicit pick yet so the
    /// model carries forward the user's last preference instead of
    /// snapping back to upstream defaults
    #[must_use]
    pub fn last_for_model(&self, model_id: &str) -> Option<ThinkingLevel> {
        self.last_for_model
            .get(model_id)
            .copied()
            .map(ThinkingLevel::normalize_visible)
    }

    /// record a per-dir+model preference. the value is normalised before
    /// storage so we never persist `Minimal`. also updates the per-model
    /// fallback so callers in other dirs inherit the latest pick
    pub fn set(&mut self, dir: PathBuf, model_id: String, level: ThinkingLevel) {
        let normalised = level.normalize_visible();
        self.by_dir
            .entry(dir)
            .or_default()
            .insert(model_id.clone(), normalised);
        self.last_for_model.insert(model_id, normalised);
    }

    /// drop entries pointing at directories that no longer exist on
    /// disk. called on load so the file doesn't grow forever as users
    /// retire old projects. the per-model fallback is preserved because
    /// it isn't tied to any one directory
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
    fn serialises_with_per_dir_and_per_model_sections() {
        // disk shape carries both the per-(dir, model) explicit picks
        // and the per-model fallback so a fresh load can answer both
        // tiers of the lookup hierarchy without scanning by_dir
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
        // pre-fallback shape was a single flat `{path: {model: level}}`
        // map (transparent serde). new shape requires `by_dir` /
        // `last_for_model` keys, so old files fail to load and reset
        let old = r#"{"/home/rowan/dev/mush":{"gpt-5":"high"}}"#;
        assert!(serde_json::from_str::<ThinkingPrefs>(old).is_err());
    }

    #[test]
    fn deserialises_old_flat_shape_as_error() {
        // even older `{model_id: level}` shape (pre per-dir migration)
        // also fails to load — the schema has only ever moved forward
        let old = r#"{"gpt-5":"high","claude-opus-4-7":"medium"}"#;
        assert!(serde_json::from_str::<ThinkingPrefs>(old).is_err());
    }

    #[test]
    fn deserialises_without_per_model_section_for_forward_compat() {
        // the per-model fallback was added later; files written with
        // only `by_dir` populated must still load cleanly (the fallback
        // map starts empty and repopulates as the user sets levels)
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
        // when the user picks a level for a model in dir A, then later
        // opens the same model from dir B, the fallback `last_for_model`
        // hands back dir A's choice so the level isn't reset to the
        // upstream default. tracks the most recent insertion across all
        // directories
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::High);
        prefs.set(PathBuf::from("/b"), "gpt-5".into(), ThinkingLevel::Low);
        // the fallback returns the most recent level for that model
        // regardless of which dir it was set in
        assert_eq!(
            prefs.last_for_model("gpt-5"),
            Some(ThinkingLevel::Low),
            "last_for_model should return the most recent level"
        );
        // unknown model has no fallback
        assert!(prefs.last_for_model("missing-model").is_none());
    }

    #[test]
    fn last_for_model_normalises_minimal_to_low() {
        // legacy ThinkingLevel::Minimal collapses to Low in the visible
        // controls; the per-model fallback must follow the same rule so
        // we don't surface a level the picker can't render
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::Minimal);
        assert_eq!(prefs.last_for_model("gpt-5"), Some(ThinkingLevel::Low));
    }

    #[test]
    fn set_in_new_dir_updates_per_model_fallback_for_old_dir() {
        // the per-model fallback always reflects the latest pick. so
        // setting a different level in a new dir overrides the fallback
        // for *all* dirs that previously didn't have an explicit pick
        let mut prefs = ThinkingPrefs::default();
        prefs.set(PathBuf::from("/a"), "gpt-5".into(), ThinkingLevel::High);
        // user now uses gpt-5 in /b and picks Low
        prefs.set(PathBuf::from("/b"), "gpt-5".into(), ThinkingLevel::Low);
        // /a still gets High (explicit per-dir pick wins over fallback)
        assert_eq!(
            prefs.get(Path::new("/a"), "gpt-5"),
            Some(ThinkingLevel::High)
        );
        // /c (no entry) gets the most recent fallback
        assert_eq!(prefs.last_for_model("gpt-5"), Some(ThinkingLevel::Low));
    }
}
