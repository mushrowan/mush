use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub type LastModelsSaver = std::sync::Arc<dyn Fn(&LastModels) + Send + Sync>;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LastModels {
    by_dir: HashMap<PathBuf, String>,
}

impl LastModels {
    #[must_use]
    pub fn get(&self, dir: &Path) -> Option<&str> {
        self.by_dir.get(dir).map(String::as_str)
    }

    pub fn set(&mut self, dir: PathBuf, model_id: String) {
        self.by_dir.insert(dir, model_id);
    }

    pub fn prune_missing_dirs(&mut self) {
        self.by_dir.retain(|p, _| p.exists());
    }

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
        assert!(serde_json::from_str::<LastModels>("gpt-5\n").is_err());
    }
}
