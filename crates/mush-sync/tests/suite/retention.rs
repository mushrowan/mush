use std::time::Duration;

use mush_sync::restore::Restored;
use mush_sync::{Opened, checkout};

use crate::harness::{bloated, files_under, machine, server_with};

const GRACE: Duration = Duration::from_millis(400);

fn session_generations(root: &std::path::Path, id: &str) -> std::collections::BTreeSet<String> {
    let sid = mush_sync::Keys::from_master(&crate::harness::MASTER).sid(id);
    chunk_generations(&root.join("sessions").join(sid))
}

fn chunk_generations(root: &std::path::Path) -> std::collections::BTreeSet<String> {
    if !root.exists() {
        return std::collections::BTreeSet::new();
    }
    files_under(root)
        .into_iter()
        .filter_map(|path| {
            let text = path.to_string_lossy().into_owned();
            let (_, rest) = text.split_once("/chunks/")?;
            Some(rest.split('/').next()?.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn a_deleted_session_is_pruned_only_after_the_grace_and_once_every_live_machine_saw_it_go() {
    let root = tempfile::tempdir().unwrap();
    let server = server_with(root.path(), Some(GRACE)).await;
    let (dot, esme, old) = (
        machine("dot", &server),
        machine("esme", &server),
        machine("old", &server),
    );
    let id = dot.create("doomed", &["keep me for a while"]);
    dot.sync().await;
    old.sync().await;
    esme.sync().await;
    tokio::time::sleep(GRACE * 3).await;
    dot.create("keeps the feed moving", &["so esme reports again"]);
    dot.sync().await;
    esme.sync().await;

    dot.store().delete(&id).unwrap();
    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
    tokio::time::sleep(GRACE).await;

    let early = dot.engine.remote().prune(true).await.unwrap();
    assert!(early.sessions.is_empty(), "{early:?}");
    assert!(
        early
            .blocked
            .iter()
            .any(|blocked| blocked.reason.contains("esme")),
        "{early:?}"
    );
    assert_eq!(early.stale_machines, ["old".to_owned()]);

    assert_eq!(esme.sync().await.deleted, [id.to_string()]);
    let before = files_under(root.path());
    let planned = dot.engine.remote().prune(true).await.unwrap();
    assert_eq!(planned.sessions.len(), 1, "{planned:?}");
    assert!(planned.freed > 0);
    assert_eq!(
        files_under(root.path()),
        before,
        "a dry run removes nothing"
    );

    let done = dot.engine.remote().prune(false).await.unwrap();
    assert_eq!(done.sessions.len(), 1);
    assert!(session_generations(root.path(), id.as_str()).is_empty());
    assert!(dot.sync().await.quiet());
    assert!(esme.sync().await.quiet());
    assert!(
        dot.engine
            .restore(id.as_str(), None)
            .await
            .unwrap_err()
            .to_string()
            .contains("pruned")
    );

    old.append(&id, "old kept working offline");
    old.sync().await;
    assert_eq!(esme.sync().await.pulled, [id.to_string()]);
    assert_eq!(
        esme.texts(&id),
        ["keep me for a while", "old kept working offline"]
    );
    assert!(old.sync().await.quiet());
}

#[tokio::test]
async fn an_older_generation_stays_restorable_until_the_grace_runs_out() {
    let root = tempfile::tempdir().unwrap();
    let server = server_with(root.path(), Some(GRACE)).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = bloated(&dot, "after the bloat");
    dot.sync().await;
    esme.sync().await;
    let bloated_history = dot.history(&id);

    dot.store().slim_history(&id, true).unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(
        session_generations(root.path(), id.as_str()),
        ["0".to_owned(), "1".to_owned()].into()
    );

    let restorable = esme.engine.restorable().await.unwrap();
    let entry = restorable
        .iter()
        .find(|entry| entry.session == id.as_str())
        .unwrap();
    assert_eq!(entry.deleted_at, None);
    assert_eq!(entry.generations.len(), 1);
    assert_eq!(entry.generations[0].generation, 0);
    assert_eq!(entry.generations[0].len, bloated_history.len() as u64);

    let Restored::Copied { copy, missing, .. } =
        esme.engine.restore(id.as_str(), Some(0)).await.unwrap()
    else {
        panic!("an older generation comes back as a copy");
    };
    assert_eq!(missing, 0);
    let copy = mush_session::SessionId::from(copy.as_str());
    assert_eq!(esme.history(&copy), bloated_history);
    assert_eq!(
        esme.title(&copy).as_deref(),
        Some("untitled (generation 0)")
    );
    dot.sync().await;
    assert_eq!(dot.history(&copy), bloated_history);

    tokio::time::sleep(GRACE).await;
    dot.sync().await;
    esme.sync().await;
    let done = dot.engine.remote().prune(false).await.unwrap();
    assert_eq!(done.generations.len(), 1, "{done:?}");
    assert!(done.generations[0].bytes > 0);
    assert_eq!(
        session_generations(root.path(), id.as_str()),
        ["1".to_owned()].into()
    );
    assert!(
        dot.engine
            .restore(id.as_str(), Some(0))
            .await
            .unwrap_err()
            .to_string()
            .contains("not kept")
    );
    assert!(dot.sync().await.quiet());
    assert_eq!(esme.history(&id), dot.history(&id));
}

#[tokio::test]
async fn an_open_session_is_never_pruned_under_its_holder() {
    let root = tempfile::tempdir().unwrap();
    let server = server_with(root.path(), Some(GRACE)).await;
    let dot = machine("dot", &server);
    let id = bloated(&dot, "busy");
    dot.sync().await;
    dot.store().slim_history(&id, true).unwrap();
    dot.sync().await;

    let lease = dot.store().acquire(&id).unwrap();
    let Opened::Claimed(open) =
        checkout(dot.engine.clone(), &lease, Duration::from_secs(60), false)
            .await
            .unwrap()
    else {
        panic!("dot reaches the server");
    };
    tokio::time::sleep(GRACE).await;
    let report = dot.engine.remote().prune(false).await.unwrap();
    assert!(report.generations.is_empty(), "{report:?}");
    assert!(
        report
            .blocked
            .iter()
            .any(|blocked| blocked.reason == "open on dot"),
        "{report:?}"
    );
    assert!(session_generations(root.path(), id.as_str()).contains("0"));
    open.finish().await;
    drop(lease);
    assert_eq!(
        dot.engine
            .remote()
            .prune(false)
            .await
            .unwrap()
            .generations
            .len(),
        1
    );
}

#[tokio::test]
async fn a_deleted_session_comes_back_from_the_server_on_every_machine() {
    let root = tempfile::tempdir().unwrap();
    let server = server_with(root.path(), None).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("oops", &["deleted by mistake"]);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    std::fs::write(dot.artifact(&id, "notes.md"), "keep the notes").unwrap();
    dot.sync().await;
    esme.sync().await;

    esme.store().delete(&id).unwrap();
    esme.sync().await;
    assert_eq!(dot.sync().await.deleted, [id.to_string()]);
    let listed = dot.engine.restorable().await.unwrap();
    assert!(
        listed
            .iter()
            .any(|entry| entry.session == id.as_str() && entry.deleted_at.is_some())
    );

    assert_eq!(
        dot.engine.restore(id.as_str(), None).await.unwrap(),
        Restored::Undeleted {
            session: id.to_string()
        }
    );
    assert_eq!(dot.texts(&id), ["deleted by mistake"]);
    assert_eq!(esme.sync().await.pulled, [id.to_string()]);
    assert_eq!(esme.texts(&id), ["deleted by mistake"]);
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "notes.md")).unwrap(),
        "keep the notes"
    );
    assert!(
        dot.engine
            .restore(id.as_str(), None)
            .await
            .unwrap_err()
            .to_string()
            .contains("not deleted")
    );
}

#[tokio::test]
async fn a_session_that_gains_its_first_history_is_not_a_replaced_generation() {
    let root = tempfile::tempdir().unwrap();
    let server = server_with(root.path(), None).await;
    let dot = machine("dot", &server);
    let mut session = mush_session::Session::new("test-model", "/tmp");
    session.meta.title = Some("header only".into());
    dot.store().save(&session).unwrap();
    let id = session.meta.id.clone();
    let _ = std::fs::remove_file(dot.store().history_path(&id));
    dot.sync().await;
    dot.append(&id, "the first message");
    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
    assert!(
        dot.engine
            .restorable()
            .await
            .unwrap()
            .iter()
            .all(|entry| entry.session != id.as_str())
    );
    assert_eq!(
        session_generations(root.path(), id.as_str()),
        ["0".to_owned()].into()
    );
}
