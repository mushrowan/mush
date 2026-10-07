use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use crate::util::{canonical_or_given, lock};

static SHARED: LazyLock<ReadLedger> = LazyLock::new(ReadLedger::default);

#[derive(Debug, Clone, Default)]
pub struct ReadLedger(Arc<Mutex<HashMap<PathBuf, SystemTime>>>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRead {
    pub path: PathBuf,
}

impl std::fmt::Display for StaleRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} changed on disk since it was last read. read it again before editing",
            self.path.display()
        )
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl ReadLedger {
    #[must_use]
    pub fn shared() -> Self {
        SHARED.clone()
    }

    pub fn record(&self, path: &Path) {
        let Some(modified) = mtime(path) else {
            return;
        };
        lock(&self.0).insert(canonical_or_given(path), modified);
    }

    pub fn forget(&self, path: &Path) {
        lock(&self.0).remove(&canonical_or_given(path));
    }

    pub fn check(&self, path: &Path) -> Result<(), StaleRead> {
        let recorded = lock(&self.0).get(&canonical_or_given(path)).copied();
        match (recorded, mtime(path)) {
            (Some(recorded), Some(current)) if recorded != current => Err(StaleRead {
                path: path.to_path_buf(),
            }),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bump_mtime(path: &Path) {
        let later = filetime::FileTime::from_unix_time(4_102_444_800, 0);
        filetime::set_file_mtime(path, later).unwrap();
    }

    #[test]
    fn a_file_never_read_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();

        assert_eq!(ReadLedger::default().check(&path), Ok(()));
    }

    #[test]
    fn an_unchanged_file_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let ledger = ReadLedger::default();

        ledger.record(&path);

        assert_eq!(ledger.check(&path), Ok(()));
    }

    #[test]
    fn a_file_changed_after_the_read_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let ledger = ReadLedger::default();
        ledger.record(&path);

        bump_mtime(&path);

        assert_eq!(ledger.check(&path), Err(StaleRead { path: path.clone() }));
    }

    #[test]
    fn recording_again_clears_the_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let ledger = ReadLedger::default();
        ledger.record(&path);
        bump_mtime(&path);

        ledger.record(&path);

        assert_eq!(ledger.check(&path), Ok(()));
    }

    #[test]
    fn a_deleted_file_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let ledger = ReadLedger::default();
        ledger.record(&path);

        std::fs::remove_file(&path).unwrap();

        assert_eq!(ledger.check(&path), Ok(()));
    }

    #[test]
    fn the_same_file_by_another_path_is_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let alias = dir.path().join("./a.txt");
        let ledger = ReadLedger::default();
        ledger.record(&path);

        bump_mtime(&path);

        assert!(ledger.check(&alias).is_err());
    }
}
