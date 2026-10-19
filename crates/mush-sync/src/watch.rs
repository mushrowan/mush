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

fn changes_something(kind: notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    match kind {
        notify::EventKind::Create(_)
        | notify::EventKind::Remove(_)
        | notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        notify::EventKind::Modify(modify) => !matches!(modify, ModifyKind::Metadata(_)),
        notify::EventKind::Access(_) | notify::EventKind::Any | notify::EventKind::Other => false,
    }
}

fn session_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if mush_ai::dirs::saved_output_name(name) {
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

#[derive(Debug, Clone, Copy)]
enum Wake {
    Sweep,
    Remote,
    Local,
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
            if !changes_something(event.kind) {
                return;
            }
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
    let mut next = Wake::Sweep;
    loop {
        let mut dirty = std::mem::take(
            &mut *dirty
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let scope = match next {
            Wake::Sweep => {
                dirty.clear();
                Scope::All
            }
            Wake::Remote => Scope::Changed(dirty.into_iter().collect()),
            Wake::Local => Scope::Sessions(dirty.into_iter().collect()),
        };
        match engine.pass(scope, Direction::Both, None, PATIENCE).await {
            Ok(report) => log(&report),
            Err(error) if error.offline() => tracing::info!(%error, "sync server unreachable"),
            Err(error) => tracing::warn!(%error, "sync pass failed"),
        }
        next = tokio::select! {
            () = &mut shutdown => break,
            woke = woken.recv() => {
                let mut remote = woke.unwrap_or(true);
                tokio::time::sleep(SETTLE).await;
                while let Ok(again) = woken.try_recv() {
                    remote |= again;
                }
                if remote { Wake::Remote } else { Wake::Local }
            }
            () = tokio::time::sleep(SWEEP) => Wake::Sweep,
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
    };

    #[test]
    fn reading_a_session_never_wakes_the_watcher_but_writing_one_does() {
        for quiet in [
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
            EventKind::Other,
        ] {
            assert!(!changes_something(quiet), "{quiet:?}");
        }
        for loud in [
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::To)),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
        ] {
            assert!(changes_something(loud), "{loud:?}");
        }
    }
}
