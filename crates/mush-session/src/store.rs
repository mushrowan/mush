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

    #[error("session {0} is open in another mush. mush --resume {0} attaches to it")]
    Busy(SessionId),
}

#[derive(Debug)]
pub struct SessionLease {
    _file: std::fs::File,
    id: SessionId,
}

impl SessionLease {
    #[must_use]
    pub fn id(&self) -> &SessionId {
        &self.id
    }
}

pub struct SessionStore {
    base_dir: PathBuf,
    logged: std::sync::Mutex<std::collections::HashMap<SessionId, Logged>>,
}

#[derive(Debug, Clone, Copy)]
struct Logged {
    entries: usize,
    bytes: u64,
    damaged: bool,
}

#[derive(serde::Serialize)]
struct StoredSession<'a> {
    meta: &'a SessionMeta,
    tree: StoredTree<'a>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    panes: &'a [crate::session::PaneSession],
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: &'a Option<String>,
}

#[derive(serde::Serialize)]
struct StoredTree<'a> {
    leaf_id: Option<&'a crate::tree::EntryId>,
}

impl<'a> From<&'a Session> for StoredSession<'a> {
    fn from(session: &'a Session) -> Self {
        Self {
            meta: &session.meta,
            tree: StoredTree {
                leaf_id: session.conversation.leaf_id(),
            },
            panes: &session.panes,
            system_prompt: &session.system_prompt,
        }
    }
}

impl SessionStore {
    fn lease_path(&self, id: &SessionId) -> PathBuf {
        self.base_dir.join(format!("{id}.lease"))
    }

    fn open_lease_file(&self, id: &SessionId) -> std::io::Result<std::fs::File> {
        std::fs::create_dir_all(&self.base_dir)?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.lease_path(id))
    }

    pub fn is_leased(&self, id: &SessionId) -> bool {
        if self.lease_path(id).symlink_metadata().is_err() {
            return false;
        }
        match self.open_lease_file(id) {
            Ok(file) => matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            Err(_) => true,
        }
    }

    pub fn acquire(&self, id: &SessionId) -> Result<SessionLease, StoreError> {
        let file = self.open_lease_file(id)?;
        match file.try_lock() {
            Ok(()) => {
                return Ok(SessionLease {
                    _file: file,
                    id: id.clone(),
                });
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            Err(std::fs::TryLockError::WouldBlock) => {}
        }
        let inherited = std::env::var("MUSH_SESSION_LEASE_FD")
            .ok()
            .map(|value| {
                value
                    .parse::<i32>()
                    .map_err(|_| std::io::Error::other("invalid session lease descriptor"))
            })
            .transpose()?;
        let Some(inherited) = inherited else {
            return Err(StoreError::Busy(id.clone()));
        };
        let file =
            mush_supervisor::admission::session_lock(&self.base_dir, id.as_str(), Some(inherited))
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        StoreError::Busy(id.clone())
                    } else {
                        StoreError::Io(error)
                    }
                })?;
        Ok(SessionLease {
            _file: file,
            id: id.clone(),
        })
    }

    pub fn default_dir() -> PathBuf {
        data_dir().join("sessions")
    }

    pub fn default_artifact_dir(id: &SessionId) -> PathBuf {
        data_dir().join("session-artifacts").join(id.as_str())
    }

    pub fn kernel_state_path(id: &SessionId) -> PathBuf {
        Self::default_artifact_dir(id).join("kernel-state.pickle")
    }

    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            logged: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn init(&self) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.base_dir)?;
        Ok(())
    }

    pub fn session_path(&self, id: &SessionId) -> PathBuf {
        self.base_dir.join(format!("{id}.json"))
    }

    pub fn history_path(&self, id: &SessionId) -> PathBuf {
        self.base_dir.join(format!("{id}.history.jsonl"))
    }

    fn logged(&self, id: &SessionId) -> Option<Logged> {
        let logged = self
            .logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .copied()?;
        let on_disk = std::fs::metadata(self.history_path(id)).ok()?.len();
        (on_disk == logged.bytes).then_some(logged)
    }

    fn remember(&self, id: &SessionId, logged: Logged) {
        self.logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), logged);
    }

    fn append_history(&self, session: &Session) -> Result<(), StoreError> {
        let id = &session.meta.id;
        let entries = session.conversation.tree().entries();
        let path = self.history_path(id);
        let known = self
            .logged(id)
            .filter(|logged| logged.entries <= entries.len());
        let start = known.map_or(0, |logged| logged.entries);
        let mut appended = Vec::new();
        for entry in &entries[start..] {
            serde_json::to_writer(&mut appended, entry)?;
            appended.push(b'\n');
        }
        let bytes = match known {
            Some(logged) if logged.damaged => {
                let mut repaired = repaired_history(&std::fs::read(&path)?);
                repaired.extend_from_slice(&appended);
                mush_ai::private_io::write_private(&path, &repaired)?;
                repaired.len() as u64
            }
            Some(logged) => {
                if !appended.is_empty() {
                    use std::io::Write;
                    let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
                    file.write_all(&appended)?;
                }
                logged.bytes + appended.len() as u64
            }
            None => {
                mush_ai::private_io::write_private(&path, &appended)?;
                appended.len() as u64
            }
        };
        self.remember(
            id,
            Logged {
                entries: entries.len(),
                bytes,
                damaged: false,
            },
        );
        Ok(())
    }

    pub fn save(&self, session: &Session) -> Result<(), StoreError> {
        self.init()?;
        self.append_history(session)?;
        let path = self.session_path(&session.meta.id);
        let json = serde_json::to_string_pretty(&StoredSession::from(session))?;
        mush_ai::private_io::write_private(&path, json)?;
        Ok(())
    }

    pub fn load(&self, id: &SessionId) -> Result<Session, StoreError> {
        let path = self.session_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let json = std::fs::read_to_string(path)?;
        let mut session: Session = serde_json::from_str(&json)?;
        let history = self.history_path(id);
        let Ok(bytes) = std::fs::read(&history) else {
            return Ok(session);
        };
        let History {
            mut entries,
            damaged,
        } = History::parse(&bytes);
        for entry in &mut entries {
            cap_entry(entry);
        }
        let logged = Logged {
            entries: entries.len(),
            bytes: bytes.len() as u64,
            damaged,
        };
        let tree =
            crate::tree::SessionTree::rebuild(entries, session.conversation.leaf_id().cloned())
                .map_err(|error| StoreError::Io(std::io::Error::other(error)))?;
        session.conversation = crate::ConversationState::from_tree(tree);
        self.remember(id, logged);
        Ok(session)
    }

    pub fn slim_history(&self, id: &SessionId, apply: bool) -> Result<SlimReport, StoreError> {
        let _lease = apply.then(|| self.acquire(id)).transpose()?;
        let path = self.history_path(id);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SlimReport::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut slimmed = Vec::with_capacity(bytes.len());
        let mut capped = 0;
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let capped_entry = serde_json::from_slice::<crate::tree::SessionEntry>(line)
                .ok()
                .and_then(|mut entry| cap_entry(&mut entry).then_some(entry));
            match capped_entry {
                Some(entry) => {
                    capped += 1;
                    serde_json::to_writer(&mut slimmed, &entry)?;
                }
                None => slimmed.extend_from_slice(line),
            }
            slimmed.push(b'\n');
        }
        if apply && capped > 0 {
            mush_ai::private_io::write_private(&path, &slimmed)?;
            self.logged
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
        }
        Ok(SlimReport {
            before: bytes.len() as u64,
            after: slimmed.len() as u64,
            capped,
        })
    }

    pub fn set_lifecycle(
        &self,
        id: &SessionId,
        lifecycle: crate::Lifecycle,
    ) -> Result<(), StoreError> {
        let path = self.session_path(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let mut stored: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let Some(meta) = stored
            .get_mut("meta")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return Err(StoreError::Io(std::io::Error::other(
                "the session has no meta",
            )));
        };
        match lifecycle {
            crate::Lifecycle::Live => {
                meta.remove("lifecycle");
            }
            crate::Lifecycle::Archived => {
                meta.insert("lifecycle".into(), serde_json::to_value(lifecycle)?);
            }
        }
        mush_ai::private_io::write_private(&path, serde_json::to_string_pretty(&stored)?)?;
        Ok(())
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
        let mut sessions: Vec<SessionMeta> = self
            .list_paths()?
            .iter()
            .filter_map(|path| load_meta(path).ok())
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
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
        let _ = std::fs::remove_file(self.lease_path(id));
        let _ = std::fs::remove_file(self.history_path(id));
        self.logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlimReport {
    pub before: u64,
    pub after: u64,
    pub capped: usize,
}

struct History {
    entries: Vec<crate::tree::SessionEntry>,
    damaged: bool,
}

impl History {
    fn parse(bytes: &[u8]) -> Self {
        let mut history = Self {
            entries: Vec::new(),
            damaged: false,
        };
        for line in history_lines(bytes) {
            history.damaged |= line.damaged;
            let Some(content) = line.content else {
                continue;
            };
            match serde_json::from_slice(content) {
                Ok(entry) => history.entries.push(entry),
                Err(_) => match serde_json::from_slice::<serde_json::Value>(content) {
                    Ok(value) => history.entries.extend(unknown_entry(value)),
                    Err(_) => history.damaged = true,
                },
            }
        }
        history
    }
}

#[derive(serde::Deserialize)]
struct EntryFrame {
    id: crate::tree::EntryId,
    parent_id: Option<crate::tree::EntryId>,
    timestamp: mush_ai::types::Timestamp,
}

fn unknown_entry(mut value: serde_json::Value) -> Option<crate::tree::SessionEntry> {
    let kind = value.get_mut("kind")?.take();
    let frame: EntryFrame = serde_json::from_value(value).ok()?;
    Some(crate::tree::SessionEntry {
        id: frame.id,
        parent_id: frame.parent_id,
        timestamp: frame.timestamp,
        kind: crate::tree::EntryKind::Unknown(kind),
    })
}

struct HistoryLine<'a> {
    content: Option<&'a [u8]>,
    damaged: bool,
}

fn history_lines(bytes: &[u8]) -> impl Iterator<Item = HistoryLine<'_>> {
    let terminated = bytes.last().is_none_or(|byte| *byte == b'\n');
    let body = if terminated && !bytes.is_empty() {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    let count = body.split(|byte| *byte == b'\n').count();
    body.split(|byte| *byte == b'\n')
        .enumerate()
        .map(move |(index, raw)| {
            let content = &raw[raw.iter().take_while(|byte| **byte == 0).count()..];
            let torn = !terminated && index + 1 == count;
            let zeroed = content.len() != raw.len();
            HistoryLine {
                content: (!content.is_empty()).then_some(content),
                damaged: torn || zeroed,
            }
        })
        .filter(|line| line.content.is_some() || line.damaged)
}

fn repaired_history(bytes: &[u8]) -> Vec<u8> {
    let mut repaired = Vec::with_capacity(bytes.len());
    for line in history_lines(bytes) {
        let Some(content) = line.content else {
            continue;
        };
        if serde_json::from_slice::<serde::de::IgnoredAny>(content).is_ok() {
            repaired.extend_from_slice(content);
            repaired.push(b'\n');
        }
    }
    repaired
}

fn cap_entry(entry: &mut crate::tree::SessionEntry) -> bool {
    match &mut entry.kind {
        crate::tree::EntryKind::Message {
            message: mush_ai::types::Message::ToolResult(result),
        } => result.cap_details(),
        _ => false,
    }
}

pub fn load_meta(path: &Path) -> Result<SessionMeta, StoreError> {
    struct MetaFirst<'a>(&'a mut Option<SessionMeta>);

    impl<'de> serde::de::Visitor<'de> for MetaFirst<'_> {
        type Value = ();

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a session object")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == "meta" {
                    *self.0 = Some(map.next_value()?);
                    return Ok(());
                }
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(())
        }
    }

    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut meta = None;
    // stopping at meta leaves the rest of the object unread, which serde_json
    // reports as an error, so a found meta wins over the parse result
    let parsed = serde::Deserializer::deserialize_map(
        &mut serde_json::Deserializer::from_reader(file),
        MetaFirst(&mut meta),
    );
    match (meta, parsed) {
        (Some(meta), _) => Ok(meta),
        (None, Err(error)) => Err(error.into()),
        (None, Ok(())) => {
            Err(<serde_json::Error as serde::de::Error>::missing_field("meta").into())
        }
    }
}

pub use mush_ai::dirs::data_dir;

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

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::from_ms(1000))
                .build(),
        )
    }

    #[test]
    fn archiving_a_session_marks_only_its_lifecycle_and_live_clears_it() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("keep me"));
        store.save(&session).unwrap();

        store
            .set_lifecycle(&session.meta.id, crate::Lifecycle::Archived)
            .unwrap();
        let archived = store.load(&session.meta.id).unwrap();
        assert_eq!(archived.meta.lifecycle, crate::Lifecycle::Archived);
        assert_eq!(archived.meta.message_count, 1);
        assert_eq!(archived.context().len(), 1);
        assert_eq!(archived.meta.updated_at, session.meta.updated_at);

        store
            .set_lifecycle(&session.meta.id, crate::Lifecycle::Live)
            .unwrap();
        let json = std::fs::read_to_string(store.session_path(&session.meta.id)).unwrap();
        assert!(!json.contains("lifecycle"), "{json}");
        assert!(
            store
                .set_lifecycle(&SessionId::from("missing"), crate::Lifecycle::Archived)
                .is_err()
        );
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
    fn a_session_saved_twice_appends_only_the_new_history() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("one"));
        store.save(&session).unwrap();
        let first = std::fs::metadata(store.history_path(&session.meta.id))
            .unwrap()
            .len();

        session.push_message(user("two"));
        store.save(&session).unwrap();
        let second = std::fs::metadata(store.history_path(&session.meta.id))
            .unwrap()
            .len();

        assert!(second > first, "{second} should be past {first}");
        let history = std::fs::read_to_string(store.history_path(&session.meta.id)).unwrap();
        assert_eq!(history.lines().count(), 2);
        assert!(history.starts_with(&history[..first as usize]));

        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(loaded.context().len(), 2);
    }

    #[test]
    fn the_session_file_carries_no_history() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("a message that only lives in the history log"));
        store.save(&session).unwrap();

        let json = std::fs::read_to_string(store.session_path(&session.meta.id)).unwrap();
        assert!(!json.contains("a message that only lives in the history log"));
        assert!(json.contains("leaf_id"));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("messages").is_none());
        assert!(value["tree"].get("entries").is_none());
    }

    #[test]
    fn an_old_session_file_keeps_working_and_migrates_on_save() {
        let (dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("from the old format"));
        let legacy = serde_json::to_string_pretty(&session).unwrap();
        assert!(legacy.contains("from the old format"));
        let _ = dir;
        store.init().unwrap();
        std::fs::write(store.session_path(&session.meta.id), &legacy).unwrap();

        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(loaded.context().len(), 1);
        assert!(!store.history_path(&session.meta.id).exists());

        store.save(&loaded).unwrap();
        assert!(store.history_path(&session.meta.id).exists());
        let json = std::fs::read_to_string(store.session_path(&session.meta.id)).unwrap();
        assert!(!json.contains("from the old format"));
        assert_eq!(store.load(&session.meta.id).unwrap().context().len(), 1);
    }

    fn append_raw(store: &SessionStore, id: &SessionId, bytes: &[u8]) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(store.history_path(id))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn every_line_parses(store: &SessionStore, id: &SessionId) -> usize {
        let history = std::fs::read(store.history_path(id)).unwrap();
        assert_eq!(history.last(), Some(&b'\n'));
        assert!(!history.contains(&0));
        let lines: Vec<serde_json::Value> = history
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        lines.len()
    }

    #[test]
    fn a_torn_tail_is_dropped_on_load_and_repaired_before_the_next_append() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("one"));
        session.push_message(user("two"));
        store.save(&session).unwrap();
        append_raw(
            &store,
            &session.meta.id,
            b"{\"id\":\"torn\",\"parent_id\":nu",
        );

        let reopened = SessionStore::new(store.base_dir.clone());
        let mut loaded = reopened.load(&session.meta.id).unwrap();
        assert_eq!(loaded.context().len(), 2);
        loaded.push_message(user("three"));
        reopened.save(&loaded).unwrap();
        assert_eq!(every_line_parses(&reopened, &session.meta.id), 3);
        assert_eq!(reopened.load(&session.meta.id).unwrap().context().len(), 3);
    }

    #[test]
    fn a_whole_entry_missing_its_newline_or_a_zeroed_tail_survive_the_repair() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("one"));
        session.push_message(user("two"));
        store.save(&session).unwrap();
        let path = store.history_path(&session.meta.id);
        let history = std::fs::read(&path).unwrap();
        std::fs::write(&path, &history[..history.len() - 1]).unwrap();

        let reopened = SessionStore::new(store.base_dir.clone());
        let mut loaded = reopened.load(&session.meta.id).unwrap();
        assert_eq!(loaded.context().len(), 2);
        loaded.push_message(user("three"));
        reopened.save(&loaded).unwrap();
        assert_eq!(every_line_parses(&reopened, &session.meta.id), 3);

        append_raw(&reopened, &session.meta.id, b"\0\0\0\0\0\0");
        let zeroed = SessionStore::new(store.base_dir.clone());
        let mut loaded = zeroed.load(&session.meta.id).unwrap();
        assert_eq!(loaded.context().len(), 3);
        loaded.push_message(user("four"));
        zeroed.save(&loaded).unwrap();
        assert_eq!(every_line_parses(&zeroed, &session.meta.id), 4);
        assert_eq!(zeroed.load(&session.meta.id).unwrap().context().len(), 4);
    }

    #[test]
    fn an_unknown_entry_kind_is_kept_in_the_tree_and_on_disk() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("one"));
        store.save(&session).unwrap();
        let parent = session.conversation.leaf_id().unwrap().to_string();
        let future = format!(
            "{{\"id\":\"future01\",\"parent_id\":\"{parent}\",\"timestamp\":5,\"kind\":{{\"type\":\"hologram\",\"depth\":3}}}}"
        );
        let alien = "{\"id\":\"alien001\",\"parent_id\":\"future01\",\"timestamp\":6,\"kind\":{\"type\":\"message\",\"message\":{\"role\":\"martian\"}}}";
        let mut child = user("after the future");
        if let Message::User(user) = &mut child {
            user.timestamp_ms = Timestamp::from_ms(7);
        }
        let child = serde_json::json!({
            "id": "child001",
            "parent_id": "alien001",
            "timestamp": 7,
            "kind": {"type": "message", "message": child},
        });
        append_raw(
            &store,
            &session.meta.id,
            format!("{future}\n{alien}\n{child}\n").as_bytes(),
        );
        let mut stored: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(store.session_path(&session.meta.id)).unwrap(),
        )
        .unwrap();
        stored["tree"]["leaf_id"] = "child001".into();
        std::fs::write(store.session_path(&session.meta.id), stored.to_string()).unwrap();

        let reopened = SessionStore::new(store.base_dir.clone());
        let mut loaded = reopened.load(&session.meta.id).unwrap();
        let texts: Vec<String> = loaded
            .context()
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(user.text()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["one", "after the future"]);
        assert!(matches!(
            loaded.conversation.tree().entries()[1].kind,
            crate::tree::EntryKind::Unknown(_)
        ));

        loaded.push_message(user("two"));
        reopened.save(&loaded).unwrap();
        let history = std::fs::read_to_string(reopened.history_path(&session.meta.id)).unwrap();
        assert!(history.contains(&future), "{history}");
        assert!(history.contains(alien), "{history}");

        reopened.logged.lock().unwrap().remove(&session.meta.id);
        loaded.push_message(user("three"));
        reopened.save(&loaded).unwrap();
        let rewritten = std::fs::read_to_string(reopened.history_path(&session.meta.id)).unwrap();
        assert!(rewritten.contains("\"type\":\"hologram\""), "{rewritten}");
        assert!(rewritten.contains("\"role\":\"martian\""), "{rewritten}");
        assert_eq!(reopened.load(&session.meta.id).unwrap().context().len(), 4);
    }

    #[test]
    fn a_history_log_that_does_not_match_its_record_is_rewritten() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.push_message(user("one"));
        store.save(&session).unwrap();

        std::fs::write(store.history_path(&session.meta.id), b"").unwrap();
        session.push_message(user("two"));
        store.save(&session).unwrap();

        let history = std::fs::read_to_string(store.history_path(&session.meta.id)).unwrap();
        assert_eq!(history.lines().count(), 2);
        assert_eq!(store.load(&session.meta.id).unwrap().context().len(), 2);
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
    fn loading_caps_oversized_tool_details_but_keeps_the_content() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        let stdout = "x\n".repeat(mush_ai::types::DETAIL_TEXT_LIMIT);
        session
            .conversation
            .append_message(Message::ToolResult(ToolResultMessage {
                tool_call_id: ToolCallId::from("call"),
                tool_name: ToolName::from("ipython"),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "shown to the model".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: Some(serde_json::json!({ "stdout": stdout })),
            }));
        store.save(&session).unwrap();

        let loaded = store.load(&session.meta.id).unwrap();

        let context = loaded.context();
        let [Message::ToolResult(result)] = context.as_slice() else {
            panic!("{context:?}");
        };
        let kept = result.details.as_ref().unwrap()["stdout"].as_str().unwrap();
        assert!(
            kept.len() <= mush_ai::types::DETAIL_TEXT_LIMIT + 64,
            "{}",
            kept.len()
        );
        assert_eq!(
            result.content,
            vec![ToolResultContentPart::Text(TextContent {
                text: "shown to the model".into(),
            })]
        );
    }

    fn bloated_tool_result() -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: ToolCallId::from("call"),
            tool_name: ToolName::from("ipython"),
            content: vec![ToolResultContentPart::Text(TextContent {
                text: "shown to the model".into(),
            })],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
            details: Some(serde_json::json!({
                "stdout": "x\n".repeat(mush_ai::types::DETAIL_TEXT_LIMIT)
            })),
        })
    }

    #[test]
    fn slimming_rewrites_only_bloated_entries_and_only_when_applied() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.conversation.append_message(user("before"));
        session.conversation.append_message(bloated_tool_result());
        session.conversation.append_message(user("after"));
        store.save(&session).unwrap();
        let path = store.history_path(&session.meta.id);
        let original = std::fs::read_to_string(&path).unwrap();
        let loaded_before = store.load(&session.meta.id).unwrap().context();

        let dry = store.slim_history(&session.meta.id, false).unwrap();
        assert_eq!(dry.capped, 1);
        assert!(dry.after + 60_000 < dry.before, "{dry:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        let applied = store.slim_history(&session.meta.id, true).unwrap();
        assert_eq!(applied, dry);
        let slimmed = std::fs::read_to_string(&path).unwrap();
        assert_eq!(slimmed.len() as u64, applied.after);
        let (before, after): (Vec<_>, Vec<_>) =
            (original.lines().collect(), slimmed.lines().collect());
        assert_eq!(before[0], after[0]);
        assert_eq!(before[2], after[2]);
        assert_eq!(
            store.load(&session.meta.id).unwrap().context(),
            loaded_before
        );
        assert_eq!(
            store.slim_history(&session.meta.id, true).unwrap().capped,
            0
        );
    }

    #[test]
    fn slimming_refuses_a_session_that_is_open() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.conversation.append_message(bloated_tool_result());
        store.save(&session).unwrap();
        let _lease = store.acquire(&session.meta.id).unwrap();

        assert!(matches!(
            store.slim_history(&session.meta.id, true),
            Err(StoreError::Busy(_))
        ));
        assert_eq!(
            store.slim_history(&session.meta.id, false).unwrap().capped,
            1
        );
    }

    #[test]
    fn a_slimmed_session_keeps_appending_after_it() {
        let (_dir, store) = temp_store();
        let mut session = Session::new("test-model", "/tmp");
        session.conversation.append_message(bloated_tool_result());
        store.save(&session).unwrap();
        store.slim_history(&session.meta.id, true).unwrap();

        session.conversation.append_message(user("later"));
        store.save(&session).unwrap();

        let loaded = store.load(&session.meta.id).unwrap();
        assert_eq!(loaded.context().len(), 2);
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
    fn listing_skips_corrupt_and_empty_files() {
        let (dir, store) = temp_store();
        let good = Session::new("model-a", "/tmp");
        store.save(&good).unwrap();

        let sessions_dir = dir.path().join("sessions");
        mush_ai::private_io::write_atomic(&sessions_dir.join("corrupt.json"), "not json").unwrap();
        mush_ai::private_io::write_atomic(&sessions_dir.join("empty.json"), "").unwrap();

        let sessions = store.list().unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, good.meta.id);
    }

    #[test]
    fn listing_reads_meta_wherever_it_sits_and_stops_once_found() {
        let (dir, store) = temp_store();
        let session = Session::new("model-a", "/tmp");
        let meta = serde_json::to_string(&session.meta).unwrap();
        let sessions_dir = dir.path().join("sessions");
        mush_ai::private_io::write_atomic(
            &sessions_dir.join("first.json"),
            format!(r#"{{"meta": {meta}, "tree": {{"entries": [not json"#),
        )
        .unwrap();
        mush_ai::private_io::write_atomic(
            &sessions_dir.join("last.json"),
            format!(r#"{{"tree": {{"entries": [1, {{"a": "b"}}]}}, "meta": {meta}}}"#),
        )
        .unwrap();
        mush_ai::private_io::write_atomic(&sessions_dir.join("none.json"), r#"{"tree": {}}"#)
            .unwrap();

        let sessions = store.list().unwrap();

        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|listed| listed.id == session.meta.id));
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
