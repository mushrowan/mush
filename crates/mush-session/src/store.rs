//! session persistence to disk
//!
//! sessions are stored as json files under `~/.local/share/mush/sessions/`.
//! each session gets its own file named by session id.

use std::path::{Path, PathBuf};

use crate::session::{Session, SessionId, SessionMeta};

/// errors from session store operations
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("session not found: {0}")]
    NotFound(String),
}

/// file-based session store
pub struct SessionStore {
    base_dir: PathBuf,
}

impl SessionStore {
    /// create a store using the default data directory
    pub fn default_dir() -> PathBuf {
        data_dir().join("sessions")
    }

    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// ensure the sessions directory exists
    pub fn init(&self) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.base_dir)?;
        Ok(())
    }

    fn session_path(&self, id: &SessionId) -> PathBuf {
        self.base_dir.join(format!("{id}.json"))
    }

    /// save a session to disk
    pub fn save(&self, session: &Session) -> Result<(), StoreError> {
        self.init()?;
        let path = self.session_path(&session.meta.id);
        let json = serde_json::to_string_pretty(session)?;
        mush_ai::private_io::write_private(&path, json)?;
        Ok(())
    }

    /// load a session by id
    pub fn load(&self, id: &SessionId) -> Result<Session, StoreError> {
        let path = self.session_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let json = std::fs::read_to_string(path)?;
        let session: Session = serde_json::from_str(&json)?;
        Ok(session)
    }

    /// list all session metadata (without loading full messages)
    pub fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        self.init()?;
        let mut sessions = Vec::new();

        for path in self.list_paths()? {
            match load_meta(&path) {
                Ok(meta) => sessions.push(meta),
                Err(_) => continue, // skip corrupt files
            }
        }

        // sort by updated_at descending (most recent first)
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }

    /// enumerate session file paths without parsing them. cheap
    /// readdir-only pass: callers that want to stream metadata
    /// progressively (e.g. the TUI `/sessions` picker) drive json
    /// parsing per-path off the hot path
    pub fn list_paths(&self) -> Result<Vec<PathBuf>, StoreError> {
        self.init()?;
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&self.base_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            paths.push(path);
        }
        Ok(paths)
    }

    /// delete a session
    pub fn delete(&self, id: &SessionId) -> Result<(), StoreError> {
        let path = self.session_path(id);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// load just the metadata from a session file without deserialising all messages
pub fn load_meta(path: &Path) -> Result<SessionMeta, StoreError> {
    // for now we load the full file - could optimise later with streaming json
    let json = std::fs::read_to_string(path)?;
    let session: Session = serde_json::from_str(&json)?;
    Ok(session.meta)
}

/// resolve the mush data directory
///
/// checks MUSH_DATA_DIR, XDG_DATA_HOME, HOME in order,
/// falling back to .mush in the current directory
pub fn data_dir() -> PathBuf {
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
    use super::*;
    use crate::session::Session;
    use mush_ai::types::*;

    fn temp_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        (dir, store)
    }

    #[test]
    fn save_and_load_session() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(UserMessage {
            content: UserContent::Text("hello".into()),
            timestamp_ms: Timestamp::from_ms(1000),
        }));

        store.save(&session).unwrap();
        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(loaded.meta.id, session.meta.id);
        assert_eq!(loaded.context().len(), 1);
    }

    #[test]
    fn load_nonexistent_session() {
        let (_dir, store) = temp_store();
        let id = SessionId::from("nonexistent");
        assert!(store.load(&id).is_err());
    }

    #[test]
    fn list_sessions() {
        let (_dir, store) = temp_store();

        let session1 = Session::new("model-a", "/tmp");
        let session2 = Session::new("model-b", "/tmp");
        store.save(&session1).unwrap();
        store.save(&session2).unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn delete_session() {
        let (_dir, store) = temp_store();
        let session = Session::new("test-model", "/tmp");
        store.save(&session).unwrap();

        store.delete(&session.meta.id).unwrap();
        assert!(store.load(&session.meta.id).is_err());
    }

    #[test]
    fn list_empty_store() {
        let (_dir, store) = temp_store();
        let list = store.list().unwrap();
        assert!(list.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn saved_session_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = temp_store();
        let session = Session::new("test-model", "/tmp");
        store.save(&session).unwrap();

        let path = store.session_path(&session.meta.id);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "session file contains user conversations and must be owner-only, got {mode:o}"
        );
    }
}
