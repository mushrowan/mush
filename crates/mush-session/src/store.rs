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

#[derive(Debug, Clone)]
pub struct SessionListReport {
    pub sessions: Vec<SessionMeta>,
    pub ignored: Vec<IgnoredSessionFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredSessionFile {
    pub path: PathBuf,
    pub reason: String,
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
        Ok(self.list_report()?.sessions)
    }

    pub fn list_report(&self) -> Result<SessionListReport, StoreError> {
        self.init()?;
        let mut sessions = Vec::new();
        let mut ignored = Vec::new();

        for path in self.list_paths()? {
            match load_meta(&path) {
                Ok(meta) => sessions.push(meta),
                Err(error) => ignored.push(IgnoredSessionFile {
                    path,
                    reason: ignored_session_reason(&error),
                }),
            }
        }

        // sort by updated_at descending (most recent first)
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        ignored.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(SessionListReport { sessions, ignored })
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

fn ignored_session_reason(error: &StoreError) -> String {
    match error {
        StoreError::Io(error) => {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                "zero-byte or truncated session file".into()
            } else {
                error.to_string()
            }
        }
        StoreError::Json(error) if error.is_eof() => "zero-byte or truncated session file".into(),
        other => other.to_string(),
    }
}

/// load just the metadata from a session file without deserialising all messages
pub fn load_meta(path: &Path) -> Result<SessionMeta, StoreError> {
    #[derive(serde::Deserialize)]
    struct MetaEnvelope {
        meta: SessionMeta,
    }

    let json = std::fs::read_to_string(path)?;
    let envelope: MetaEnvelope = serde_json::from_str(&json)?;
    Ok(envelope.meta)
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
    fn load_meta_ignores_large_or_corrupt_message_payloads() {
        let (dir, _store) = temp_store();
        let path = dir.path().join("sessions").join("meta-only.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        mush_ai::private_io::write_atomic(
            &path,
            r#"{
                "meta": {
                    "id": "meta-only",
                    "title": "metadata survives",
                    "model_id": "test-model",
                    "created_at": 1000,
                    "updated_at": 2000,
                    "message_count": 42,
                    "cwd": "/tmp/project"
                },
                "messages": "this would fail full session deserialisation"
            }"#,
        )
        .unwrap();

        let meta = load_meta(&path).unwrap();

        assert_eq!(meta.id, SessionId::from("meta-only"));
        assert_eq!(meta.title.as_deref(), Some("metadata survives"));
        assert_eq!(meta.message_count, 42);
    }

    #[test]
    fn system_prompt_round_trips() {
        // the system prompt is part of the cached request prefix on the
        // anthropic side. if we don't persist it with the session, a
        // resumed session re-derives the prompt from current AGENTS.md
        // and any drift busts the cache on the first call. round-trip
        // through json must preserve the exact string
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.system_prompt = Some("you are running inside mush\n\nproject context: foo".into());
        store.save(&session).unwrap();

        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(
            loaded.system_prompt.as_deref(),
            Some("you are running inside mush\n\nproject context: foo")
        );
    }

    #[test]
    fn legacy_session_without_system_prompt_loads_with_none() {
        // older session files were written before system_prompt existed.
        // they must still load, deserialising the missing field as None
        let (dir, store) = temp_store();
        let id = SessionId::from("legacy");
        let legacy_json = format!(
            r#"{{
                "meta": {{
                    "id": "{id}",
                    "title": null,
                    "model_id": "test-model",
                    "created_at": 0,
                    "updated_at": 0,
                    "message_count": 0,
                    "cwd": "/tmp"
                }},
                "tree_nodes": [],
                "current_branch": null,
                "panes": []
            }}"#
        );
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        mush_ai::private_io::write_atomic(
            &dir.path().join("sessions").join(format!("{id}.json")),
            legacy_json,
        )
        .unwrap();

        let loaded = store.load(&id).unwrap();
        assert!(loaded.system_prompt.is_none());
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
    fn list_report_includes_corrupt_and_empty_files() {
        let (dir, store) = temp_store();
        let good = Session::new("model-a", "/tmp");
        store.save(&good).unwrap();

        let sessions_dir = dir.path().join("sessions");
        mush_ai::private_io::write_atomic(&sessions_dir.join("corrupt.json"), "not json").unwrap();
        mush_ai::private_io::write_atomic(&sessions_dir.join("empty.json"), "").unwrap();

        let report = store.list_report().unwrap();

        assert_eq!(report.sessions.len(), 1);
        assert_eq!(report.sessions[0].id, good.meta.id);
        assert_eq!(report.ignored.len(), 2);
        assert!(report.ignored.iter().any(|ignored| {
            ignored.path.ends_with("corrupt.json") && ignored.reason.contains("json error")
        }));
        assert!(report.ignored.iter().any(|ignored| {
            ignored.path.ends_with("empty.json") && ignored.reason.contains("zero-byte")
        }));
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
