//! per-directory "most recently used model" persistence.
//!
//! the persisted shape is a flat map keyed by canonical cwd, so the
//! same project always boots with the model the user last switched to
//! while there. lives under `data_dir()` so we never pollute the
//! project tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// callback to persist the last-model map when it changes. takes the
/// whole map so the writer can serialise atomically.
pub type LastModelsSaver = std::sync::Arc<dyn Fn(&LastModels) + Send + Sync>;

/// per-directory record of the model the user last selected. loaded
/// once at startup and mutated as the user runs `/model <id>`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LastModels {
    by_dir: HashMap<PathBuf, String>,
}

impl LastModels {
    /// look up the saved model id for `dir`, or `None` if none is
    /// recorded.
    #[must_use]
    pub fn get(&self, dir: &Path) -> Option<&str> {
        self.by_dir.get(dir).map(String::as_str)
    }

    /// record `model_id` as the last-used model for `dir`.
    pub fn set(&mut self, dir: PathBuf, model_id: String) {
        self.by_dir.insert(dir, model_id);
    }

    /// drop entries pointing at directories that no longer exist on
    /// disk. called on load so the file stays bounded as users retire
    /// projects.
    pub fn prune_missing_dirs(&mut self) {
        self.by_dir.retain(|p, _| p.exists());
    }

    /// number of directories with a recorded model. test-only helper;
    /// production code should use [`Self::get`] / [`Self::set`].
    #[must_use]
    pub fn directory_count(&self) -> usize {
        self.by_dir.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_none_for_unknown_dir() {
        let last = LastModels::default();
        assert!(last.get(Path::new("/nope")).is_none());
    }

    #[test]
    fn set_then_get_round_trips_per_directory() {
        // each project carries its own last-model so resuming in dir A
        // doesn't drag dir B's choice along
        let mut last = LastModels::default();
        last.set(PathBuf::from("/a"), "gpt-5".into());
        last.set(PathBuf::from("/b"), "claude-opus-4-7".into());
        assert_eq!(last.get(Path::new("/a")), Some("gpt-5"));
        assert_eq!(last.get(Path::new("/b")), Some("claude-opus-4-7"));
    }

    #[test]
    fn set_replaces_existing_entry_for_same_directory() {
        let mut last = LastModels::default();
        last.set(PathBuf::from("/a"), "gpt-5".into());
        last.set(PathBuf::from("/a"), "claude-opus-4-7".into());
        assert_eq!(last.get(Path::new("/a")), Some("claude-opus-4-7"));
        assert_eq!(last.directory_count(), 1);
    }

    #[test]
    fn prune_missing_dirs_keeps_existent_drops_others() {
        // entries for retired projects shouldn't keep the file growing
        let tmp = tempfile::tempdir().unwrap();
        let mut last = LastModels::default();
        last.set(tmp.path().to_path_buf(), "gpt-5".into());
        last.set(
            PathBuf::from("/this/does/not/exist/anywhere"),
            "claude-opus-4-7".into(),
        );
        last.prune_missing_dirs();
        assert_eq!(last.directory_count(), 1);
        assert!(last.get(tmp.path()).is_some());
    }

    #[test]
    fn deserialises_old_plain_text_format_as_error() {
        // the pre-migration shape was a plain `model_id\n` text file at
        // last-model.txt. we moved to `last-model.json` so that file is
        // simply ignored; this test pins the parser's failure mode in
        // case anyone tries to feed plain text to the new path
        assert!(serde_json::from_str::<LastModels>("gpt-5\n").is_err());
    }
}
