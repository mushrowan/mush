use std::sync::Arc;
use std::time::Duration;

use mush_session::SessionLease;

use crate::engine::{Direction, Engine, Scope, SyncError};
use crate::protocol::{LeaseRequest, ReleaseRequest};
use crate::remote::Claimed;

const OPEN_PATIENCE: Duration = Duration::from_secs(20);
const RENEW_PATIENCE: Duration = Duration::from_secs(1);
const FINISH_PATIENCE: Duration = Duration::from_secs(30);

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
    match engine
        .pass(
            Scope::Family(session.clone()),
            Direction::Both,
            Some(lease),
            OPEN_PATIENCE,
        )
        .await
    {
        Ok(report) if !report.failed.is_empty() => {
            tracing::warn!(?report.failed, "some of this session did not sync before opening");
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "could not bring the session up to date before opening")
        }
    }
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let renewing = tokio::spawn(renew(
        Arc::clone(&engine),
        session.clone(),
        request,
        ttl,
        stopped,
    ));
    Ok(Opened::Claimed(Checkout {
        engine,
        session,
        stop: Some(stop),
        renewing: Some(renewing),
    }))
}

async fn renew(
    engine: Arc<Engine>,
    session: String,
    request: LeaseRequest,
    ttl: Duration,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let sid = engine.keys().sid(&session);
    loop {
        tokio::select! {
            _ = &mut stopped => return,
            () = tokio::time::sleep(ttl / 3) => {}
        }
        match engine.remote().claim(&sid, &request).await {
            Ok(Claimed::Granted(_)) => {}
            Ok(Claimed::Held { lease, .. }) => tracing::warn!(
                session,
                holder = lease.holder,
                "another machine took this session over, what happens here from now on is kept as a fork"
            ),
            Err(error) => tracing::debug!(%error, "could not renew the session lease"),
        }
        if let Err(error) = engine
            .pass(
                Scope::Family(session.clone()),
                Direction::PushOnly,
                None,
                RENEW_PATIENCE,
            )
            .await
        {
            tracing::debug!(%error, "could not push the open session");
        }
    }
}

impl Checkout {
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    pub async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
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
    }
}
