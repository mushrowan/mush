//! file conflict tracking for multi-pane agents
//!
//! tracks which panes modify which files and provides advisory file locks.
//! used in "none" isolation mode (detect-and-warn) to alert users when
//! multiple agents touch the same file

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mush_ai::types::ToolCallId;
use serde::Deserialize;

use crate::pane::PaneId;

/// how pane file access is isolated
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// detect-and-warn: track modifications, advisory locks (default)
    #[default]
    None,
    /// git worktree per pane: full filesystem isolation
    Worktree,
    /// jj change per pane: separate changes in same working dir
    Jj,
}

impl std::fmt::Display for IsolationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Worktree => write!(f, "worktree"),
            Self::Jj => write!(f, "jj"),
        }
    }
}

/// auto-detect which isolation modes are available in the cwd
pub fn available_modes(cwd: &Path) -> Vec<IsolationMode> {
    let mut modes = vec![IsolationMode::None];
    if cwd.join(".jj").is_dir() {
        modes.push(IsolationMode::Jj);
    }
    if cwd.join(".git").is_dir() || cwd.join(".jj").is_dir() {
        modes.push(IsolationMode::Worktree);
    }
    modes
}

/// detected conflict: multiple panes modified the same file
#[derive(Debug, Clone)]
pub struct Conflict {
    pub path: PathBuf,
    pub other_panes: Vec<PaneId>,
}

/// tracks file modifications and advisory locks across panes
#[derive(Clone)]
pub struct FileTracker {
    /// pending tool operations: (pane_id, tool_call_id) -> resolved path
    pending: Arc<Mutex<HashMap<(PaneId, ToolCallId), PathBuf>>>,
    /// files modified by each pane: path -> set of pane ids
    modifications: Arc<Mutex<HashMap<PathBuf, BTreeSet<PaneId>>>>,
    /// advisory locks: path -> owning pane id
    locks: Arc<Mutex<HashMap<PathBuf, PaneId>>>,
    /// cwd for resolving relative paths
    cwd: PathBuf,
}

impl FileTracker {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            modifications: Arc::new(Mutex::new(HashMap::new())),
            locks: Arc::new(Mutex::new(HashMap::new())),
            cwd,
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }

    /// record a file-modifying tool starting (write/edit)
    pub fn record_tool_start(
        &self,
        pane_id: PaneId,
        tool_call_id: &ToolCallId,
        tool_name: &str,
        args: &serde_json::Value,
    ) {
        if matches!(tool_name, "write" | "edit")
            && let Some(path) = args["path"].as_str()
        {
            let resolved = self.resolve(path);
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((pane_id, tool_call_id.clone()), resolved);
        }
    }

    /// record a file-modifying tool completing. returns a conflict if
    /// another pane has already modified this file
    pub fn record_tool_end(
        &self,
        pane_id: PaneId,
        tool_call_id: &ToolCallId,
        success: bool,
    ) -> Option<Conflict> {
        let path = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(pane_id, tool_call_id.clone()))?;

        if !success {
            return None;
        }

        let mut mods = self.modifications.lock().unwrap_or_else(|e| e.into_inner());
        let panes = mods.entry(path.clone()).or_default();
        panes.insert(pane_id);

        if panes.len() > 1 {
            let others: Vec<_> = panes.iter().filter(|&&p| p != pane_id).copied().collect();
            Some(Conflict {
                path,
                other_panes: others,
            })
        } else {
            None
        }
    }

    /// check if a file is locked by another pane. returns the lock owner if so
    pub fn check_lock(&self, pane_id: PaneId, path: &str) -> Option<PaneId> {
        let resolved = self.resolve(path);
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .get(&resolved)
            .copied()
            .filter(|&owner| owner != pane_id)
    }

    /// acquire an advisory lock. returns Err(owner) if already locked by another pane
    pub fn lock(&self, pane_id: PaneId, path: &str) -> Result<(), PaneId> {
        let resolved = self.resolve(path);
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&owner) = locks.get(&resolved)
            && owner != pane_id
        {
            return Err(owner);
        }
        locks.insert(resolved, pane_id);
        Ok(())
    }

    /// release an advisory lock. returns false if not owned by this pane
    pub fn unlock(&self, pane_id: PaneId, path: &str) -> bool {
        let resolved = self.resolve(path);
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        if locks.get(&resolved) == Some(&pane_id) {
            locks.remove(&resolved);
            true
        } else {
            false
        }
    }

    /// list all locks as (path, owner) pairs
    pub fn list_locks(&self) -> Vec<(PathBuf, PaneId)> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(p, &id)| (p.clone(), id))
            .collect()
    }

    /// list files modified by a specific pane
    pub fn pane_modifications(&self, pane_id: PaneId) -> Vec<PathBuf> {
        self.modifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, panes)| panes.contains(&pane_id))
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// release all locks and pending operations for a pane (on close)
    pub fn release_pane(&self, pane_id: PaneId) {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, &mut owner| owner != pane_id);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|&(pid, _), _| pid != pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> FileTracker {
        FileTracker::new(PathBuf::from("/project"))
    }

    #[test]
    fn track_modification() {
        let ft = tracker();
        let p1 = PaneId::new(1);

        let tool_call_id = ToolCallId::from("tc1");
        ft.record_tool_start(
            p1,
            &tool_call_id,
            "write",
            &serde_json::json!({"path": "src/main.rs"}),
        );
        let conflict = ft.record_tool_end(p1, &tool_call_id, true);
        assert!(conflict.is_none());

        let mods = ft.pane_modifications(p1);
        assert_eq!(mods.len(), 1);
        assert!(mods[0].ends_with("src/main.rs"));
    }

    #[test]
    fn detect_conflict() {
        let ft = tracker();
        let p1 = PaneId::new(1);
        let p2 = PaneId::new(2);

        let first_tool_call = ToolCallId::from("tc1");
        let second_tool_call = ToolCallId::from("tc2");

        // pane 1 edits a file
        ft.record_tool_start(
            p1,
            &first_tool_call,
            "edit",
            &serde_json::json!({"path": "src/lib.rs"}),
        );
        let conflict = ft.record_tool_end(p1, &first_tool_call, true);
        assert!(conflict.is_none());

        // pane 2 edits the same file
        ft.record_tool_start(
            p2,
            &second_tool_call,
            "write",
            &serde_json::json!({"path": "src/lib.rs"}),
        );
        let conflict = ft.record_tool_end(p2, &second_tool_call, true);
        assert!(conflict.is_some());

        let c = conflict.unwrap();
        assert!(c.path.ends_with("src/lib.rs"));
        assert_eq!(c.other_panes, vec![p1]);
    }

    #[test]
    fn no_conflict_on_different_files() {
        let ft = tracker();
        let p1 = PaneId::new(1);
        let p2 = PaneId::new(2);

        let first_tool_call = ToolCallId::from("tc1");
        let second_tool_call = ToolCallId::from("tc2");

        ft.record_tool_start(
            p1,
            &first_tool_call,
            "write",
            &serde_json::json!({"path": "a.rs"}),
        );
        ft.record_tool_end(p1, &first_tool_call, true);

        ft.record_tool_start(
            p2,
            &second_tool_call,
            "write",
            &serde_json::json!({"path": "b.rs"}),
        );
        let conflict = ft.record_tool_end(p2, &second_tool_call, true);
        assert!(conflict.is_none());
    }

    #[test]
    fn failed_tool_not_tracked() {
        let ft = tracker();
        let p1 = PaneId::new(1);

        let tool_call_id = ToolCallId::from("tc1");
        ft.record_tool_start(
            p1,
            &tool_call_id,
            "write",
            &serde_json::json!({"path": "fail.rs"}),
        );
        let conflict = ft.record_tool_end(p1, &tool_call_id, false);
        assert!(conflict.is_none());
        assert!(ft.pane_modifications(p1).is_empty());
    }

    #[test]
    fn non_file_tools_ignored() {
        let ft = tracker();
        let p1 = PaneId::new(1);

        let tool_call_id = ToolCallId::from("tc1");
        ft.record_tool_start(
            p1,
            &tool_call_id,
            "bash",
            &serde_json::json!({"command": "ls"}),
        );
        // nothing pending, so record_tool_end is a no-op
        let conflict = ft.record_tool_end(p1, &tool_call_id, true);
        assert!(conflict.is_none());
    }

    #[test]
    fn advisory_lock() {
        let ft = tracker();
        let p1 = PaneId::new(1);
        let p2 = PaneId::new(2);

        assert!(ft.lock(p1, "config.toml").is_ok());
        // same pane can re-lock (idempotent)
        assert!(ft.lock(p1, "config.toml").is_ok());
        // different pane blocked
        assert_eq!(ft.lock(p2, "config.toml"), Err(p1));
        // check_lock
        assert_eq!(ft.check_lock(p2, "config.toml"), Some(p1));
        assert_eq!(ft.check_lock(p1, "config.toml"), None); // own lock is fine

        // unlock
        assert!(ft.unlock(p1, "config.toml"));
        assert!(ft.lock(p2, "config.toml").is_ok());
    }

    #[test]
    fn list_locks_shows_all() {
        let ft = tracker();
        ft.lock(PaneId::new(1), "a.rs").unwrap();
        ft.lock(PaneId::new(2), "b.rs").unwrap();
        let locks = ft.list_locks();
        assert_eq!(locks.len(), 2);
    }

    #[test]
    fn release_pane_cleans_up() {
        let ft = tracker();
        let p1 = PaneId::new(1);

        let tool_call_id = ToolCallId::from("tc1");
        ft.lock(p1, "locked.rs").unwrap();
        ft.record_tool_start(
            p1,
            &tool_call_id,
            "write",
            &serde_json::json!({"path": "pending.rs"}),
        );

        ft.release_pane(p1);

        assert!(ft.list_locks().is_empty());
        // pending was cleaned, so tool_end is a no-op
        assert!(ft.record_tool_end(p1, &tool_call_id, true).is_none());
    }

    #[test]
    fn unlock_wrong_pane_fails() {
        let ft = tracker();
        ft.lock(PaneId::new(1), "mine.rs").unwrap();
        assert!(!ft.unlock(PaneId::new(2), "mine.rs"));
    }
}
