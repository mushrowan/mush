//! task locking for shared-codebase multi-agent scenarios
//!
//! agents claim tasks by writing lock files to `.mush/tasks/`.
//! each lock file is a JSON manifest identifying the agent and
//! what it's working on. designed for cross-process coordination
//! where multiple mush sessions share a codebase.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// a claimed task
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskLock {
    /// unique task identifier (used as filename)
    pub id: String,
    /// human-readable description of the task
    pub description: String,
    /// which agent claimed this (typically pid or session id)
    pub agent: String,
    /// when the lock was acquired (unix timestamp)
    pub claimed_at: u64,
    /// optional list of files this task intends to modify
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// manages task locks in a `.mush/tasks/` directory
pub struct TaskStore {
    dir: PathBuf,
}

/// errors from task operations
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("task '{task}' already claimed by agent '{agent}'")]
    AlreadyClaimed { task: String, agent: String },
    #[error("task '{task}' not found")]
    NotFound { task: String },
    #[error("task '{task}' owned by agent '{owner}', not '{requester}'")]
    NotOwner {
        task: String,
        owner: String,
        requester: String,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl TaskStore {
    /// create a store rooted at `cwd/.mush/tasks`
    pub fn new(cwd: &Path) -> Self {
        Self {
            dir: cwd.join(".mush").join("tasks"),
        }
    }

    /// claim a task. fails if already claimed by another agent
    pub fn claim(&self, lock: &TaskLock) -> Result<(), TaskError> {
        std::fs::create_dir_all(&self.dir)?;

        let path = self.lock_path(&lock.id);
        if let Some(existing) = self.read_lock(&path)?
            && existing.agent != lock.agent
        {
            return Err(TaskError::AlreadyClaimed {
                task: lock.id.clone(),
                agent: existing.agent,
            });
        }

        let json = serde_json::to_string_pretty(lock)
            .unwrap_or_else(|_| "{{\"error\": \"serialisation failed\"}}".into());
        mush_ai::private_io::write_private(&path, json)?;
        Ok(())
    }

    /// release a task. only the owning agent can release it
    pub fn release(&self, task_id: &str, agent: &str) -> Result<(), TaskError> {
        let path = self.lock_path(task_id);
        let existing = self.read_lock(&path)?.ok_or_else(|| TaskError::NotFound {
            task: task_id.to_string(),
        })?;

        if existing.agent != agent {
            return Err(TaskError::NotOwner {
                task: task_id.to_string(),
                owner: existing.agent,
                requester: agent.to_string(),
            });
        }

        std::fs::remove_file(&path)?;
        Ok(())
    }

    /// list all currently claimed tasks
    pub fn list(&self) -> Result<Vec<TaskLock>, TaskError> {
        if !self.dir.exists() {
            return Ok(vec![]);
        }
        let mut tasks = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(lock) = self.read_lock(&path)?
            {
                tasks.push(lock);
            }
        }
        tasks.sort_by_key(|t| t.claimed_at);
        Ok(tasks)
    }

    /// check if any claimed task overlaps with a given file path
    pub fn file_conflicts(&self, file: &str) -> Result<Vec<TaskLock>, TaskError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|t| t.files.iter().any(|f| f == file))
            .collect())
    }

    fn lock_path(&self, task_id: &str) -> PathBuf {
        self.dir.join(format!("{task_id}.json"))
    }

    fn read_lock(&self, path: &Path) -> Result<Option<TaskLock>, TaskError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let lock: TaskLock = serde_json::from_str(&contents)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some(lock))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(TaskError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, TaskStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path());
        (dir, store)
    }

    fn sample_lock(id: &str, agent: &str) -> TaskLock {
        TaskLock {
            id: id.into(),
            description: format!("work on {id}"),
            agent: agent.into(),
            claimed_at: 1000,
            files: vec![],
        }
    }

    #[test]
    fn claim_and_list() {
        let (_dir, store) = test_store();
        let lock = sample_lock("task-1", "agent-a");

        store.claim(&lock).unwrap();

        let tasks = store.list().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "task-1");
        assert_eq!(tasks[0].agent, "agent-a");
    }

    #[test]
    fn claim_is_idempotent_for_same_agent() {
        let (_dir, store) = test_store();
        let lock = sample_lock("task-1", "agent-a");

        store.claim(&lock).unwrap();
        store.claim(&lock).unwrap(); // should not error
    }

    #[test]
    fn claim_rejects_different_agent() {
        let (_dir, store) = test_store();

        store.claim(&sample_lock("task-1", "agent-a")).unwrap();

        let err = store.claim(&sample_lock("task-1", "agent-b")).unwrap_err();
        assert!(matches!(err, TaskError::AlreadyClaimed { .. }));
        assert!(err.to_string().contains("agent-a"));
    }

    #[test]
    fn release_removes_lock() {
        let (_dir, store) = test_store();
        store.claim(&sample_lock("task-1", "agent-a")).unwrap();

        store.release("task-1", "agent-a").unwrap();

        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn release_rejects_wrong_agent() {
        let (_dir, store) = test_store();
        store.claim(&sample_lock("task-1", "agent-a")).unwrap();

        let err = store.release("task-1", "agent-b").unwrap_err();
        assert!(matches!(err, TaskError::NotOwner { .. }));
    }

    #[test]
    fn release_not_found() {
        let (_dir, store) = test_store();
        let err = store.release("nonexistent", "agent-a").unwrap_err();
        assert!(matches!(err, TaskError::NotFound { .. }));
    }

    #[test]
    fn list_empty_when_no_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_sorted_by_claimed_at() {
        let (_dir, store) = test_store();

        let mut lock1 = sample_lock("early", "agent-a");
        lock1.claimed_at = 100;
        let mut lock2 = sample_lock("late", "agent-b");
        lock2.claimed_at = 200;

        store.claim(&lock2).unwrap();
        store.claim(&lock1).unwrap();

        let tasks = store.list().unwrap();
        assert_eq!(tasks[0].id, "early");
        assert_eq!(tasks[1].id, "late");
    }

    #[test]
    fn file_conflicts_found() {
        let (_dir, store) = test_store();

        let mut lock = sample_lock("refactor", "agent-a");
        lock.files = vec!["src/main.rs".into(), "src/lib.rs".into()];
        store.claim(&lock).unwrap();

        let conflicts = store.file_conflicts("src/main.rs").unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].id, "refactor");

        let no_conflicts = store.file_conflicts("src/other.rs").unwrap();
        assert!(no_conflicts.is_empty());
    }

    #[test]
    fn task_lock_roundtrips_through_json() {
        let lock = TaskLock {
            id: "t1".into(),
            description: "fix bug".into(),
            agent: "agent-x".into(),
            claimed_at: 12345,
            files: vec!["a.rs".into()],
        };
        let json = serde_json::to_string(&lock).unwrap();
        let parsed: TaskLock = serde_json::from_str(&json).unwrap();
        assert_eq!(lock, parsed);
    }

    #[test]
    fn task_lock_json_omits_empty_files() {
        let lock = sample_lock("t1", "a");
        let json = serde_json::to_string(&lock).unwrap();
        assert!(!json.contains("files"));
    }
}
