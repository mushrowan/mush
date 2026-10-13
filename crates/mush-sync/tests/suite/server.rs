use std::time::{Duration, Instant};

use mush_sync::protocol::{Commit, LeaseRequest, manifest_aad};
use mush_sync::remote::{Claimed, Committed};
use mush_sync::{Keys, Remote};

use crate::harness::{MASTER, machine, server_at};

fn remote(url: &str) -> Remote {
    Remote::new(
        url,
        Keys::from_master(&MASTER).token(),
        Duration::from_secs(2),
    )
    .unwrap()
}

fn lease(holder: &str, ttl_ms: u64) -> LeaseRequest {
    LeaseRequest {
        holder: holder.to_owned(),
        ttl_ms,
        take: false,
    }
}

#[tokio::test]
async fn a_stale_commit_is_refused_as_a_conflict() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let remote = remote(&server.url);
    let keys = Keys::from_master(&MASTER);
    let sid = keys.sid("session");
    let commit = |expect: u64| Commit {
        expect,
        holder: "dot".into(),
        upload: "up1".into(),
        generation: 0,
        seq: 0,
        blobs: Vec::new(),
        manifest: "e30=".into(),
        deleted: false,
    };
    let Committed::Accepted(head) = remote.commit(&sid, &commit(0)).await.unwrap() else {
        panic!("the first commit lands");
    };
    assert_eq!(head.version, 1);
    let Committed::Conflict(current) = remote.commit(&sid, &commit(0)).await.unwrap() else {
        panic!("a stale commit must conflict");
    };
    assert_eq!(current.version, 1);
    let missing = Commit {
        seq: 1,
        ..commit(1)
    };
    assert!(remote.commit(&sid, &missing).await.is_err());
    let _ = manifest_aad(&sid, 1);
}

#[tokio::test]
async fn leases_refuse_others_until_they_lapse_and_then_move_the_fence() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let remote = remote(&server.url);
    let sid = Keys::from_master(&MASTER).sid("session");

    let Claimed::Granted(first) = remote.claim(&sid, &lease("dot", 1_000)).await.unwrap() else {
        panic!("a free session is granted");
    };
    assert_eq!(first.lease.fence, 1);
    let Claimed::Held { lease: held, .. } =
        remote.claim(&sid, &lease("esme", 60_000)).await.unwrap()
    else {
        panic!("a held session is refused");
    };
    assert_eq!(held.holder, "dot");
    let Claimed::Granted(renewed) = remote.claim(&sid, &lease("dot", 1_000)).await.unwrap() else {
        panic!("the holder renews");
    };
    assert_eq!(renewed.lease.fence, 1);

    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let Claimed::Granted(taken) = remote.claim(&sid, &lease("esme", 60_000)).await.unwrap() else {
        panic!("a lapsed lease is free");
    };
    assert_eq!(taken.lease.holder, "esme");
    assert_eq!(taken.lease.fence, 2);
}

#[tokio::test]
async fn a_waiting_watcher_wakes_as_soon_as_something_changes() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let watcher = remote(&server.url);
    let start = watcher.changes(0, Duration::ZERO, true).await.unwrap();
    let waiting = tokio::spawn(async move {
        let begun = Instant::now();
        let changes = watcher
            .changes(start.rev, Duration::from_secs(30), true)
            .await
            .unwrap();
        (changes.rev, begun.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sid = Keys::from_master(&MASTER).sid("session");
    remote(&server.url)
        .claim(&sid, &lease("dot", 60_000))
        .await
        .unwrap();
    let (rev, waited) = waiting.await.unwrap();
    assert!(rev > start.rev);
    assert!(waited < Duration::from_secs(10), "{waited:?}");
}

#[tokio::test]
async fn the_server_keeps_its_identity_and_data_across_restarts() {
    let root = tempfile::tempdir().unwrap();
    let first = server_at(root.path()).await;
    let dot = machine("dot", &first);
    let id = dot.create("durable", &["persisted"]);
    dot.sync().await;
    let identity = remote(&first.url)
        .changes(0, Duration::ZERO, true)
        .await
        .unwrap()
        .server;
    drop(first);

    let second = server_at(root.path()).await;
    let changes = remote(&second.url)
        .changes(0, Duration::ZERO, false)
        .await
        .unwrap();
    assert_eq!(changes.server, identity);
    assert_eq!(changes.heads.len(), 1);
    let esme = machine("esme", &second);
    esme.sync().await;
    assert_eq!(esme.texts(&id), ["persisted"]);
    let dot = dot.moved_to(&second);
    assert!(dot.sync().await.quiet());
}

#[tokio::test]
async fn appending_onto_history_the_server_no_longer_has_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let remote = remote(&server.url);
    let sid = Keys::from_master(&MASTER).sid("session");
    remote
        .stage(&sid, "up1", 0, 0, b"first".to_vec())
        .await
        .unwrap();
    let first = Commit {
        expect: 0,
        holder: "dot".into(),
        upload: "up1".into(),
        generation: 0,
        seq: 1,
        blobs: Vec::new(),
        manifest: "e30=".into(),
        deleted: false,
    };
    assert!(matches!(
        remote.commit(&sid, &first).await.unwrap(),
        Committed::Accepted(_)
    ));
    std::fs::remove_dir_all(root.path().join("sessions").join(&sid).join("chunks")).unwrap();
    remote
        .stage(&sid, "up2", 0, 1, b"second".to_vec())
        .await
        .unwrap();
    let append = Commit {
        expect: 1,
        upload: "up2".into(),
        seq: 2,
        ..first
    };
    let refused = remote.commit(&sid, &append).await.unwrap_err();
    assert!(refused.to_string().contains("pruned"), "{refused}");
}
