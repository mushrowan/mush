use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::Watcher;

use crate::engine::{Direction, Engine, Report, Scope, SyncError};

const SETTLE: Duration = Duration::from_secs(2);
const SWEEP: Duration = Duration::from_secs(10 * 60);
const LONG_POLL: Duration = Duration::from_secs(240);
const PATIENCE: Duration = Duration::from_secs(120);

fn session_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if crate::tool_output::saved_output(name) {
        return Some(crate::tool_output::TOOL_OUTPUT.to_owned());
    }
    let id = name
        .strip_suffix(".history.jsonl")
        .or_else(|| name.strip_suffix(".json"))?;
    crate::protocol::valid_session(id).then(|| id.to_owned())
}

fn log(report: &Report) {
    if report.quiet() {
        return;
    }
    tracing::info!(
        pushed = report.pushed.len(),
        pulled = report.pulled.len(),
        forked = report.forked.len(),
        deleted = report.deleted.len(),
        waiting = report.waiting.len(),
        failed = report.failed.len(),
        "sync pass"
    );
    for (session, error) in &report.failed {
        tracing::warn!(session, error, "session did not sync");
    }
    for (session, fork) in &report.forked {
        tracing::warn!(
            session,
            fork,
            "session diverged, the local side was kept as a fork"
        );
    }
}

pub async fn watch(
    engine: Arc<Engine>,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), SyncError> {
    let (wake, mut woken) = tokio::sync::mpsc::channel::<bool>(4);
    let dirty = Arc::new(Mutex::new(BTreeSet::new()));
    std::fs::create_dir_all(&engine.layout().sessions)?;
    let mut watcher = {
        let wake = wake.clone();
        let dirty = Arc::clone(&dirty);
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else {
                return;
            };
            let ids: Vec<String> = event
                .paths
                .iter()
                .filter_map(|path| session_of(path))
                .collect();
            if ids.is_empty() {
                return;
            }
            dirty
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(ids);
            let _ = wake.try_send(false);
        })
        .map_err(std::io::Error::other)?
    };
    watcher
        .watch(
            &engine.layout().sessions,
            notify::RecursiveMode::NonRecursive,
        )
        .map_err(std::io::Error::other)?;
    std::fs::create_dir_all(&engine.layout().tool_output)?;
    watcher
        .watch(
            &engine.layout().tool_output,
            notify::RecursiveMode::NonRecursive,
        )
        .map_err(std::io::Error::other)?;
    let poller = tokio::spawn(poll(Arc::clone(&engine), wake));
    tokio::pin!(shutdown);
    let mut full = true;
    loop {
        let scope = if full {
            dirty
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            Scope::All
        } else {
            let ids = std::mem::take(
                &mut *dirty
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            Scope::Sessions(ids.into_iter().collect())
        };
        match engine.pass(scope, Direction::Both, None, PATIENCE).await {
            Ok(report) => log(&report),
            Err(error) if error.offline() => tracing::info!(%error, "sync server unreachable"),
            Err(error) => tracing::warn!(%error, "sync pass failed"),
        }
        full = tokio::select! {
            () = &mut shutdown => break,
            woke = woken.recv() => {
                let mut full = woke.unwrap_or(true);
                tokio::time::sleep(SETTLE).await;
                while let Ok(again) = woken.try_recv() {
                    full |= again;
                }
                full
            }
            () = tokio::time::sleep(SWEEP) => true,
        };
    }
    poller.abort();
    drop(watcher);
    Ok(())
}

async fn poll(engine: Arc<Engine>, wake: tokio::sync::mpsc::Sender<bool>) {
    let mut seen = 0;
    let mut backoff = Duration::from_secs(5);
    loop {
        match engine.remote().changes(seen, LONG_POLL, true).await {
            Ok(changes) => {
                backoff = Duration::from_secs(5);
                if changes.rev != seen {
                    seen = changes.rev;
                    let _ = wake.send(true).await;
                }
            }
            Err(error) => {
                tracing::debug!(%error, "waiting for the sync server");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(300));
            }
        }
    }
}
