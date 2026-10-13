use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as Url, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::protocol::{
    Archived, Blocked, Changes, Commit, Head, Lease, LeaseGrant, LeaseRequest, Leases, Machine,
    MachineReport, Machines, PruneReport, PruneRequest, PrunedGeneration, PrunedSession, Refusal,
    ReleaseRequest, now_ms, valid_blob, valid_holder, valid_sid, valid_upload,
};

const MAX_UPLOAD: u64 = 512 << 20;
const MAX_WAIT_SECS: u64 = 300;
const PAGE: usize = 500;
const UNREFERENCED_GRACE: Duration = Duration::from_secs(60 * 60);
const STAGING_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
const PRUNE_FIRST: Duration = Duration::from_secs(10 * 60);
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub root: PathBuf,
    pub token_hash: String,
    pub retention: Option<Duration>,
}

struct Shared {
    root: PathBuf,
    retention: Option<Duration>,
    token_hash: blake3::Hash,
    id: String,
    inner: tokio::sync::Mutex<Inner>,
    rev: tokio::sync::watch::Sender<u64>,
}

struct Inner {
    heads: HashMap<String, Head>,
    rev: u64,
}

enum Failure {
    Refused(StatusCode, Box<Refusal>),
    Missing,
    Invalid(String),
    Io(std::io::Error),
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::NotFound {
            Self::Missing
        } else {
            Self::Io(error)
        }
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self::Io(error.into())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Refused(status, refusal) => (status, Json(*refusal)).into_response(),
            Self::Missing => StatusCode::NOT_FOUND.into_response(),
            Self::Invalid(reason) => {
                (StatusCode::BAD_REQUEST, Json(Refusal::Invalid { reason })).into_response()
            }
            Self::Io(error) => {
                tracing::error!(%error, "mush sync server storage failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

fn invalid(reason: &str) -> Failure {
    Failure::Invalid(reason.to_owned())
}

pub async fn serve(listener: tokio::net::TcpListener, config: ServerConfig) -> std::io::Result<()> {
    use axum::serve::ListenerExt;
    let (app, shared) = app(config)?;
    let pruning = tokio::spawn(prune_daily(shared));
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "could not turn off nagle");
        }
    });
    let served = axum::serve(listener, app).await;
    pruning.abort();
    served
}

async fn prune_daily(shared: Arc<Shared>) {
    if shared.retention.is_none() {
        return;
    }
    tokio::time::sleep(PRUNE_FIRST).await;
    loop {
        match shared.prune(false).await {
            Ok(report) => tracing::info!(
                generations = report.generations.len(),
                sessions = report.sessions.len(),
                blocked = report.blocked.len(),
                freed = report.freed + report.swept_bytes,
                "pruned old history"
            ),
            Err(error) => tracing::error!(%error, "pruning old history failed"),
        }
        tokio::time::sleep(PRUNE_EVERY).await;
    }
}

pub fn router(config: ServerConfig) -> std::io::Result<Router> {
    Ok(app(config)?.0)
}

fn app(config: ServerConfig) -> std::io::Result<(Router, Arc<Shared>)> {
    let shared = Arc::new(Shared::open(config)?);
    let router = Router::new()
        .route("/v1/changes", get(changes))
        .route("/v1/leases", get(leases))
        .route("/v1/machines", get(machines))
        .route("/v1/machines/{name}", post(report_machine))
        .route("/v1/prune", post(prune))
        .route("/v1/sessions/{sid}", get(head))
        .route("/v1/sessions/{sid}/archive", get(archive))
        .route("/v1/sessions/{sid}/chunks/{generation}/{seq}", get(chunk))
        .route(
            "/v1/sessions/{sid}/uploads/{upload}/{generation}/{seq}",
            put(stage),
        )
        .route("/v1/sessions/{sid}/blobs/{blob}", get(blob).put(store_blob))
        .route("/v1/sessions/{sid}/commit", post(commit))
        .route("/v1/sessions/{sid}/lease", post(lease))
        .route("/v1/sessions/{sid}/release", post(release))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&shared),
            authorise,
        ))
        .with_state(Arc::clone(&shared));
    Ok((router, shared))
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

impl Shared {
    fn open(config: ServerConfig) -> std::io::Result<Self> {
        let token_hash = blake3::Hash::from_hex(config.token_hash.trim())
            .map_err(|_| std::io::Error::other("the token hash is not 64 hex characters"))?;
        private_dir(&config.root.join("sessions"))?;
        let identity = config.root.join("server-id");
        let id = match std::fs::read_to_string(&identity) {
            Ok(id) => id.trim().to_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut bytes = [0u8; 16];
                getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
                let id = blake3::hash(&bytes).to_hex()[..32].to_owned();
                mush_ai::private_io::write_private(&identity, &id)?;
                id
            }
            Err(error) => return Err(error),
        };
        let mut heads = HashMap::new();
        for entry in std::fs::read_dir(config.root.join("sessions"))? {
            let entry = entry?;
            let sid = entry.file_name().to_string_lossy().into_owned();
            if !valid_sid(&sid) {
                continue;
            }
            sweep_staging(&entry.path());
            match std::fs::read(entry.path().join("head.json")) {
                Ok(bytes) => {
                    let head: Head = serde_json::from_slice(&bytes)?;
                    heads.insert(sid, head);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let rev = heads.values().map(|head| head.rev).max().unwrap_or(0);
        let (sender, _) = tokio::sync::watch::channel(rev);
        Ok(Self {
            root: config.root,
            retention: config.retention,
            token_hash,
            id,
            inner: tokio::sync::Mutex::new(Inner { heads, rev }),
            rev: sender,
        })
    }

    fn session(&self, sid: &str) -> PathBuf {
        self.root.join("sessions").join(sid)
    }

    fn persist(&self, head: &Head) -> std::io::Result<()> {
        let dir = self.session(&head.sid);
        private_dir(&dir)?;
        mush_ai::private_io::write_private(&dir.join("head.json"), serde_json::to_vec(head)?)
    }

    fn archive_file(&self, sid: &str, version: u64) -> PathBuf {
        self.session(sid)
            .join("archive")
            .join(format!("{version}.json"))
    }

    fn archived(&self, sid: &str) -> std::io::Result<Vec<Archived>> {
        let entries = match std::fs::read_dir(self.session(sid).join("archive")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut archived = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
                && let Ok(found) = serde_json::from_slice::<Archived>(&std::fs::read(&path)?)
            {
                archived.push(found);
            }
        }
        archived.sort_by_key(|entry| entry.head.version);
        Ok(archived)
    }

    fn kept_blobs(&self, head: &Head, archived: &[Archived]) -> Vec<String> {
        let mut keep: BTreeSet<String> = head.blobs.iter().cloned().collect();
        for entry in archived {
            keep.extend(entry.head.blobs.iter().cloned());
        }
        keep.into_iter().collect()
    }

    fn machines_dir(&self) -> PathBuf {
        self.root.join("machines")
    }

    fn machines(&self) -> std::io::Result<Vec<Machine>> {
        let entries = match std::fs::read_dir(self.machines_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut machines = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
                && let Ok(machine) = serde_json::from_slice::<Machine>(&std::fs::read(&path)?)
            {
                machines.push(machine);
            }
        }
        machines.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(machines)
    }

    async fn prune(&self, dry_run: bool) -> std::io::Result<PruneReport> {
        let now = now_ms();
        let mut report = PruneReport {
            dry_run,
            now,
            ..PruneReport::default()
        };
        let Some(retention) = self.retention else {
            return Ok(report);
        };
        let keep_ms = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
        report.keep_ms = keep_ms;
        let horizon = now.saturating_sub(keep_ms.saturating_mul(3));
        let (live, stale): (Vec<Machine>, Vec<Machine>) = self
            .machines()?
            .into_iter()
            .partition(|machine| machine.seen_at >= horizon);
        report.stale_machines = stale.into_iter().map(|machine| machine.name).collect();
        let behind = |rev: u64, writer: Option<&String>| {
            live.iter()
                .filter(|machine| Some(&machine.name) != writer && machine.report.rev < rev)
                .min_by_key(|machine| machine.report.rev)
                .map(|machine| machine.name.clone())
        };
        let mut inner = self.inner.lock().await;
        let sids: Vec<String> = inner.heads.keys().cloned().collect();
        for sid in sids {
            let Some(mut head) = inner.heads.get(&sid).cloned() else {
                continue;
            };
            let dir = self.session(&sid);
            let open_on = head
                .lease
                .as_ref()
                .filter(|lease| lease.active(now))
                .map(|lease| lease.holder.clone());
            let mut changed = false;
            let mut blocked = |what: &str, reason: String| {
                report.blocked.push(Blocked {
                    sid: sid.clone(),
                    what: what.to_owned(),
                    reason,
                });
            };
            if head.deleted
                && head.pruned_at.is_none()
                && let Some(deleted_at) = head.deleted_at
                && deleted_at.saturating_add(keep_ms) <= now
            {
                if let Some(holder) = &open_on {
                    blocked("deleted session", format!("open on {holder}"));
                } else if let Some(machine) = behind(head.rev, head.writer.as_ref()) {
                    blocked(
                        "deleted session",
                        format!("{machine} has not synced since it was deleted"),
                    );
                } else {
                    let parts = ["chunks", "blobs", "archive", "staging"];
                    let bytes = parts.iter().map(|part| dir_size(&dir.join(part))).sum();
                    report.sessions.push(PrunedSession {
                        sid: sid.clone(),
                        deleted_at,
                        bytes,
                    });
                    report.freed += bytes;
                    if !dry_run {
                        for part in parts {
                            remove_tree(&dir.join(part))?;
                        }
                        head.pruned_at = Some(now);
                        head.blobs.clear();
                        head.archived.clear();
                        self.persist(&head)?;
                        inner.heads.insert(sid.clone(), head);
                    }
                    continue;
                }
            }
            let archived = self.archived(&sid)?;
            let mut kept = Vec::new();
            let mut dropped = Vec::new();
            for entry in archived {
                if entry.archived_at.saturating_add(keep_ms) > now {
                    kept.push(entry);
                } else if let Some(holder) = &open_on {
                    blocked(
                        &format!("generation {}", entry.head.generation),
                        format!("open on {holder}"),
                    );
                    kept.push(entry);
                } else if let Some(machine) =
                    behind(entry.superseded_rev, entry.superseded_by.as_ref())
                {
                    blocked(
                        &format!("generation {}", entry.head.generation),
                        format!("{machine} has not synced since it was replaced"),
                    );
                    kept.push(entry);
                } else {
                    dropped.push(entry);
                }
            }
            let referenced: BTreeSet<u64> = std::iter::once(head.generation)
                .chain(kept.iter().map(|entry| entry.head.generation))
                .collect();
            let mut generations: BTreeSet<u64> = BTreeSet::new();
            for entry in &dropped {
                let generation = entry.head.generation;
                let bytes = if referenced.contains(&generation) || !generations.insert(generation) {
                    0
                } else {
                    dir_size(&dir.join("chunks").join(generation.to_string()))
                };
                report.generations.push(PrunedGeneration {
                    sid: sid.clone(),
                    generation,
                    version: entry.head.version,
                    archived_at: entry.archived_at,
                    bytes,
                });
                report.freed += bytes;
            }
            for (generation, path) in chunk_generations(&dir)? {
                let orphaned = !referenced.contains(&generation)
                    && !generations.contains(&generation)
                    && !head.archived.iter().any(|version| {
                        kept.iter().chain(&dropped).any(|entry| {
                            entry.head.version == *version && entry.head.generation == generation
                        })
                    })
                    && older_than(&path, retention);
                if orphaned {
                    let bytes = dir_size(&path);
                    generations.insert(generation);
                    report.generations.push(PrunedGeneration {
                        sid: sid.clone(),
                        generation,
                        version: 0,
                        archived_at: 0,
                        bytes,
                    });
                    report.freed += bytes;
                }
            }
            if !dry_run {
                for generation in &generations {
                    if !referenced.contains(generation) {
                        remove_tree(&dir.join("chunks").join(generation.to_string()))?;
                    }
                }
                for entry in &dropped {
                    match std::fs::remove_file(self.archive_file(&sid, entry.head.version)) {
                        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                            return Err(error);
                        }
                        _ => {}
                    }
                }
                if !dropped.is_empty() {
                    head.archived = kept.iter().map(|entry| entry.head.version).collect();
                    changed = true;
                }
            }
            let keep = self.kept_blobs(&head, &kept);
            report.swept_bytes += sweep_blobs_at(&dir, &keep, dry_run);
            report.swept_bytes += sweep_staging_at(&dir, dry_run);
            if changed {
                self.persist(&head)?;
                inner.heads.insert(sid, head);
            }
        }
        Ok(report)
    }
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![path.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                Ok(_) => total += entry.metadata().map_or(0, |meta| meta.len()),
                Err(_) => {}
            }
        }
    }
    total
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn chunk_generations(session: &Path) -> std::io::Result<Vec<(u64, PathBuf)>> {
    let entries = match std::fs::read_dir(session.join("chunks")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some(generation) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        {
            found.push((generation, entry.path()));
        }
    }
    Ok(found)
}

fn older_than(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed > age)
}

fn sweep_staging(session: &Path) {
    sweep_staging_at(session, false);
}

fn sweep_staging_at(session: &Path, dry_run: bool) -> u64 {
    let Ok(entries) = std::fs::read_dir(session.join("staging")) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        if older_than(&entry.path(), STAGING_GRACE) {
            swept += entry.metadata().map_or(0, |meta| meta.len());
            if !dry_run {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    swept
}

fn sweep_blobs(session: &Path, keep: &[String]) {
    sweep_blobs_at(session, keep, false);
}

fn sweep_blobs_at(session: &Path, keep: &[String], dry_run: bool) -> u64 {
    let Ok(entries) = std::fs::read_dir(session.join("blobs")) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) && older_than(&entry.path(), UNREFERENCED_GRACE) {
            swept += entry.metadata().map_or(0, |meta| meta.len());
            if !dry_run {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    swept
}

async fn authorise(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(token) if blake3::hash(token.as_bytes()) == shared.token_hash => {
            next.run(request).await
        }
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

#[derive(Deserialize)]
struct ChangesQuery {
    #[serde(default)]
    since: u64,
    #[serde(default)]
    wait: u64,
    #[serde(default)]
    peek: bool,
}

fn page(shared: &Shared, inner: &Inner, since: u64, peek: bool) -> Changes {
    let mut changes = Changes {
        server: shared.id.clone(),
        now: now_ms(),
        rev: inner.rev,
        heads: Vec::new(),
        more: false,
    };
    if peek {
        return changes;
    }
    let mut changed: Vec<&Head> = inner
        .heads
        .values()
        .filter(|head| head.rev > since)
        .collect();
    changed.sort_by_key(|head| head.rev);
    changes.more = changed.len() > PAGE;
    changes.heads = changed.into_iter().take(PAGE).cloned().collect();
    if changes.more
        && let Some(last) = changes.heads.last()
    {
        changes.rev = last.rev;
    }
    changes
}

async fn changes(
    State(shared): State<Arc<Shared>>,
    Query(query): Query<ChangesQuery>,
) -> Json<Changes> {
    let mut watcher = shared.rev.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(query.wait.min(MAX_WAIT_SECS));
    loop {
        let changes = {
            let inner = shared.inner.lock().await;
            page(&shared, &inner, query.since, query.peek)
        };
        let ready = if query.peek {
            changes.rev > query.since
        } else {
            !changes.heads.is_empty()
        };
        if ready || query.wait == 0 {
            return Json(changes);
        }
        match tokio::time::timeout_at(deadline, watcher.changed()).await {
            Ok(Ok(())) => {}
            _ => return Json(changes),
        }
    }
}

async fn leases(State(shared): State<Arc<Shared>>) -> Json<Leases> {
    let now = now_ms();
    let inner = shared.inner.lock().await;
    let held = inner
        .heads
        .values()
        .filter_map(|head| {
            let lease = head.lease.as_ref().filter(|lease| lease.active(now))?;
            Some((head.sid.clone(), lease.clone()))
        })
        .collect();
    Json(Leases { now, held })
}

async fn head(
    State(shared): State<Arc<Shared>>,
    Url(sid): Url<String>,
) -> Result<Json<Head>, Failure> {
    if !valid_sid(&sid) {
        return Err(invalid("bad session"));
    }
    let inner = shared.inner.lock().await;
    inner
        .heads
        .get(&sid)
        .cloned()
        .map(Json)
        .ok_or(Failure::Missing)
}

async fn send_file(path: PathBuf) -> Result<Response, Failure> {
    let file = tokio::fs::File::open(path).await?;
    let stream = tokio_util::io::ReaderStream::new(file);
    Ok(Body::from_stream(stream).into_response())
}

async fn receive_file(path: &Path, body: Body) -> Result<(), Failure> {
    let Some(parent) = path.parent() else {
        return Err(invalid("bad path"));
    };
    private_dir(parent)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
    name.push(format!(".part-{}", u64::from_le_bytes(nonce)));
    let partial = path.with_file_name(name);
    let written = async {
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut stream = body.into_data_stream();
        let mut total = 0u64;
        while let Some(piece) = stream.next().await {
            let piece = piece.map_err(std::io::Error::other)?;
            total += piece.len() as u64;
            if total > MAX_UPLOAD {
                return Err(std::io::Error::other("upload is too large"));
            }
            file.write_all(&piece).await?;
        }
        file.sync_all().await?;
        tokio::fs::rename(&partial, path).await
    }
    .await;
    if written.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
    }
    Ok(written?)
}

async fn chunk(
    State(shared): State<Arc<Shared>>,
    Url((sid, generation, seq)): Url<(String, u64, u64)>,
) -> Result<Response, Failure> {
    if !valid_sid(&sid) {
        return Err(invalid("bad session"));
    }
    send_file(
        shared
            .session(&sid)
            .join("chunks")
            .join(generation.to_string())
            .join(seq.to_string()),
    )
    .await
}

fn staged(shared: &Shared, sid: &str, upload: &str, generation: u64, seq: u64) -> PathBuf {
    shared
        .session(sid)
        .join("staging")
        .join(format!("{upload}-{generation}-{seq}"))
}

async fn stage(
    State(shared): State<Arc<Shared>>,
    Url((sid, upload, generation, seq)): Url<(String, String, u64, u64)>,
    body: Body,
) -> Result<StatusCode, Failure> {
    if !valid_sid(&sid) || !valid_upload(&upload) {
        return Err(invalid("bad upload"));
    }
    receive_file(&staged(&shared, &sid, &upload, generation, seq), body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn blob(
    State(shared): State<Arc<Shared>>,
    Url((sid, blob)): Url<(String, String)>,
) -> Result<Response, Failure> {
    if !valid_sid(&sid) || !valid_blob(&blob) {
        return Err(invalid("bad blob"));
    }
    send_file(shared.session(&sid).join("blobs").join(blob)).await
}

async fn store_blob(
    State(shared): State<Arc<Shared>>,
    Url((sid, blob)): Url<(String, String)>,
    body: Body,
) -> Result<StatusCode, Failure> {
    if !valid_sid(&sid) || !valid_blob(&blob) {
        return Err(invalid("bad blob"));
    }
    receive_file(&shared.session(&sid).join("blobs").join(blob), body).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn fresh(sid: &str) -> Head {
    Head {
        sid: sid.to_owned(),
        ..Head::default()
    }
}

fn locked(lease: &Lease, now: u64) -> Failure {
    Failure::Refused(
        StatusCode::LOCKED,
        Box::new(Refusal::Locked {
            lease: lease.clone(),
            now,
        }),
    )
}

async fn commit(
    State(shared): State<Arc<Shared>>,
    Url(sid): Url<String>,
    Json(commit): Json<Commit>,
) -> Result<Json<Head>, Failure> {
    if !valid_sid(&sid)
        || !valid_holder(&commit.holder)
        || !valid_upload(&commit.upload)
        || !commit.blobs.iter().all(|blob| valid_blob(blob))
    {
        return Err(invalid("bad commit"));
    }
    let mut inner = shared.inner.lock().await;
    let current = inner
        .heads
        .get(&sid)
        .cloned()
        .unwrap_or_else(|| fresh(&sid));
    if commit.expect != current.version {
        return Err(Failure::Refused(
            StatusCode::CONFLICT,
            Box::new(Refusal::Conflict { head: current }),
        ));
    }
    let now = now_ms();
    if let Some(lease) = &current.lease
        && lease.active(now)
        && lease.holder != commit.holder
    {
        return Err(locked(lease, now));
    }
    let first = if commit.generation == current.generation && current.version > 0 {
        if commit.seq < current.seq {
            return Err(invalid("history went backwards"));
        }
        current.seq
    } else if commit.generation > current.generation || current.version == 0 {
        0
    } else {
        return Err(invalid("history generation went backwards"));
    };
    let dir = shared.session(&sid);
    if first > 0
        && !dir
            .join("chunks")
            .join(commit.generation.to_string())
            .join((first - 1).to_string())
            .is_file()
    {
        return Err(invalid(
            "the history this builds on was pruned from the server, it has to be sent afresh",
        ));
    }
    for seq in first..commit.seq {
        if !staged(&shared, &sid, &commit.upload, commit.generation, seq).is_file() {
            return Err(invalid("a history chunk was not uploaded"));
        }
    }
    if !commit
        .blobs
        .iter()
        .all(|blob| dir.join("blobs").join(blob).is_file())
    {
        return Err(invalid("a file was not uploaded"));
    }
    let chunks = dir.join("chunks").join(commit.generation.to_string());
    private_dir(&chunks)?;
    for seq in first..commit.seq {
        std::fs::rename(
            staged(&shared, &sid, &commit.upload, commit.generation, seq),
            chunks.join(seq.to_string()),
        )?;
    }
    let rev = inner.rev + 1;
    let mut archived = current.archived.clone();
    let supersedes = current.version > 0
        && current.manifest.is_some()
        && current.pruned_at.is_none()
        && (commit.generation > current.generation || (commit.deleted && !current.deleted));
    if supersedes {
        let entry = Archived {
            head: Head {
                lease: None,
                archived: Vec::new(),
                ..current.clone()
            },
            archived_at: now,
            superseded_rev: rev,
            superseded_by: Some(commit.holder.clone()),
        };
        private_dir(&dir.join("archive"))?;
        mush_ai::private_io::write_private(
            &shared.archive_file(&sid, current.version),
            serde_json::to_vec(&entry)?,
        )?;
        archived.push(current.version);
    }
    let deleted_at = match (commit.deleted, current.deleted) {
        (false, _) => None,
        (true, true) => current.deleted_at.or(Some(now)),
        (true, false) => Some(now),
    };
    let head = Head {
        sid: sid.clone(),
        version: current.version + 1,
        rev,
        generation: commit.generation,
        seq: commit.seq,
        blobs: commit.blobs,
        manifest: Some(commit.manifest),
        lease: current.lease,
        updated_at: now,
        deleted: commit.deleted,
        deleted_at,
        pruned_at: None,
        archived,
        writer: Some(commit.holder.clone()),
    };
    shared.persist(&head)?;
    inner.rev = head.rev;
    inner.heads.insert(sid.clone(), head.clone());
    shared.rev.send_replace(head.rev);
    let keep = shared.kept_blobs(&head, &shared.archived(&sid)?);
    sweep_blobs(&dir, &keep);
    sweep_staging(&dir);
    Ok(Json(head))
}

async fn archive(
    State(shared): State<Arc<Shared>>,
    Url(sid): Url<String>,
) -> Result<Json<Vec<Archived>>, Failure> {
    if !valid_sid(&sid) {
        return Err(invalid("bad session"));
    }
    let _inner = shared.inner.lock().await;
    Ok(Json(shared.archived(&sid)?))
}

async fn machines(State(shared): State<Arc<Shared>>) -> Result<Json<Machines>, Failure> {
    Ok(Json(Machines {
        now: now_ms(),
        machines: shared.machines()?,
    }))
}

async fn report_machine(
    State(shared): State<Arc<Shared>>,
    Url(name): Url<String>,
    Json(report): Json<MachineReport>,
) -> Result<StatusCode, Failure> {
    if !valid_holder(&name) {
        return Err(invalid("bad machine name"));
    }
    let machine = Machine {
        name: name.clone(),
        seen_at: now_ms(),
        report,
    };
    private_dir(&shared.machines_dir())?;
    mush_ai::private_io::write_private(
        &shared.machines_dir().join(format!("{name}.json")),
        serde_json::to_vec(&machine)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}

async fn prune(
    State(shared): State<Arc<Shared>>,
    Json(request): Json<PruneRequest>,
) -> Result<Json<PruneReport>, Failure> {
    Ok(Json(shared.prune(request.dry_run).await?))
}

async fn lease(
    State(shared): State<Arc<Shared>>,
    Url(sid): Url<String>,
    Json(request): Json<LeaseRequest>,
) -> Result<Json<LeaseGrant>, Failure> {
    if !valid_sid(&sid) || !valid_holder(&request.holder) {
        return Err(invalid("bad lease"));
    }
    let mut inner = shared.inner.lock().await;
    let mut head = inner
        .heads
        .get(&sid)
        .cloned()
        .unwrap_or_else(|| fresh(&sid));
    let now = now_ms();
    let expires_at = now + request.ttl_ms.clamp(1_000, 24 * 60 * 60 * 1000);
    let renewed = match &head.lease {
        Some(lease) if lease.active(now) && lease.holder == request.holder => Lease {
            expires_at,
            ..lease.clone()
        },
        Some(lease) if lease.active(now) && !request.take => return Err(locked(lease, now)),
        previous => Lease {
            holder: request.holder,
            fence: previous.as_ref().map_or(0, |lease| lease.fence) + 1,
            expires_at,
        },
    };
    let changed_hands = head
        .lease
        .as_ref()
        .is_none_or(|lease| lease.fence != renewed.fence);
    head.lease = Some(renewed.clone());
    if changed_hands {
        head.rev = inner.rev + 1;
        inner.rev = head.rev;
    }
    shared.persist(&head)?;
    inner.heads.insert(sid, head);
    if changed_hands {
        shared.rev.send_replace(inner.rev);
    }
    Ok(Json(LeaseGrant {
        lease: renewed,
        now,
    }))
}

async fn release(
    State(shared): State<Arc<Shared>>,
    Url(sid): Url<String>,
    Json(request): Json<ReleaseRequest>,
) -> Result<StatusCode, Failure> {
    if !valid_sid(&sid) || !valid_holder(&request.holder) {
        return Err(invalid("bad release"));
    }
    let mut inner = shared.inner.lock().await;
    let Some(mut head) = inner.heads.get(&sid).cloned() else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let ours = head
        .lease
        .as_ref()
        .is_some_and(|lease| lease.holder == request.holder || request.force);
    if !ours {
        return Ok(StatusCode::NO_CONTENT);
    }
    head.lease = None;
    head.rev = inner.rev + 1;
    inner.rev = head.rev;
    shared.persist(&head)?;
    inner.heads.insert(sid, head);
    shared.rev.send_replace(inner.rev);
    Ok(StatusCode::NO_CONTENT)
}
