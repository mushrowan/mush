use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mush_session::{SessionId, SessionLease};

use crate::keys::Keys;
use crate::local::{
    Cursor, HistoryStamp, Layout, Limits, SessionState, Stamp, append, artifact_files,
    complete_lines, copy_durably, live_only, read_range, remove, same_content, stamp, tail_hash,
    temp_beside, write_file,
};
use crate::protocol::{
    ARTIFACTS, Commit, FileRef, Head, HistoryRef, Lease, META, Manifest, blob_aad, chunk_aad,
    manifest_aad, now_ms, valid_session,
};
use crate::remote::{Committed, Remote, RemoteError};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("local storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Tampered(String),
    #[error("the sync server went back in time for session {0}, refusing to follow it")]
    Rollback(String),
}

impl SyncError {
    #[must_use]
    pub fn offline(&self) -> bool {
        matches!(self, Self::Remote(error) if error.offline())
    }
}

impl From<serde_json::Error> for SyncError {
    fn from(error: serde_json::Error) -> Self {
        Self::Io(error.into())
    }
}

#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub head: Head,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    Family(String),
    Sessions(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Both,
    PushOnly,
}

#[derive(Debug, Default)]
pub struct Report {
    pub pushed: Vec<String>,
    pub pulled: Vec<String>,
    pub forked: Vec<(String, String)>,
    pub deleted: Vec<String>,
    pub sidecars: Vec<(String, String)>,
    pub waiting: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
}

impl Report {
    #[must_use]
    pub fn quiet(&self) -> bool {
        self.pushed.is_empty()
            && self.pulled.is_empty()
            && self.forked.is_empty()
            && self.deleted.is_empty()
            && self.sidecars.is_empty()
            && self.failed.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Free,
    Ours,
    Busy,
}

#[derive(Debug, Clone)]
struct Entry {
    blob: String,
    len: u64,
    stamp: Stamp,
}

impl Entry {
    fn file(&self) -> FileRef {
        FileRef {
            blob: self.blob.clone(),
            len: self.len,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryView {
    Absent,
    Same,
    Appended { to: u64 },
    Rewritten { len: u64 },
}

#[derive(Debug)]
struct Local {
    meta: Option<Entry>,
    history: HistoryView,
    history_stamp: Option<Stamp>,
    files: BTreeMap<String, Entry>,
    held_back: BTreeSet<String>,
}

impl Local {
    fn exists(&self) -> bool {
        self.meta.is_some()
    }

    fn history_len(&self, base: Option<&HistoryRef>) -> Option<u64> {
        match self.history {
            HistoryView::Absent => None,
            HistoryView::Same => base.map(|history| history.len),
            HistoryView::Appended { to } => Some(to),
            HistoryView::Rewritten { len } => Some(len),
        }
    }

    fn prefix_intact(&self) -> bool {
        matches!(
            self.history,
            HistoryView::Same | HistoryView::Appended { .. }
        )
    }
}

#[derive(Clone, Copy)]
struct Seen<'a> {
    local: &'a Local,
    changes: &'a Delta,
}

#[derive(Debug, Default)]
struct Delta {
    core_local: bool,
    core_remote: bool,
    files_local: BTreeSet<String>,
    files_remote: BTreeSet<String>,
}

impl Delta {
    fn local(&self) -> bool {
        self.core_local || !self.files_local.is_empty()
    }
}

fn base_exists(state: Option<&SessionState>) -> bool {
    state
        .and_then(|state| state.manifest.as_ref())
        .is_some_and(|manifest| !manifest.deleted)
}

fn delta(local: &Local, state: Option<&SessionState>, theirs: Option<&Manifest>) -> Delta {
    let base = state.and_then(|state| state.manifest.as_ref());
    let base_file = |key: &str| {
        state
            .and_then(|state| state.file(key))
            .map(|file| &file.blob)
    };
    let core_local = match (local.exists(), base_exists(state)) {
        (true, true) => {
            let base_history = base.and_then(|base| base.history.as_ref());
            let history_same = match local.history {
                HistoryView::Same => true,
                HistoryView::Absent => base_history.is_none(),
                HistoryView::Appended { .. } | HistoryView::Rewritten { .. } => false,
            };
            local.meta.as_ref().map(|meta| &meta.blob) != base_file(META) || !history_same
        }
        (false, false) => false,
        _ => true,
    };
    let core_remote = theirs.is_some_and(|theirs| {
        let previous = base.cloned().unwrap_or_default();
        let exists = |manifest: &Manifest, known: bool| known && !manifest.deleted;
        exists(theirs, true) != exists(&previous, base.is_some())
            || theirs.files.get(META) != previous.files.get(META)
            || theirs.history != previous.history
    });
    let mut keys: BTreeSet<&String> = local.files.keys().collect();
    keys.extend(local.held_back.iter());
    if let Some(state) = state {
        keys.extend(state.skipped.keys());
        if let Some(base) = base {
            keys.extend(base.files.keys());
        }
    }
    if let Some(theirs) = theirs {
        keys.extend(theirs.files.keys());
    }
    let mut changes = Delta {
        core_local,
        core_remote,
        ..Delta::default()
    };
    for key in keys.into_iter().filter(|key| key.starts_with(ARTIFACTS)) {
        let ours = if local.held_back.contains(key) {
            base_file(key)
        } else {
            local.files.get(key).map(|entry| &entry.blob)
        };
        if ours != base_file(key) {
            changes.files_local.insert(key.clone());
        }
        let previous = base.and_then(|base| base.files.get(key));
        if let Some(theirs) = theirs
            && theirs.files.get(key) != previous
        {
            changes.files_remote.insert(key.clone());
        }
    }
    changes
}

struct Feed {
    entries: HashMap<String, RemoteEntry>,
    unreadable: Vec<(Head, String)>,
}

struct Download {
    start: u64,
    path: PathBuf,
}

impl Drop for Download {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

enum Pushed {
    Done,
    Conflict(Head),
    Locked(Lease),
}

pub struct Engine {
    layout: Layout,
    keys: Arc<Keys>,
    remote: Remote,
    machine: String,
    limits: Limits,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("machine", &self.machine)
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

fn upload_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Engine {
    #[must_use]
    pub fn new(
        layout: Layout,
        keys: Arc<Keys>,
        remote: Remote,
        machine: String,
        limits: Limits,
    ) -> Self {
        Self {
            layout,
            keys,
            remote,
            machine,
            limits,
        }
    }

    #[must_use]
    pub fn machine(&self) -> &str {
        &self.machine
    }

    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    #[must_use]
    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    #[must_use]
    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    async fn lock(&self, id: &str, patience: Duration) -> Result<Option<std::fs::File>, SyncError> {
        let deadline = tokio::time::Instant::now() + patience;
        loop {
            if let Some(lock) = self.layout.session_lock(id)? {
                return Ok(Some(lock));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn pass(
        &self,
        scope: Scope,
        direction: Direction,
        held: Option<&SessionLease>,
        patience: Duration,
    ) -> Result<Report, SyncError> {
        let mut cursor = Cursor::load(&self.layout)?;
        let Feed {
            entries: feed,
            unreadable,
        } = self.feed(&mut cursor).await?;
        let mut report = Report::default();
        if !unreadable.is_empty() {
            let mut known: HashMap<String, String> = HashMap::new();
            for id in self
                .layout
                .local_ids()?
                .into_iter()
                .chain(self.layout.state_ids()?)
            {
                known.insert(self.keys.sid(&id), id);
            }
            for (head, error) in unreadable {
                let session = known.get(&head.sid).cloned();
                if let Some(id) = &session {
                    let mut state = SessionState::load(&self.layout, id)?;
                    self.remember_remote(id, &mut state, head.version)?;
                }
                report.failed.push((session.unwrap_or(head.sid), error));
            }
        }
        let ids: Vec<String> = match &scope {
            Scope::All => {
                let mut ids = self.layout.local_ids()?;
                ids.extend(self.layout.state_ids()?);
                ids.extend(feed.keys().cloned());
                ids.into_iter().collect()
            }
            Scope::Family(root) => self.family(root, &feed)?,
            Scope::Sessions(ids) => ids.clone(),
        };
        let mut queue: VecDeque<String> = ids.into();
        let mut seen = HashSet::new();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let fed = feed.get(&id).cloned();
            let version = fed.as_ref().map(|entry| entry.head.version);
            let Some(_lock) = self.lock(&id, patience).await? else {
                self.remember(&id, version)?;
                report
                    .waiting
                    .push((id, "another mush sync is on it".to_owned()));
                continue;
            };
            match self.reconcile(&id, fed, direction, held, &mut report).await {
                Ok(Some(fork)) => queue.push_back(fork),
                Ok(None) => {}
                Err(error) if error.offline() => return Err(error),
                Err(error) => {
                    self.remember(&id, version)?;
                    report.failed.push((id, error.to_string()));
                }
            }
        }
        if scope == Scope::All && direction == Direction::Both {
            cursor.save(&self.layout)?;
        }
        Ok(report)
    }

    async fn feed(&self, cursor: &mut Cursor) -> Result<Feed, SyncError> {
        let mut entries = HashMap::new();
        let mut unreadable = Vec::new();
        let mut since = cursor.rev;
        loop {
            let changes = self.remote.changes(since, Duration::ZERO, false).await?;
            if changes.server != cursor.server {
                if !cursor.server.is_empty() {
                    tracing::warn!(
                        "the sync server has a new identity, comparing every session afresh"
                    );
                }
                self.layout.forget_states()?;
                cursor.server = changes.server;
                cursor.rev = 0;
                if since > 0 {
                    since = 0;
                    entries.clear();
                    unreadable.clear();
                    continue;
                }
            }
            for head in changes.heads {
                match self.open_head(head.clone()) {
                    Ok(Some(entry)) => {
                        entries.insert(entry.manifest.session.clone(), entry);
                    }
                    Ok(None) => {}
                    Err(error) => unreadable.push((head, error.to_string())),
                }
            }
            since = changes.rev;
            if !changes.more {
                break;
            }
        }
        cursor.rev = since;
        Ok(Feed {
            entries,
            unreadable,
        })
    }

    pub(crate) fn open_head(&self, head: Head) -> Result<Option<RemoteEntry>, SyncError> {
        let Some(sealed) = &head.manifest else {
            return Ok(None);
        };
        let sealed = STANDARD
            .decode(sealed)
            .map_err(|_| SyncError::Tampered("a session head is not base64".into()))?;
        let plain = self
            .keys
            .open(&manifest_aad(&head.sid, head.version), &sealed)
            .map_err(|error| SyncError::Tampered(error.to_string()))?;
        let manifest: Manifest = serde_json::from_slice(&plain)?;
        let consistent = valid_session(&manifest.session)
            && self.keys.sid(&manifest.session) == head.sid
            && manifest.history.as_ref().is_none_or(|history| {
                history.generation == head.generation && history.seq == head.seq
            });
        if !consistent {
            return Err(SyncError::Tampered(format!(
                "the head of {} does not match its manifest",
                head.sid
            )));
        }
        Ok(Some(RemoteEntry { head, manifest }))
    }

    async fn fetch_entry(&self, sid: &str) -> Result<Option<RemoteEntry>, SyncError> {
        match self.remote.head(sid).await? {
            Some(head) => self.open_head(head),
            None => Ok(None),
        }
    }

    fn parents(&self) -> std::io::Result<HashMap<String, String>> {
        let mut parents = HashMap::new();
        for id in self.layout.local_ids()? {
            if let Ok(meta) = mush_session::load_meta(&self.layout.meta(&id))
                && let Some(parent) = meta.parent
            {
                parents.insert(id, parent.to_string());
            }
        }
        Ok(parents)
    }

    fn family(
        &self,
        root: &str,
        feed: &HashMap<String, RemoteEntry>,
    ) -> std::io::Result<Vec<String>> {
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for (child, parent) in self.parents()? {
            children.entry(parent).or_default().push(child);
        }
        for (child, entry) in feed {
            if let Some(parent) = &entry.manifest.parent {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(child.clone());
            }
        }
        let mut family = vec![root.to_owned()];
        let mut index = 0;
        while let Some(id) = family.get(index).cloned() {
            index += 1;
            for child in children.remove(&id).unwrap_or_default() {
                if !family.contains(&child) {
                    family.push(child);
                }
            }
        }
        Ok(family)
    }

    fn root_of(&self, id: &str) -> String {
        let mut current = id.to_owned();
        for _ in 0..32 {
            match mush_session::load_meta(&self.layout.meta(&current))
                .ok()
                .and_then(|meta| meta.parent)
            {
                Some(parent) if parent.as_str() != current => current = parent.to_string(),
                _ => break,
            }
        }
        current
    }

    fn liveness(&self, id: &str, held: Option<&SessionLease>) -> Liveness {
        let root = self.root_of(id);
        let held_id = held.map(|lease| lease.id().as_str());
        if held_id == Some(id) || held_id == Some(root.as_str()) {
            return Liveness::Ours;
        }
        let store = self.layout.store();
        let leased = |session: &str| store.is_leased(&SessionId::from(session));
        if leased(id) || (root != id && leased(&root)) {
            Liveness::Busy
        } else {
            Liveness::Free
        }
    }

    fn write_leases(&self, id: &str, held: Option<&SessionLease>) -> Option<Vec<SessionLease>> {
        let store = self.layout.store();
        let mut wanted = vec![id.to_owned()];
        let root = self.root_of(id);
        if root != id {
            wanted.push(root);
        }
        let mut leases = Vec::new();
        for session in wanted {
            if held.is_some_and(|lease| lease.id().as_str() == session) {
                continue;
            }
            leases.push(store.acquire(&SessionId::from(session.as_str())).ok()?);
        }
        Some(leases)
    }

    fn entry(
        &self,
        key: &str,
        path: &Path,
        now: Stamp,
        state: Option<&SessionState>,
    ) -> std::io::Result<Entry> {
        let known = state.and_then(|state| Some((state.stamps.get(key)?, state.file(key)?)));
        let (blob, len) = match known {
            Some((seen, file)) if *seen == now => (file.blob.clone(), file.len),
            _ => {
                let bytes = std::fs::read(path)?;
                (self.keys.blob_id(&bytes), bytes.len() as u64)
            }
        };
        Ok(Entry {
            blob,
            len,
            stamp: now,
        })
    }

    fn history_view(
        &self,
        id: &str,
        state: Option<&SessionState>,
    ) -> std::io::Result<(HistoryView, Option<Stamp>)> {
        let path = self.layout.history(id);
        let Some(now) = stamp(&path)? else {
            return Ok((HistoryView::Absent, None));
        };
        let base = state.and_then(|state| {
            Some((
                state.manifest.as_ref()?.history.as_ref()?,
                state.history.as_ref()?,
            ))
        });
        let Some((reference, seen)) = base else {
            let len = complete_lines(&path, 0, now.len)?;
            return Ok((HistoryView::Rewritten { len }, Some(now)));
        };
        if now == seen.stamp {
            return Ok((HistoryView::Same, Some(now)));
        }
        let intact = now.dev == seen.stamp.dev
            && now.ino == seen.stamp.ino
            && now.len >= reference.len
            && tail_hash(&path, reference.len)? == seen.tail;
        if !intact {
            let len = complete_lines(&path, 0, now.len)?;
            return Ok((HistoryView::Rewritten { len }, Some(now)));
        }
        let to = complete_lines(&path, reference.len, now.len)?;
        let view = if to > reference.len {
            HistoryView::Appended { to }
        } else {
            HistoryView::Same
        };
        Ok((view, Some(now)))
    }

    fn scan(&self, id: &str, state: Option<&SessionState>, live: bool) -> Result<Local, SyncError> {
        let meta_path = self.layout.meta(id);
        let meta = match stamp(&meta_path)? {
            Some(now) if now.len > self.limits.file => {
                return Err(SyncError::Io(std::io::Error::other(format!(
                    "{id}.json is {} bytes, past the sync limit",
                    now.len
                ))));
            }
            Some(now) => Some(self.entry(META, &meta_path, now, state)?),
            None => None,
        };
        let (history, history_stamp) = if meta.is_some() {
            self.history_view(id, state)?
        } else {
            (HistoryView::Absent, None)
        };
        let mut files = BTreeMap::new();
        let mut held_back = BTreeSet::new();
        for (key, (path, now)) in artifact_files(&self.layout.artifact_dir(id))? {
            let excluded = !self.limits.allows(&key, now.len)
                || !self.limits.current(&key, &now)
                || (live && live_only(&key));
            if excluded {
                held_back.insert(key);
                continue;
            }
            let entry = self.entry(&key, &path, now, state)?;
            files.insert(key, entry);
        }
        Ok(Local {
            meta,
            history,
            history_stamp,
            files,
            held_back,
        })
    }

    fn remember(&self, id: &str, version: Option<u64>) -> std::io::Result<()> {
        let Some(version) = version else {
            return Ok(());
        };
        let mut state = SessionState::load(&self.layout, id)?;
        if state.as_ref().is_some_and(|state| state.version >= version) {
            return Ok(());
        }
        self.remember_remote(id, &mut state, version)
    }

    fn remember_remote(
        &self,
        id: &str,
        state: &mut Option<SessionState>,
        version: u64,
    ) -> std::io::Result<()> {
        let state = state.get_or_insert_with(SessionState::default);
        if state.remote_version != version {
            state.remote_version = version;
            state.save(&self.layout, id)?;
        }
        Ok(())
    }

    async fn reconcile(
        &self,
        id: &str,
        mut theirs: Option<RemoteEntry>,
        direction: Direction,
        held: Option<&SessionLease>,
        report: &mut Report,
    ) -> Result<Option<String>, SyncError> {
        let sid = self.keys.sid(id);
        let mut state = SessionState::load(&self.layout, id)?;
        if theirs.is_none()
            && let Some(known) = &state
            && known.remote_version > known.version
        {
            theirs = self.fetch_entry(&sid).await?;
        }
        if let (Some(entry), Some(known)) = (&theirs, &state)
            && entry.head.version == known.version
        {
            theirs = None;
        }
        if theirs.is_none()
            && let Some(known) = &state
            && known.remote_version <= known.version
            && self.untouched(id, known)?
        {
            return Ok(None);
        }
        let mut forked = None;
        for _ in 0..4 {
            let version = state.as_ref().map_or(0, |state| state.version);
            if let Some(entry) = &theirs {
                if entry.head.version < version {
                    return Err(SyncError::Rollback(id.to_owned()));
                }
                if entry.head.version == version {
                    theirs = None;
                }
            }
            let live = self.liveness(id, held);
            let local = self.scan(id, state.as_ref(), live != Liveness::Free)?;
            let changes = delta(
                &local,
                state.as_ref(),
                theirs.as_ref().map(|entry| &entry.manifest),
            );
            match theirs.take() {
                Some(entry) => {
                    let waiting = match (direction, live) {
                        (Direction::PushOnly, _) => Some("open here, syncs when it closes"),
                        (_, Liveness::Busy) => {
                            Some("open in another mush here, syncs when it closes")
                        }
                        _ => None,
                    };
                    if let Some(reason) = waiting {
                        self.remember_remote(id, &mut state, entry.head.version)?;
                        report.waiting.push((id.to_owned(), reason.to_owned()));
                        return Ok(forked);
                    }
                    let Some(_leases) = self.write_leases(id, held) else {
                        self.remember_remote(id, &mut state, entry.head.version)?;
                        report
                            .waiting
                            .push((id.to_owned(), "busy, retrying later".to_owned()));
                        return Ok(forked);
                    };
                    let seen = Seen {
                        local: &local,
                        changes: &changes,
                    };
                    if let Some(fork) = self
                        .merge(id, &sid, &mut state, seen, entry, report)
                        .await?
                    {
                        forked = Some(fork);
                    }
                }
                None if changes.local() => {
                    match self.push(id, &sid, &mut state, &local, &changes).await? {
                        Pushed::Done => {
                            report.pushed.push(id.to_owned());
                            return Ok(forked);
                        }
                        Pushed::Conflict(head) => theirs = self.open_head(head)?,
                        Pushed::Locked(lease) => {
                            report.waiting.push((
                                id.to_owned(),
                                format!("open on {}, syncs when it closes there", lease.holder),
                            ));
                            return Ok(forked);
                        }
                    }
                }
                None => {
                    if let Some(known) = &mut state
                        && known.remote_version != known.version
                    {
                        known.remote_version = known.version;
                        known.save(&self.layout, id)?;
                    }
                    return Ok(forked);
                }
            }
        }
        report
            .waiting
            .push((id.to_owned(), "kept changing during the pass".to_owned()));
        Ok(forked)
    }

    fn untouched(&self, id: &str, known: &SessionState) -> std::io::Result<bool> {
        let recorded_meta = known.stamps.get(META).copied();
        let meta = stamp(&self.layout.meta(id))?;
        if meta.is_some() && meta != recorded_meta {
            return Ok(false);
        }
        if meta.is_none() && base_exists(Some(known)) {
            return Ok(false);
        }
        if meta.is_some() {
            let history = stamp(&self.layout.history(id))?;
            if history != known.history.as_ref().map(|history| history.stamp) {
                return Ok(false);
            }
        }
        let artifacts = artifact_files(&self.layout.artifact_dir(id))?;
        let recorded = known
            .stamps
            .iter()
            .filter(|(key, _)| key.starts_with(ARTIFACTS))
            .count();
        if artifacts.len() != recorded {
            return Ok(false);
        }
        Ok(artifacts
            .iter()
            .all(|(key, (_, now))| known.stamps.get(key) == Some(now)))
    }

    async fn fetch_history(
        &self,
        id: &str,
        sid: &str,
        theirs: &HistoryRef,
        from: Option<&HistoryRef>,
    ) -> Result<Download, SyncError> {
        let (first, mut chain, start) = match from {
            Some(base)
                if base.generation == theirs.generation
                    && base.seq <= theirs.seq
                    && base.len <= theirs.len =>
            {
                (base.seq, base.chain.clone(), base.len)
            }
            _ => (0, self.keys.chain_start(id, theirs.generation), 0),
        };
        std::fs::create_dir_all(&self.layout.sessions)?;
        let download = Download {
            start,
            path: temp_beside(&self.layout.history(id), "sync-in")?,
        };
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&download.path)?
        };
        let mut len = start;
        for seq in first..theirs.seq {
            let sealed = self.remote.chunk(sid, theirs.generation, seq).await?;
            let plain = self
                .keys
                .open(&chunk_aad(sid, theirs.generation, seq), &sealed)
                .map_err(|error| SyncError::Tampered(format!("history of {id}: {error}")))?;
            chain = self.keys.chain_next(&chain, &plain);
            len += plain.len() as u64;
            file.write_all(&plain)?;
        }
        if len != theirs.len || chain != theirs.chain {
            return Err(SyncError::Tampered(format!(
                "the history of {id} on the server does not match its head"
            )));
        }
        file.sync_all()?;
        Ok(download)
    }

    fn apply_history(&self, id: &str, download: &Download) -> std::io::Result<()> {
        let target = self.layout.history(id);
        if download.start == 0 {
            std::fs::rename(&download.path, &target)
        } else {
            append(&target, download.start, &download.path)
        }
    }

    async fn fetch_blob(&self, id: &str, sid: &str, file: &FileRef) -> Result<Vec<u8>, SyncError> {
        let sealed = self.remote.blob(sid, &file.blob).await?;
        let plain = self
            .keys
            .open(&blob_aad(sid, &file.blob), &sealed)
            .map_err(|error| SyncError::Tampered(format!("a file of {id}: {error}")))?;
        if self.keys.blob_id(&plain) != file.blob {
            return Err(SyncError::Tampered(format!(
                "a file of {id} does not match its name"
            )));
        }
        Ok(plain)
    }

    fn fork(&self, id: &str, local: &Local) -> Result<String, SyncError> {
        let digest = format!(
            "{}:{:?}:{:?}",
            local.meta.as_ref().map_or("", |meta| meta.blob.as_str()),
            local.history,
            local.history_stamp.map(|stamp| (stamp.ino, stamp.len)),
        );
        let fork = self.keys.fork_id(id, &self.machine, &digest);
        if self.layout.meta(&fork).exists() {
            return Ok(fork);
        }
        if self.layout.history(id).exists() {
            copy_durably(&self.layout.history(id), &self.layout.history(&fork))?;
        }
        for (key, (path, _)) in artifact_files(&self.layout.artifact_dir(id))? {
            if let Some(target) = self.layout.file(&fork, &key) {
                copy_durably(&path, &target)?;
            }
        }
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.layout.meta(id))?)?;
        if let Some(fields) = meta
            .get_mut("meta")
            .and_then(serde_json::Value::as_object_mut)
        {
            let title = fields
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("untitled")
                .to_owned();
            fields.insert("id".into(), fork.clone().into());
            fields.insert(
                "title".into(),
                format!("{title} (diverged on {})", self.machine).into(),
            );
            fields.remove("parent");
        }
        write_file(&self.layout.meta(&fork), &serde_json::to_vec_pretty(&meta)?)?;
        Ok(fork)
    }

    async fn core_matches(
        &self,
        id: &str,
        sid: &str,
        state: Option<&SessionState>,
        local: &Local,
        theirs: &Manifest,
    ) -> Result<(bool, Option<Download>), SyncError> {
        if local.meta.as_ref().map(|meta| &meta.blob)
            != theirs.files.get(META).map(|file| &file.blob)
        {
            return Ok((false, None));
        }
        let base = state
            .and_then(|state| state.manifest.as_ref())
            .and_then(|manifest| manifest.history.as_ref());
        let Some(remote) = &theirs.history else {
            return Ok((local.history == HistoryView::Absent, None));
        };
        if local.history_len(base) != Some(remote.len) {
            return Ok((false, None));
        }
        let from = base.filter(|_| local.prefix_intact());
        let download = self.fetch_history(id, sid, remote, from).await?;
        let equal = same_content(&self.layout.history(id), download.start, &download.path)?;
        Ok((equal, Some(download)))
    }

    async fn merge(
        &self,
        id: &str,
        sid: &str,
        state: &mut Option<SessionState>,
        seen: Seen<'_>,
        entry: RemoteEntry,
        report: &mut Report,
    ) -> Result<Option<String>, SyncError> {
        let Seen { local, changes } = seen;
        let theirs = &entry.manifest;
        let exists = !theirs.deleted;
        let mut fork = None;
        let mut take_core = changes.core_remote;
        let mut matched = None;
        if changes.core_local && changes.core_remote {
            match (local.exists(), exists) {
                (true, false) => take_core = false,
                (false, _) => {}
                (true, true) => {
                    let (equal, download) = self
                        .core_matches(id, sid, state.as_ref(), local, theirs)
                        .await?;
                    if !equal {
                        let copy = self.fork(id, local)?;
                        report.forked.push((id.to_owned(), copy.clone()));
                        fork = Some(copy);
                        if let Some(download) = &download {
                            self.apply_history(id, download)?;
                        }
                    }
                    matched = download.map(|_| ());
                }
            }
        }
        let base = state
            .as_ref()
            .and_then(|state| state.manifest.as_ref())
            .and_then(|manifest| manifest.history.as_ref())
            .cloned();
        let mut wrote_history = matched.is_some();
        if take_core {
            if exists {
                if matched.is_none() {
                    match &theirs.history {
                        Some(history) => {
                            let from = base.as_ref().filter(|_| local.prefix_intact());
                            let download = self.fetch_history(id, sid, history, from).await?;
                            self.apply_history(id, &download)?;
                        }
                        None => remove(&self.layout.history(id))?,
                    }
                    wrote_history = true;
                }
            } else {
                remove(&self.layout.meta(id))?;
                remove(&self.layout.history(id))?;
                report.deleted.push(id.to_owned());
            }
        }
        let mut stamps = state
            .as_ref()
            .map(|state| state.stamps.clone())
            .unwrap_or_default();
        let mut skipped = state
            .as_ref()
            .map(|state| state.skipped.clone())
            .unwrap_or_default();
        let artifact_keys: BTreeSet<String> = if fork.is_some() {
            theirs
                .files
                .keys()
                .chain(local.files.keys())
                .filter(|key| key.starts_with(ARTIFACTS))
                .cloned()
                .collect()
        } else {
            changes.files_remote.clone()
        };
        for key in artifact_keys {
            let Some(path) = self.layout.file(id, &key) else {
                continue;
            };
            let target = theirs.files.get(&key);
            let ours = local.files.get(&key);
            if target.map(|file| &file.blob) == ours.map(|entry| &entry.blob) {
                skipped.remove(&key);
                if let Some(entry) = ours {
                    stamps.insert(key, entry.stamp);
                }
                continue;
            }
            if target.is_some_and(|file| !self.limits.allows(&key, file.len)) {
                let previous = state.as_ref().and_then(|state| state.file(&key)).cloned();
                skipped.insert(key, previous);
                continue;
            }
            let recorded = state
                .as_ref()
                .and_then(|state| state.stamps.get(&key))
                .copied();
            let dirty = changes.files_local.contains(&key)
                || (local.held_back.contains(&key)
                    && stamp(&path)?.is_some()
                    && stamp(&path)? != recorded);
            if fork.is_none() && dirty && stamp(&path)?.is_some() {
                let sidecar = path.with_file_name(format!(
                    "{}.sync-conflict-{}-{}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    self.machine,
                    now_ms()
                ));
                std::fs::rename(&path, &sidecar)?;
                report.sidecars.push((id.to_owned(), key.clone()));
            }
            skipped.remove(&key);
            stamps.remove(&key);
            match target {
                Some(file) => {
                    let bytes = self.fetch_blob(id, sid, file).await?;
                    write_file(&path, &bytes)?;
                    if let Some(now) = stamp(&path)? {
                        stamps.insert(key, now);
                    }
                }
                None => remove(&path)?,
            }
        }
        if take_core && exists {
            match theirs.files.get(META) {
                Some(file) if local.meta.as_ref().map(|meta| &meta.blob) != Some(&file.blob) => {
                    let bytes = self.fetch_blob(id, sid, file).await?;
                    write_file(&self.layout.meta(id), &bytes)?;
                }
                Some(_) => {}
                None => remove(&self.layout.meta(id))?,
            }
            stamps.remove(META);
            if let Some(now) = stamp(&self.layout.meta(id))? {
                stamps.insert(META.to_owned(), now);
            }
        }
        let history = if wrote_history {
            match (&theirs.history, stamp(&self.layout.history(id))?) {
                (Some(reference), Some(now)) => Some(HistoryStamp {
                    stamp: now,
                    tail: tail_hash(&self.layout.history(id), reference.len)?,
                }),
                _ => None,
            }
        } else {
            state.as_ref().and_then(|state| state.history.clone())
        };
        let merged = SessionState {
            version: entry.head.version,
            remote_version: entry.head.version,
            generation: entry.head.generation,
            manifest: Some(theirs.clone()),
            skipped,
            history,
            stamps,
        };
        merged.save(&self.layout, id)?;
        *state = Some(merged);
        let pulled = (take_core && exists) || !changes.files_remote.is_empty();
        if pulled && !report.pulled.iter().any(|seen| seen == id) {
            report.pulled.push(id.to_owned());
        }
        Ok(fork)
    }

    async fn push(
        &self,
        id: &str,
        sid: &str,
        state: &mut Option<SessionState>,
        local: &Local,
        changes: &Delta,
    ) -> Result<Pushed, SyncError> {
        let previous = state.clone().unwrap_or_default();
        let expect = previous.version;
        let base = previous.manifest.clone().unwrap_or_default();
        let mut manifest = base.clone();
        manifest.session = id.to_owned();
        manifest.writer = self.machine.clone();
        manifest.written_at = now_ms();
        let upload = upload_token()?;
        let mut pieces: Vec<(u64, u64)> = Vec::new();
        let mut history = base.history.clone();
        let mut generation = previous.generation;
        if changes.core_local {
            manifest.deleted = !local.exists();
            if let Some(meta) = &local.meta {
                manifest.files.insert(META.to_owned(), meta.file());
                let info = mush_session::load_meta(&self.layout.meta(id)).ok();
                manifest.parent = info
                    .as_ref()
                    .and_then(|meta| meta.parent.as_ref())
                    .map(ToString::to_string);
                manifest.title = info.and_then(|meta| meta.title);
                let (start, end) = match (local.history, &base.history) {
                    (HistoryView::Absent, _) => {
                        history = None;
                        (0, 0)
                    }
                    (HistoryView::Same, _) => (0, 0),
                    (HistoryView::Appended { to }, Some(reference)) => (reference.len, to),
                    (HistoryView::Appended { to } | HistoryView::Rewritten { len: to }, _) => {
                        generation = if expect > 0 { generation + 1 } else { 0 };
                        history = Some(HistoryRef {
                            generation,
                            seq: 0,
                            len: 0,
                            chain: self.keys.chain_start(id, generation),
                        });
                        (0, to)
                    }
                };
                let mut offset = start;
                while offset < end {
                    let next = (offset + self.limits.chunk).min(end);
                    pieces.push((offset, next));
                    offset = next;
                }
            }
        }
        for key in &changes.files_local {
            match local.files.get(key) {
                Some(entry) => manifest.files.insert(key.clone(), entry.file()),
                None => manifest.files.remove(key),
            };
        }
        if !pieces.is_empty() {
            let path = self.layout.history(id);
            let opened = std::fs::File::open(&path)?;
            let scanned = local.history_stamp.map(|stamp| stamp.ino);
            if Some(std::os::unix::fs::MetadataExt::ino(&opened.metadata()?)) != scanned {
                return Err(SyncError::Io(std::io::Error::other(
                    "the history was replaced while syncing, retrying next pass",
                )));
            }
            let Some(reference) = history.as_mut() else {
                return Err(SyncError::Io(std::io::Error::other(
                    "history state is missing",
                )));
            };
            for (start, end) in pieces {
                let bytes = read_range(&opened, start, end)?;
                reference.chain = self.keys.chain_next(&reference.chain, &bytes);
                let sealed = self
                    .keys
                    .seal(&chunk_aad(sid, reference.generation, reference.seq), &bytes)?;
                self.remote
                    .stage(sid, &upload, reference.generation, reference.seq, sealed)
                    .await?;
                reference.seq += 1;
                reference.len = end;
            }
        }
        manifest.history = history;
        let known: HashSet<&String> = base.files.values().map(|file| &file.blob).collect();
        for (key, file) in &manifest.files {
            if known.contains(&file.blob) {
                continue;
            }
            let Some(path) = self.layout.file(id, key) else {
                continue;
            };
            let bytes = std::fs::read(&path)?;
            if self.keys.blob_id(&bytes) != file.blob {
                return Err(SyncError::Io(std::io::Error::other(format!(
                    "{key} changed while syncing, retrying next pass"
                ))));
            }
            let sealed = self.keys.seal(&blob_aad(sid, &file.blob), &bytes)?;
            self.remote.store_blob(sid, &file.blob, sealed).await?;
        }
        let blobs: BTreeSet<String> = manifest
            .files
            .values()
            .map(|file| file.blob.clone())
            .collect();
        let sealed = self.keys.seal(
            &manifest_aad(sid, expect + 1),
            &serde_json::to_vec(&manifest)?,
        )?;
        let commit = Commit {
            expect,
            holder: self.machine.clone(),
            upload,
            generation: manifest
                .history
                .as_ref()
                .map_or(generation, |history| history.generation),
            seq: manifest.history.as_ref().map_or(0, |history| history.seq),
            blobs: blobs.into_iter().collect(),
            manifest: STANDARD.encode(sealed),
        };
        let head = match self.remote.commit(sid, &commit).await? {
            Committed::Accepted(head) => head,
            Committed::Conflict(head) => return Ok(Pushed::Conflict(head)),
            Committed::Locked { lease, .. } => return Ok(Pushed::Locked(lease)),
        };
        let mut next = previous;
        next.version = head.version;
        next.remote_version = head.version;
        next.generation = head.generation;
        for key in &changes.files_local {
            next.skipped.remove(key);
            match local.files.get(key) {
                Some(entry) => next.stamps.insert(key.clone(), entry.stamp),
                None => next.stamps.remove(key),
            };
        }
        if changes.core_local {
            match &local.meta {
                Some(meta) => next.stamps.insert(META.to_owned(), meta.stamp),
                None => next.stamps.remove(META),
            };
            next.history = match (&manifest.history, local.history_stamp) {
                (Some(reference), Some(now)) if local.exists() => Some(HistoryStamp {
                    stamp: now,
                    tail: tail_hash(&self.layout.history(id), reference.len)?,
                }),
                _ if local.exists() => None,
                _ => next.history,
            };
        }
        next.manifest = Some(manifest);
        next.save(&self.layout, id)?;
        *state = Some(next);
        Ok(Pushed::Done)
    }
}
