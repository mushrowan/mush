use std::path::{Path, PathBuf};

use crate::session::{Session, SessionId, SessionMeta};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("session not found: {0}")]
    NotFound(String),

    #[error("'{prefix}' matches {} sessions, be more specific", matches.len())]
    #[diagnostic(help("candidates: {}", matches.join(", ")))]
    AmbiguousId {
        prefix: String,
        matches: Vec<String>,
    },
}

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
    pub fn default_dir() -> PathBuf {
        data_dir().join("sessions")
    }

    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn init(&self) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.base_dir)?;
        Ok(())
    }

    fn session_path(&self, id: &SessionId) -> PathBuf {
        self.base_dir.join(format!("{id}.json"))
    }

    pub fn save(&self, session: &Session) -> Result<(), StoreError> {
        self.init()?;
        let path = self.session_path(&session.meta.id);
        let json = serde_json::to_string_pretty(session)?;
        mush_ai::private_io::write_private(&path, json)?;
        Ok(())
    }

    pub fn load(&self, id: &SessionId) -> Result<Session, StoreError> {
        let path = self.session_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let json = std::fs::read_to_string(path)?;
        let session: Session = serde_json::from_str(&json)?;
        Ok(session)
    }

    pub fn resolve(&self, prefix: &str) -> Result<SessionMeta, StoreError> {
        let sessions = self.list()?;
        if let Some(exact) = sessions.iter().find(|s| s.id.as_str() == prefix) {
            return Ok(exact.clone());
        }
        let mut matches = sessions
            .into_iter()
            .filter(|s| s.id.as_str().starts_with(prefix));
        let Some(first) = matches.next() else {
            return Err(StoreError::NotFound(prefix.to_string()));
        };
        let rest: Vec<SessionMeta> = matches.collect();
        if rest.is_empty() {
            return Ok(first);
        }
        let mut ids = vec![first.id.to_string()];
        ids.extend(rest.iter().map(|s| s.id.to_string()));
        Err(StoreError::AmbiguousId {
            prefix: prefix.to_string(),
            matches: ids,
        })
    }

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

        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        ignored.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(SessionListReport { sessions, ignored })
    }

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

pub fn load_meta(path: &Path) -> Result<SessionMeta, StoreError> {
    #[derive(serde::Deserialize)]
    struct MetaEnvelope {
        meta: SessionMeta,
    }

    let json = std::fs::read_to_string(path)?;
    let envelope: MetaEnvelope = serde_json::from_str(&json)?;
    Ok(envelope.meta)
}

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
    fn resolve_accepts_an_unambiguous_id_prefix() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.meta.id = SessionId::from("abc123def456");
        store.save(&session).unwrap();

        let resolved = store.resolve("abc1").expect("prefix should resolve");
        assert_eq!(resolved.id, session.meta.id);

        let exact = store.resolve("abc123def456").expect("full id resolves");
        assert_eq!(exact.id, session.meta.id);
    }

    #[test]
    fn resolve_reports_no_match_and_ambiguity_separately() {
        let (_dir, store) = temp_store();
        for id in ["dup-one", "dup-two"] {
            let mut session = Session::new("test-model", "/tmp");
            session.meta.id = SessionId::from(id);
            store.save(&session).unwrap();
        }

        let missing = store.resolve("nope").expect_err("should not match");
        assert!(
            matches!(missing, StoreError::NotFound(_)),
            "got {missing:?}"
        );

        let ambiguous = store.resolve("dup-").expect_err("should be ambiguous");
        match ambiguous {
            StoreError::AmbiguousId { prefix, matches } => {
                assert_eq!(prefix, "dup-");
                assert_eq!(matches.len(), 2);
            }
            other => panic!("expected AmbiguousId, got {other:?}"),
        }
    }

    #[test]
    fn save_and_load_session() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hello".into()))
                .timestamp_ms(Timestamp::from_ms(1000))
                .build(),
        ));

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
