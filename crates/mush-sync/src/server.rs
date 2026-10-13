use std::collections::HashMap;
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
    Changes, Commit, Head, Lease, LeaseGrant, LeaseRequest, Refusal, ReleaseRequest, now_ms,
    valid_blob, valid_holder, valid_sid, valid_upload,
};

const MAX_UPLOAD: u64 = 512 << 20;
const MAX_WAIT_SECS: u64 = 300;
const PAGE: usize = 500;
const UNREFERENCED_GRACE: Duration = Duration::from_secs(60 * 60);
const STAGING_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub root: PathBuf,
    pub token_hash: String,
}

struct Shared {
    root: PathBuf,
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
    let app = router(config)?;
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%error, "could not turn off nagle");
        }
    });
    axum::serve(listener, app).await
}

pub fn router(config: ServerConfig) -> std::io::Result<Router> {
    let shared = Arc::new(Shared::open(config)?);
    Ok(Router::new()
        .route("/v1/changes", get(changes))
        .route("/v1/sessions/{sid}", get(head))
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
        .with_state(shared))
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
}

fn older_than(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed > age)
}

fn sweep_staging(session: &Path) {
    let Ok(entries) = std::fs::read_dir(session.join("staging")) else {
        return;
    };
    for entry in entries.flatten() {
        if older_than(&entry.path(), STAGING_GRACE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn sweep_blobs(session: &Path, keep: &[String]) {
    let Ok(entries) = std::fs::read_dir(session.join("blobs")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(&name) && older_than(&entry.path(), UNREFERENCED_GRACE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
    let head = Head {
        sid: sid.clone(),
        version: current.version + 1,
        rev: inner.rev + 1,
        generation: commit.generation,
        seq: commit.seq,
        blobs: commit.blobs,
        manifest: Some(commit.manifest),
        lease: current.lease,
        updated_at: now,
    };
    shared.persist(&head)?;
    inner.rev = head.rev;
    inner.heads.insert(sid, head.clone());
    shared.rev.send_replace(head.rev);
    sweep_blobs(&dir, &head.blobs);
    Ok(Json(head))
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
