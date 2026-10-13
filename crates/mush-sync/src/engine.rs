use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mush_session::{SessionId, SessionLease};

use crate::blob::{BlobError, Sealer, Unsealer};
use crate::keys::Keys;
use crate::local::{
    Cursor, HistoryStamp, Layout, Limits, SessionState, Stamp, append, artifact_files,
    complete_lines, copy_durably, live_only, prefix_equal, read_range, remove, stamp, tail_hash,
    temp_beside, write_file,
};
use crate::protocol::{
    ARTIFACTS, Commit, FileRef, Head, HistoryRef, Lease, META, Manifest, chunk_aad, manifest_aad,
    now_ms, valid_session,
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
    Changed(Vec<String>),
    Family(String),
    Sessions(Vec<String>),
}

impl Scope {
    fn follows_feed(&self) -> bool {
        matches!(self, Self::All | Self::Changed(_))
    }
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
    pub matched: Vec<String>,
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

#[derive(Debug, Default)]
pub struct Queue {
    pub up: Vec<String>,
    pub down: Vec<String>,
}

#[derive(Debug, Default)]
pub struct LocalSummary {
    pub sessions: usize,
    pub synced: usize,
    pub waiting: usize,
}

#[derive(Debug)]
pub struct OpenElsewhere {
    pub session: String,
    pub title: Option<String>,
    pub holder: String,
    pub left: Duration,
}

#[derive(Debug, Default)]
pub struct Overview {
    pub sessions: usize,
    pub elsewhere: Vec<OpenElsewhere>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Equal,
    Behind,
    Ahead,
    Diverged,
}

fn settle_stamps(known: &mut SessionState, local: &Local) -> bool {
    let mut changed = known.remote_version != known.version || known.held != local.capped;
    known.remote_version = known.version;
    known.held.clone_from(&local.capped);
    let entries = local
        .meta
        .as_ref()
        .map(|meta| (META, meta))
        .into_iter()
        .chain(local.files.iter().map(|(key, entry)| (key.as_str(), entry)));
    for (key, entry) in entries {
        let in_sync = known.file(key).is_some_and(|file| file.blob == entry.blob);
        if in_sync && known.stamps.get(key) != Some(&entry.stamp) {
            known.stamps.insert(key.to_owned(), entry.stamp);
            changed = true;
        }
    }
    changed
}

pub(crate) fn internal(session: &str) -> bool {
    session == crate::harness::HARNESS || session == crate::tool_output::TOOL_OUTPUT
}

pub(crate) fn meta_differs(ours: &[u8], theirs: &[u8]) -> bool {
    let essence = |bytes: &[u8]| {
        let mut value = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
        let fields = value.as_object_mut()?;
        fields.remove("tree");
        if let Some(meta) = fields
            .get_mut("meta")
            .and_then(serde_json::Value::as_object_mut)
        {
            meta.remove("updated_at");
            meta.remove("message_count");
        }
        Some(value)
    };
    match (essence(ours), essence(theirs)) {
        (Some(ours), Some(theirs)) => ours != theirs,
        _ => true,
    }
}

fn updated_at_of(meta: &[u8]) -> u64 {
    serde_json::from_slice::<serde_json::Value>(meta)
        .ok()
        .and_then(|value| value.get("meta")?.get("updated_at")?.as_u64())
        .unwrap_or(0)
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
            modified_ms: None,
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
    capped: BTreeMap<String, Stamp>,
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
        if local.exists() && ours != base_file(key) {
            changes.files_local.insert(key.clone());
        }
        let previous = base.and_then(|base| base.files.get(key));
        if let Some(theirs) = theirs
            && !theirs.deleted
            && theirs.files.get(key) != previous
        {
            changes.files_remote.insert(key.clone());
        }
    }
    changes
}

#[derive(Default)]
struct Feed {
    entries: HashMap<String, RemoteEntry>,
    unreadable: Vec<(Head, String)>,
}

pub(crate) struct Download {
    pub(crate) start: u64,
    pub(crate) path: PathBuf,
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

pub type Progress = Arc<dyn Fn(usize, usize) + Send + Sync>;

const REPORT_EVERY_MS: u64 = 60_000;

pub struct Engine {
    layout: Layout,
    keys: Arc<Keys>,
    remote: Remote,
    machine: String,
    limits: Limits,
    progress: Option<Progress>,
    reported: std::sync::atomic::AtomicU64,
    reported_rev: std::sync::atomic::AtomicU64,
    parent_cache: std::sync::Mutex<HashMap<String, (Stamp, Option<String>)>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("machine", &self.machine)
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

pub(crate) fn upload_token() -> std::io::Result<String> {
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
            progress: None,
            reported: std::sync::atomic::AtomicU64::new(0),
            reported_rev: std::sync::atomic::AtomicU64::new(0),
            parent_cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_progress(mut self, progress: Progress) -> Self {
        self.progress = Some(progress);
        self
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

    pub(crate) fn shared_keys(&self) -> &Arc<Keys> {
        &self.keys
    }

    pub(crate) fn limits(&self) -> &Limits {
        &self.limits
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
        let full = scope.follows_feed() && direction == Direction::Both;
        let result = self.pass_once(scope, direction, held, patience).await;
        if let Ok(report) = &result {
            let forks = report
                .forked
                .iter()
                .map(|(session, fork)| crate::status::fork(session, fork, "diverged"))
                .collect();
            if let Err(error) = crate::status::remember(&self.layout, forks) {
                tracing::warn!(%error, "could not note the forks for review");
            }
        }
        if full {
            self.record_pass(&result).await;
        }
        result
    }

    async fn record_pass(&self, result: &Result<Report, SyncError>) {
        let mut status = crate::status::LocalStatus::load(&self.layout);
        let now = now_ms();
        status.last_attempt_ms = now;
        match result {
            Ok(report) => {
                status.last_ok_ms = Some(now);
                status.offline_since_ms = None;
                status.last_error = report
                    .failed
                    .first()
                    .map(|(session, error)| format!("{session}: {error}"));
                status.waiting.clone_from(&report.waiting);
                status.failed.clone_from(&report.failed);
                status.rev = Cursor::load(&self.layout).map_or(status.rev, |cursor| cursor.rev);
            }
            Err(error) => {
                if error.offline() {
                    status.offline_since_ms.get_or_insert(now);
                }
                status.last_error = Some(error.to_string());
            }
        }
        if let Err(error) = status.save(&self.layout) {
            tracing::warn!(%error, "could not record the sync status");
        }
        if result.is_err() {
            return;
        }
        let last = self.reported.load(std::sync::atomic::Ordering::Relaxed);
        let last_rev = self.reported_rev.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) < REPORT_EVERY_MS
            && last_rev == status.rev
            && status.failed.is_empty()
        {
            return;
        }
        self.reported
            .store(now, std::sync::atomic::Ordering::Relaxed);
        self.reported_rev
            .store(status.rev, std::sync::atomic::Ordering::Relaxed);
        let review = crate::status::review(&self.layout).map_or(0, |review| review.len());
        let report = crate::protocol::MachineReport {
            rev: status.rev,
            sessions: self.layout.local_ids().map_or(0, |ids| ids.len() as u64),
            waiting: status.waiting.len() as u64,
            failed: status.failed.len() as u64,
            review: review as u64,
            sealed_error: status.last_error.as_deref().and_then(|error| {
                self.keys
                    .seal(
                        &crate::protocol::report_aad(&self.machine),
                        error.as_bytes(),
                    )
                    .ok()
                    .map(|sealed| STANDARD.encode(sealed))
            }),
        };
        if let Err(error) = self.remote.report(&self.machine, &report).await {
            tracing::debug!(%error, "the sync server did not take this machine's report");
        }
    }

    async fn pass_once(
        &self,
        scope: Scope,
        direction: Direction,
        held: Option<&SessionLease>,
        patience: Duration,
    ) -> Result<Report, SyncError> {
        if scope == Scope::All {
            let removed = crate::local::sweep_abandoned(&self.layout);
            if removed > 0 {
                tracing::info!(
                    removed,
                    "cleared half written files an interrupted sync left"
                );
            }
        }
        let mut cursor = Cursor::load(&self.layout)?;
        let Feed {
            entries: feed,
            unreadable,
        } = match direction {
            Direction::Both => self.feed(&mut cursor).await?,
            Direction::PushOnly => Feed::default(),
        };
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
        let mut feed = feed;
        let shared = feed.remove(crate::harness::HARNESS);
        let outputs = feed.remove(crate::tool_output::TOOL_OUTPUT);
        let mut ids: Vec<String> = match &scope {
            Scope::All => {
                let mut ids = self.layout.local_ids()?;
                ids.extend(self.layout.state_ids()?);
                ids.extend(feed.keys().cloned());
                ids.into_iter().collect()
            }
            Scope::Changed(extra) => feed.keys().chain(extra).cloned().collect(),
            Scope::Family(root) => self.family(root, &feed)?,
            Scope::Sessions(ids) => ids.clone(),
        };
        ids.retain(|id| id != crate::harness::HARNESS && id != crate::tool_output::TOOL_OUTPUT);
        let mut queue: VecDeque<String> = ids.into();
        let mut seen = HashSet::new();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(progress) = &self.progress {
                progress(seen.len(), seen.len() + queue.len());
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
        match self
            .reconcile_tool_output(outputs, direction, &mut report)
            .await
        {
            Ok(()) => {}
            Err(error) if error.offline() => return Err(error),
            Err(error) => report.failed.push((
                crate::tool_output::TOOL_OUTPUT.to_owned(),
                error.to_string(),
            )),
        }
        if scope.follows_feed() {
            match self.reconcile_harness(shared, direction, &mut report).await {
                Ok(()) => {}
                Err(error) if error.offline() => return Err(error),
                Err(error) => report
                    .failed
                    .push((crate::harness::HARNESS.to_owned(), error.to_string())),
            }
        }
        if scope.follows_feed() && direction == Direction::Both {
            cursor.save(&self.layout)?;
        }
        Ok(report)
    }

    pub fn local_summary(&self) -> std::io::Result<LocalSummary> {
        let mut summary = LocalSummary {
            sessions: self.layout.local_ids()?.len(),
            ..LocalSummary::default()
        };
        for id in self.layout.state_ids()? {
            if internal(&id) {
                continue;
            }
            let Some(state) = SessionState::load(&self.layout, &id)? else {
                continue;
            };
            if base_exists(Some(&state)) {
                summary.synced += 1;
            }
            if state.remote_version > state.version {
                summary.waiting += 1;
            }
        }
        Ok(summary)
    }

    #[must_use]
    pub fn report_error(&self, machine: &crate::protocol::Machine) -> Option<String> {
        let sealed = STANDARD
            .decode(machine.report.sealed_error.as_deref()?)
            .ok()?;
        let plain = self
            .keys
            .open(&crate::protocol::report_aad(&machine.name), &sealed)
            .ok()?;
        String::from_utf8(plain).ok()
    }

    pub fn queue(&self) -> std::io::Result<Queue> {
        let mut queue = Queue::default();
        let local = self.layout.local_ids()?;
        for id in &local {
            match SessionState::load(&self.layout, id)? {
                None => queue.up.push(id.clone()),
                Some(state) => {
                    if !self.untouched(id, &state)? {
                        queue.up.push(id.clone());
                    }
                    if state.remote_version > state.version {
                        queue.down.push(id.clone());
                    }
                }
            }
        }
        for id in self.layout.state_ids()? {
            if local.contains(&id) || internal(&id) {
                continue;
            }
            let Some(state) = SessionState::load(&self.layout, &id)? else {
                continue;
            };
            if base_exists(Some(&state)) {
                queue.up.push(id.clone());
            }
            if state.remote_version > state.version {
                queue.down.push(id);
            }
        }
        Ok(queue)
    }

    pub async fn overview(&self) -> Result<Overview, SyncError> {
        let mut overview = Overview::default();
        let mut since = 0;
        loop {
            let changes = self.remote.changes(since, Duration::ZERO, false).await?;
            for head in changes.heads {
                let entry = self
                    .open_head(head.clone())
                    .ok()
                    .flatten()
                    .filter(|entry| !internal(&entry.manifest.session));
                if entry.as_ref().is_some_and(|entry| !entry.manifest.deleted) {
                    overview.sessions += 1;
                }
                let Some(lease) = head
                    .lease
                    .filter(|lease| lease.active(changes.now) && lease.holder != self.machine)
                else {
                    continue;
                };
                overview.elsewhere.push(OpenElsewhere {
                    session: entry
                        .as_ref()
                        .map_or_else(|| head.sid.clone(), |entry| entry.manifest.session.clone()),
                    title: entry.and_then(|entry| entry.manifest.title),
                    holder: lease.holder,
                    left: Duration::from_millis(lease.expires_at.saturating_sub(changes.now)),
                });
            }
            since = changes.rev;
            if !changes.more {
                break;
            }
        }
        Ok(overview)
    }

    pub async fn open_elsewhere(
        &self,
        timeout: Duration,
    ) -> Result<HashMap<String, String>, SyncError> {
        let leases = self.remote.leases(timeout).await?;
        let mut blinded: HashMap<String, String> = HashMap::new();
        for id in self.layout.local_ids()? {
            blinded.insert(self.keys.sid(&id), id);
        }
        Ok(leases
            .held
            .into_iter()
            .filter(|(_, lease)| lease.holder != self.machine)
            .filter_map(|(sid, lease)| Some((blinded.remove(&sid)?, lease.holder)))
            .collect())
    }

    pub async fn take(&self, id: &str, ttl: Duration) -> Result<Option<String>, SyncError> {
        let request = crate::protocol::LeaseRequest {
            holder: self.machine.clone(),
            ttl_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
            take: true,
        };
        let sid = self.keys.sid(id);
        let previous = self
            .remote
            .leases(Duration::from_secs(10))
            .await?
            .held
            .into_iter()
            .find(|(held, lease)| *held == sid && lease.holder != self.machine)
            .map(|(_, lease)| lease.holder);
        self.remote.claim(&sid, &request).await?;
        Ok(previous)
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
                cursor.save(&self.layout)?;
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

    pub(crate) async fn fetch_entry(&self, sid: &str) -> Result<Option<RemoteEntry>, SyncError> {
        match self.remote.head(sid).await? {
            Some(head) => self.open_head(head),
            None => Ok(None),
        }
    }

    fn parents(&self) -> std::io::Result<HashMap<String, String>> {
        let mut known = self
            .parent_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids = self.layout.local_ids()?;
        known.retain(|id, _| ids.contains(id));
        let mut parents = HashMap::new();
        for id in ids {
            let path = self.layout.meta(&id);
            let Some(now) = stamp(&path)? else {
                known.remove(&id);
                continue;
            };
            let parent = match known.get(&id) {
                Some((seen, parent)) if *seen == now => parent.clone(),
                _ => {
                    let parent = mush_session::load_meta(&path)
                        .ok()
                        .and_then(|meta| meta.parent)
                        .map(|parent| parent.to_string());
                    known.insert(id.clone(), (now, parent.clone()));
                    parent
                }
            };
            if let Some(parent) = parent {
                parents.insert(id, parent);
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
            _ => self.keys.blob_id_of(path)?,
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
        let mut capped = BTreeMap::new();
        for (key, (path, now)) in artifact_files(&self.layout.artifact_dir(id))? {
            if self.capped(&key, &now) {
                held_back.insert(key.clone());
                capped.insert(key, now);
                continue;
            }
            if live && live_only(&key) {
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
            capped,
        })
    }

    fn capped(&self, key: &str, now: &Stamp) -> bool {
        !self.limits.allows(key, now.len) || !self.limits.current(key, now)
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
                        && settle_stamps(known, &local)
                    {
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
        let mut recorded: BTreeMap<&String, &Stamp> = known
            .stamps
            .iter()
            .filter(|(key, _)| key.starts_with(ARTIFACTS))
            .collect();
        for (key, seen) in &known.held {
            if !self.capped(key, seen) {
                return Ok(false);
            }
            recorded.insert(key, seen);
        }
        if artifacts.len() != recorded.len() {
            return Ok(false);
        }
        Ok(artifacts
            .iter()
            .all(|(key, (_, now))| recorded.get(key) == Some(&now)))
    }

    pub(crate) async fn fetch_history(
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

    pub(crate) async fn fetch_blob(
        &self,
        id: &str,
        sid: &str,
        file: &FileRef,
    ) -> Result<Vec<u8>, SyncError> {
        let mut plain = Vec::with_capacity(usize::try_from(file.len).unwrap_or(0));
        self.receive_blob(id, sid, file, &mut plain).await?;
        Ok(plain)
    }

    async fn receive_blob(
        &self,
        id: &str,
        sid: &str,
        file: &FileRef,
        sink: &mut (dyn Write + Send),
    ) -> Result<(), SyncError> {
        use futures::StreamExt;
        let broken = |error: BlobError| match error {
            BlobError::Broken(why) => SyncError::Tampered(format!("a file of {id}: {why}")),
            BlobError::Io(error) => SyncError::Io(error),
        };
        let mut stream = self.remote.blob_stream(sid, &file.blob).await?;
        let mut unsealer = Unsealer::new(&self.keys, sid, &file.blob, file.len);
        while let Some(piece) = stream.next().await {
            let piece = piece?;
            unsealer.feed(&piece, sink).map_err(broken)?;
        }
        unsealer.finish(sink).map_err(broken)
    }

    pub(crate) async fn fetch_blob_into(
        &self,
        id: &str,
        sid: &str,
        file: &FileRef,
        target: &Path,
    ) -> Result<(), SyncError> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = temp_beside(target, "sync-in")?;
        let written = async {
            let handle = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&partial)?
            };
            let mut writer = std::io::BufWriter::with_capacity(256 << 10, handle);
            self.receive_blob(id, sid, file, &mut writer).await?;
            let handle = writer
                .into_inner()
                .map_err(std::io::IntoInnerError::into_error)?;
            handle.sync_all()?;
            std::fs::rename(&partial, target)?;
            Ok::<(), SyncError>(())
        }
        .await;
        if written.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        written
    }

    pub(crate) async fn upload_file(
        &self,
        sid: &str,
        blob: &str,
        path: &Path,
        len: u64,
    ) -> Result<(), SyncError> {
        let file = std::fs::File::open(path)?;
        let (sender, receiver) = tokio::sync::mpsc::channel::<std::io::Result<Vec<u8>>>(2);
        let keys = Arc::clone(&self.keys);
        let (sid_owned, blob_owned) = (sid.to_owned(), blob.to_owned());
        let producer = tokio::task::spawn_blocking(move || {
            let reader = std::io::BufReader::with_capacity(256 << 10, file);
            for frame in Sealer::new(keys, &sid_owned, &blob_owned, reader, len) {
                let failure = frame.as_ref().err().map(std::io::Error::kind);
                let message = frame.as_ref().err().map(ToString::to_string);
                if sender.blocking_send(frame).is_err() {
                    return None;
                }
                if let (Some(kind), Some(message)) = (failure, message) {
                    return Some(std::io::Error::new(kind, message));
                }
            }
            None
        });
        let frames = futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|frame| (frame, receiver))
        });
        let sent = self
            .remote
            .store_blob_body(sid, blob, reqwest::Body::wrap_stream(frames))
            .await;
        let failed = producer.await.ok().flatten();
        match (sent, failed) {
            (_, Some(error)) => Err(SyncError::Io(error)),
            (Err(error), None) => Err(error.into()),
            (Ok(()), None) => Ok(()),
        }
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

    async fn order(
        &self,
        id: &str,
        sid: &str,
        state: Option<&SessionState>,
        local: &Local,
        theirs: &Manifest,
    ) -> Result<(Order, Option<Download>), SyncError> {
        let base = state
            .and_then(|state| state.manifest.as_ref())
            .and_then(|manifest| manifest.history.as_ref());
        let Some(remote) = &theirs.history else {
            let order = if local.history == HistoryView::Absent {
                Order::Equal
            } else {
                Order::Diverged
            };
            return Ok((order, None));
        };
        let Some(ours) = local.history_len(base) else {
            return Ok((Order::Diverged, None));
        };
        let from = base.filter(|_| local.prefix_intact());
        let download = self.fetch_history(id, sid, remote, from).await?;
        let common = ours.min(remote.len).saturating_sub(download.start);
        let agrees = prefix_equal(
            &self.layout.history(id),
            download.start,
            &download.path,
            common,
        )?;
        let order = match (agrees, ours.cmp(&remote.len)) {
            (false, _) => Order::Diverged,
            (true, std::cmp::Ordering::Equal) => Order::Equal,
            (true, std::cmp::Ordering::Less) => Order::Behind,
            (true, std::cmp::Ordering::Greater) => Order::Ahead,
        };
        Ok((order, Some(download)))
    }

    async fn merge_artifact(
        &self,
        id: &str,
        sid: &str,
        key: &str,
        path: &Path,
        theirs: &FileRef,
    ) -> Result<Option<Vec<u8>>, SyncError> {
        let lines = key.ends_with(".jsonl");
        if !lines && !key.ends_with(mush_session::harness::STATE_FILE) {
            return Ok(None);
        }
        let Ok(ours) = std::fs::read(path) else {
            return Ok(None);
        };
        let remote = self.fetch_blob(id, sid, theirs).await?;
        Ok(if lines {
            Some(crate::harness::merge_lines(&ours, &remote))
        } else {
            crate::harness::merge_bytes(&ours, &remote, None)
        })
    }

    fn keep_beside(&self, id: &str, who: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self
            .layout
            .sessions
            .join(format!("{id}.json.sync-conflict-{who}-{}", now_ms()));
        write_file(&path, bytes)
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
        let mut keep_ours = false;
        let mut settled = false;
        let mut download = None;
        if changes.core_local && changes.core_remote {
            match (local.exists(), exists) {
                (true, false) => take_core = false,
                (false, _) => {}
                (true, true) => {
                    let (order, fetched) =
                        self.order(id, sid, state.as_ref(), local, theirs).await?;
                    download = fetched;
                    let ours = local.meta.as_ref().map(|meta| meta.blob.clone());
                    let remote_meta = theirs.files.get(META);
                    let base_meta = state
                        .as_ref()
                        .and_then(|state| state.file(META))
                        .map(|file| file.blob.clone());
                    let metas_equal = ours.as_ref() == remote_meta.map(|file| &file.blob);
                    let (local_bytes, remote_bytes) = if metas_equal {
                        (Vec::new(), Vec::new())
                    } else {
                        let bytes = match remote_meta {
                            Some(file) => self.fetch_blob(id, sid, file).await?,
                            None => Vec::new(),
                        };
                        (std::fs::read(self.layout.meta(id))?, bytes)
                    };
                    let meaningful = !metas_equal && meta_differs(&local_bytes, &remote_bytes);
                    let ours_moved = ours != base_meta;
                    let theirs_moved = remote_meta.map(|file| &file.blob) != base_meta.as_ref();
                    let mut keep_theirs_beside = false;
                    let mut keep_ours_beside = false;
                    match order {
                        Order::Diverged => {
                            let copy = self.fork(id, local)?;
                            report.forked.push((id.to_owned(), copy.clone()));
                            fork = Some(copy);
                        }
                        Order::Behind => keep_ours_beside = meaningful && ours_moved,
                        Order::Ahead => {
                            take_core = false;
                            keep_ours = true;
                            keep_theirs_beside = meaningful && theirs_moved;
                        }
                        Order::Equal => {
                            settled = true;
                            if !metas_equal
                                && updated_at_of(&local_bytes) > updated_at_of(&remote_bytes)
                            {
                                take_core = false;
                                keep_ours = true;
                                keep_theirs_beside = meaningful;
                            } else {
                                keep_ours_beside = meaningful;
                            }
                        }
                    }
                    if keep_ours_beside {
                        let bytes = std::fs::read(self.layout.meta(id))?;
                        self.keep_beside(id, &self.machine, &bytes)?;
                        report.sidecars.push((id.to_owned(), META.to_owned()));
                    }
                    if keep_theirs_beside && !remote_bytes.is_empty() {
                        let who = if theirs.writer.is_empty() {
                            "remote"
                        } else {
                            theirs.writer.as_str()
                        };
                        self.keep_beside(id, who, &remote_bytes)?;
                        report.sidecars.push((id.to_owned(), META.to_owned()));
                    }
                }
            }
        }
        let base = state
            .as_ref()
            .and_then(|state| state.manifest.as_ref())
            .and_then(|manifest| manifest.history.as_ref())
            .cloned();
        let mut wrote_history = settled;
        let mut changed_here = false;
        if take_core {
            if exists {
                if !settled {
                    match &theirs.history {
                        Some(history) => {
                            let fetched = match download.take() {
                                Some(fetched) => fetched,
                                None => {
                                    let from = base.as_ref().filter(|_| local.prefix_intact());
                                    self.fetch_history(id, sid, history, from).await?
                                }
                            };
                            self.apply_history(id, &fetched)?;
                        }
                        None => remove(&self.layout.history(id))?,
                    }
                    wrote_history = true;
                    changed_here = true;
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
            if fork.is_none()
                && dirty
                && let Some(file) = target
                && let Some(merged) = self.merge_artifact(id, sid, &key, &path, file).await?
            {
                write_file(&path, &merged)?;
                changed_here = true;
                skipped.remove(&key);
                stamps.remove(&key);
                continue;
            }
            if fork.is_none() && dirty && stamp(&path)?.is_some() {
                let sidecar = path.with_file_name(format!(
                    "{}.sync-conflict-{}-{}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    self.machine,
                    now_ms()
                ));
                std::fs::rename(&path, &sidecar)?;
                crate::status::remember(
                    &self.layout,
                    vec![crate::status::sidecar(id, sidecar.clone())],
                )?;
                report.sidecars.push((id.to_owned(), key.clone()));
            }
            skipped.remove(&key);
            stamps.remove(&key);
            changed_here = true;
            match target {
                Some(file) => {
                    self.fetch_blob_into(id, sid, file, &path).await?;
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
                    self.fetch_blob_into(id, sid, file, &self.layout.meta(id))
                        .await?;
                    changed_here = true;
                }
                Some(_) => {}
                None => {
                    remove(&self.layout.meta(id))?;
                    changed_here = true;
                }
            }
            stamps.remove(META);
            if let Some(now) = stamp(&self.layout.meta(id))? {
                stamps.insert(META.to_owned(), now);
            }
        }
        if keep_ours {
            stamps.remove(META);
        }
        let history = if wrote_history || keep_ours {
            match (&theirs.history, stamp(&self.layout.history(id))?) {
                (Some(reference), Some(now)) => Some(HistoryStamp {
                    stamp: if keep_ours {
                        Stamp {
                            len: reference.len,
                            ..now
                        }
                    } else {
                        now
                    },
                    tail: tail_hash(&self.layout.history(id), reference.len)?,
                }),
                _ => None,
            }
        } else if !exists && local.exists() {
            None
        } else {
            state.as_ref().and_then(|state| state.history.clone())
        };
        let mut known = theirs.clone();
        if entry.head.pruned_at.is_some() {
            known.files.clear();
            known.history = None;
        }
        let merged = SessionState {
            version: entry.head.version,
            remote_version: entry.head.version,
            generation: entry.head.generation + u64::from(entry.head.pruned_at.is_some()),
            manifest: Some(known),
            skipped,
            history,
            stamps,
            held: local.capped.clone(),
        };
        merged.save(&self.layout, id)?;
        *state = Some(merged);
        if changed_here {
            if !report.pulled.iter().any(|seen| seen == id) {
                report.pulled.push(id.to_owned());
            }
        } else if take_core && exists && local.exists() && fork.is_none() {
            report.matched.push(id.to_owned());
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
                        generation = match (&base.history, expect) {
                            (_, 0) => 0,
                            (Some(_), _) => generation + 1,
                            (None, _) => generation,
                        };
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
            self.upload_file(sid, &file.blob, &path, file.len)
                .await
                .map_err(|error| match error {
                    SyncError::Io(error) => {
                        SyncError::Io(std::io::Error::other(format!("{key}: {error}")))
                    }
                    other => other,
                })?;
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
            deleted: manifest.deleted,
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
        next.held.clone_from(&local.capped);
        next.save(&self.layout, id)?;
        *state = Some(next);
        Ok(Pushed::Done)
    }
}
