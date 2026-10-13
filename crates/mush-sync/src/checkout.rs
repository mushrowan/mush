use std::sync::Arc;
use std::time::Duration;

use mush_session::{SessionLease, SyncState};

use crate::engine::{Direction, Engine, Scope, SyncError};
use crate::protocol::{LeaseRequest, ReleaseRequest};
use crate::remote::Claimed;

const OPEN_PATIENCE: Duration = Duration::from_secs(20);
const RENEW_PATIENCE: Duration = Duration::from_secs(1);
const FINISH_PATIENCE: Duration = Duration::from_secs(30);
const PUSH_EVERY: Duration = Duration::from_secs(5);
const WATCH_WAIT: Duration = Duration::from_secs(240);
const WATCH_BACKOFF: Duration = Duration::from_secs(60);
const OFFLINE: &str =
    "the sync server is unreachable, this session's changes stay here until it is back";

type Status = Arc<tokio::sync::watch::Sender<SyncState>>;

#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    #[error(
        "session {session} is open on {holder}, its lease there runs out in {}s. close it on {holder}, or run `mush sync take {session}` to move it here",
        expires_in.as_secs()
    )]
    Held {
        session: String,
        holder: String,
        expires_in: Duration,
    },
    #[error(transparent)]
    Sync(#[from] SyncError),
}

#[derive(Debug)]
pub enum Opened {
    Claimed(Checkout),
    Offline(String),
}

#[derive(Debug)]
pub struct Checkout {
    engine: Arc<Engine>,
    session: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    renewing: Option<tokio::task::JoinHandle<()>>,
    watching: Option<tokio::task::JoinHandle<()>>,
    state: tokio::sync::watch::Receiver<SyncState>,
}

fn note_problem(status: &Status, problem: Option<String>) {
    status.send_if_modified(|state| {
        if state.problem == problem {
            return false;
        }
        state.problem = problem;
        true
    });
}

fn note_taken(status: &Status, holder: String) {
    status.send_if_modified(|state| {
        if state.taken_by.is_some() {
            return false;
        }
        state.taken_by = Some(holder);
        true
    });
}

fn problem_of(result: &Result<crate::engine::Report, SyncError>) -> Option<String> {
    match result {
        Ok(report) => report
            .failed
            .first()
            .map(|(session, error)| format!("{session} did not sync: {error}")),
        Err(error) if error.offline() => Some(OFFLINE.to_owned()),
        Err(error) => Some(error.to_string()),
    }
}

pub async fn checkout(
    engine: Arc<Engine>,
    lease: &SessionLease,
    ttl: Duration,
    take: bool,
) -> Result<Opened, CheckoutError> {
    let session = lease.id().to_string();
    let sid = engine.keys().sid(&session);
    let request = LeaseRequest {
        holder: engine.machine().to_owned(),
        ttl_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
        take,
    };
    match engine.remote().claim(&sid, &request).await {
        Ok(Claimed::Granted(_)) => {}
        Ok(Claimed::Held { lease, now }) => {
            return Err(CheckoutError::Held {
                session,
                holder: lease.holder,
                expires_in: Duration::from_millis(lease.expires_at.saturating_sub(now)),
            });
        }
        Err(error) if error.offline() => return Ok(Opened::Offline(error.to_string())),
        Err(error) => return Err(SyncError::from(error).into()),
    }
    let opened = engine
        .pass(
            Scope::Family(session.clone()),
            Direction::Both,
            Some(lease),
            OPEN_PATIENCE,
        )
        .await;
    match &opened {
        Ok(report) if !report.failed.is_empty() => {
            tracing::warn!(?report.failed, "some of this session did not sync before opening");
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "could not bring the session up to date before opening")
        }
    }
    let (sender, state) = tokio::sync::watch::channel(SyncState {
        problem: problem_of(&opened),
        ..SyncState::default()
    });
    let status = Arc::new(sender);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let watching = tokio::spawn(watch_lease(Arc::clone(&engine), sid, Arc::clone(&status)));
    let renewing = tokio::spawn(renew(
        Arc::clone(&engine),
        session.clone(),
        request,
        ttl,
        stopped,
        status,
    ));
    Ok(Opened::Claimed(Checkout {
        engine,
        session,
        stop: Some(stop),
        renewing: Some(renewing),
        watching: Some(watching),
        state,
    }))
}

async fn watch_lease(engine: Arc<Engine>, sid: String, status: Status) {
    let mut seen = 0;
    let mut backoff = Duration::from_secs(2);
    loop {
        match engine.remote().changes(seen, WATCH_WAIT, true).await {
            Ok(changes) => {
                backoff = Duration::from_secs(2);
                if changes.rev == seen {
                    continue;
                }
                seen = changes.rev;
                let Ok(leases) = engine.remote().leases(RENEW_PATIENCE * 10).await else {
                    continue;
                };
                if let Some((_, lease)) = leases
                    .held
                    .into_iter()
                    .find(|(held, lease)| *held == sid && lease.holder != engine.machine())
                {
                    tracing::warn!(
                        holder = lease.holder,
                        "another machine took this session over, it is read-only here now"
                    );
                    note_taken(&status, lease.holder);
                    return;
                }
            }
            Err(error) => {
                tracing::debug!(%error, "waiting for the sync server to watch the lease");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(WATCH_BACKOFF);
            }
        }
    }
}

async fn renew(
    engine: Arc<Engine>,
    session: String,
    request: LeaseRequest,
    ttl: Duration,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
    status: Status,
) {
    let sid = engine.keys().sid(&session);
    let mut renewed = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = &mut stopped => return,
            () = tokio::time::sleep(PUSH_EVERY) => {}
        }
        if status.borrow().read_only() {
            return;
        }
        if renewed.elapsed() >= ttl / 3 {
            renewed = tokio::time::Instant::now();
            match engine.remote().claim(&sid, &request).await {
                Ok(Claimed::Granted(_)) => {}
                Ok(Claimed::Held { lease, .. }) => {
                    tracing::warn!(
                        session,
                        holder = lease.holder,
                        "another machine took this session over, it is read-only here now"
                    );
                    note_taken(&status, lease.holder);
                    return;
                }
                Err(error) => tracing::debug!(%error, "could not renew the session lease"),
            }
        }
        let pushed = engine
            .pass(
                Scope::Family(session.clone()),
                Direction::PushOnly,
                None,
                RENEW_PATIENCE,
            )
            .await;
        if let Err(error) = &pushed {
            tracing::debug!(%error, "could not push the open session");
        }
        note_problem(&status, problem_of(&pushed));
    }
}

impl Checkout {
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    #[must_use]
    pub fn state(&self) -> tokio::sync::watch::Receiver<SyncState> {
        self.state.clone()
    }

    pub async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.watching.take() {
            task.abort();
        }
        if let Some(task) = self.renewing.take() {
            let _ = task.await;
        }
        if let Err(error) = self
            .engine
            .pass(
                Scope::Family(self.session.clone()),
                Direction::PushOnly,
                None,
                FINISH_PATIENCE,
            )
            .await
        {
            tracing::warn!(%error, "could not push the session as it closed");
        }
        let release = ReleaseRequest {
            holder: self.engine.machine().to_owned(),
            force: false,
        };
        let sid = self.engine.keys().sid(&self.session);
        if let Err(error) = self.engine.remote().release(&sid, &release).await {
            tracing::debug!(%error, "could not release the session lease");
        }
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if let Some(task) = self.renewing.take() {
            task.abort();
        }
        if let Some(task) = self.watching.take() {
            task.abort();
        }
    }
}
