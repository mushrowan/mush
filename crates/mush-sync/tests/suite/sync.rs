use std::time::Duration;

use mush_sync::{CheckoutError, Direction, Limits, Opened, Scope, checkout};

use crate::harness::{files_under, machine, machine_with, server_at};

#[tokio::test]
async fn a_session_written_on_one_machine_resumes_on_another() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("plan the trip", &["hello", "pack the tent"]);

    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
    assert_eq!(esme.sync().await.pulled, [id.to_string()]);

    assert_eq!(esme.texts(&id), ["hello", "pack the tent"]);
    assert_eq!(esme.title(&id).as_deref(), Some("plan the trip"));
    assert_eq!(esme.history(&id), dot.history(&id));
    assert!(dot.sync().await.quiet());
    assert!(esme.sync().await.quiet());
}

#[tokio::test]
async fn appending_sends_only_the_new_tail() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let big = "a long tool transcript line ".repeat(40_000);
    let id = dot.create("big", &[&big]);
    dot.sync().await;
    esme.sync().await;

    dot.append(&id, "one more small line");
    dot.sync().await;

    let chunks: Vec<_> = files_under(root.path())
        .into_iter()
        .filter(|path| path.to_string_lossy().contains("/chunks/"))
        .collect();
    assert_eq!(chunks.len(), 2, "{chunks:?}");
    let sizes: Vec<u64> = chunks
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .collect();
    assert!(sizes.iter().any(|size| *size < 4_096), "{sizes:?}");

    assert_eq!(esme.sync().await.pulled, [id.to_string()]);
    assert_eq!(esme.history(&id), dot.history(&id));
    assert_eq!(esme.texts(&id).last().unwrap(), "one more small line");
}

#[tokio::test]
async fn nothing_on_the_server_is_readable_without_the_key() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let dot = machine("dot", &server);
    let id = dot.create("secret title marker", &["secret body marker"]);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    std::fs::write(dot.artifact(&id, "notes.md"), "secret notes marker").unwrap();
    dot.sync().await;

    for path in files_under(root.path()) {
        assert!(!path.to_string_lossy().contains(id.as_str()), "{path:?}");
        let bytes = std::fs::read(&path).unwrap();
        for marker in ["secret", id.as_str(), "notes.md"] {
            assert!(
                !bytes
                    .windows(marker.len())
                    .any(|window| window == marker.as_bytes()),
                "{marker} leaked into {path:?}"
            );
        }
    }

    let stranger = machine_with("eve", &server.url, [9; 32], Limits::default());
    let refused = stranger
        .engine
        .pass(Scope::All, Direction::Both, None, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("refused the key"), "{refused}");
}

#[tokio::test]
async fn concurrent_offline_edits_become_a_fork_not_a_loss() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("shared", &["common start"]);
    dot.sync().await;
    esme.sync().await;

    dot.append(&id, "written on dot");
    esme.append(&id, "written on esme");
    dot.sync().await;
    let report = esme.sync().await;

    let [(original, fork)] = report.forked.as_slice() else {
        panic!("{report:?}");
    };
    assert_eq!(original, id.as_str());
    let fork = mush_session::SessionId::from(fork.as_str());
    assert_eq!(esme.texts(&id), ["common start", "written on dot"]);
    assert_eq!(esme.texts(&fork), ["common start", "written on esme"]);
    assert_eq!(
        esme.title(&fork).as_deref(),
        Some("shared (diverged on esme)")
    );
    assert_eq!(esme.store().load(&fork).unwrap().meta.id, fork);

    dot.sync().await;
    assert_eq!(dot.texts(&fork), ["common start", "written on esme"]);
    assert_eq!(dot.ids(), esme.ids());
    assert!(esme.sync().await.quiet());
}

#[tokio::test]
async fn a_session_open_here_is_never_written_under_its_writer() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("busy", &["first"]);
    dot.sync().await;
    esme.sync().await;
    let before = esme.history(&id);

    dot.append(&id, "second");
    dot.sync().await;
    let open = esme.store().acquire(&id).unwrap();
    let report = esme.sync().await;
    assert_eq!(report.waiting.len(), 1, "{report:?}");
    assert_eq!(esme.history(&id), before);

    drop(open);
    assert_eq!(esme.sync().await.pulled, [id.to_string()]);
    assert_eq!(esme.texts(&id), ["first", "second"]);
}

#[tokio::test]
async fn a_lease_held_on_one_machine_refuses_another_until_released() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("leased", &["start"]);
    dot.sync().await;
    esme.sync().await;

    let dot_lease = dot.store().acquire(&id).unwrap();
    let Opened::Claimed(held) = checkout(
        dot.engine.clone(),
        &dot_lease,
        Duration::from_secs(60),
        false,
    )
    .await
    .unwrap() else {
        panic!("dot should reach the server");
    };

    let esme_lease = esme.store().acquire(&id).unwrap();
    let refused = checkout(
        esme.engine.clone(),
        &esme_lease,
        Duration::from_secs(60),
        false,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&refused, CheckoutError::Held { holder, .. } if holder == "dot"),
        "{refused}"
    );

    dot.append(&id, "last words on dot");
    held.finish().await;
    drop(dot_lease);

    let Opened::Claimed(taken) = checkout(
        esme.engine.clone(),
        &esme_lease,
        Duration::from_secs(60),
        false,
    )
    .await
    .unwrap() else {
        panic!("esme should reach the server");
    };
    assert_eq!(esme.texts(&id), ["start", "last words on dot"]);
    taken.finish().await;
}

#[tokio::test]
async fn a_writer_that_lost_its_lease_ends_up_as_a_fork() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("contested", &["start"]);
    dot.sync().await;
    esme.sync().await;

    let esme_lease = esme.store().acquire(&id).unwrap();
    let Opened::Claimed(held) = checkout(
        esme.engine.clone(),
        &esme_lease,
        Duration::from_secs(60),
        false,
    )
    .await
    .unwrap() else {
        panic!("esme should reach the server");
    };

    dot.append(&id, "dot kept typing offline");
    let report = dot.sync().await;
    assert!(
        report
            .waiting
            .iter()
            .any(|(_, why)| why.contains("open on esme")),
        "{report:?}"
    );

    esme.append(&id, "esme took over");
    held.finish().await;
    drop(esme_lease);

    let report = dot.sync().await;
    assert_eq!(report.forked.len(), 1, "{report:?}");
    assert_eq!(dot.texts(&id), ["start", "esme took over"]);
    let fork = mush_session::SessionId::from(report.forked[0].1.as_str());
    assert_eq!(dot.texts(&fork), ["start", "dot kept typing offline"]);
}

#[tokio::test]
async fn slimming_rewrites_history_as_a_new_generation_and_keeps_the_old_one() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let mut session = mush_session::Session::new("test-model", "/tmp");
    session
        .conversation
        .append_message(mush_ai::types::Message::ToolResult(
            mush_ai::types::ToolResultMessage {
                tool_call_id: mush_ai::types::ToolCallId::from("call"),
                tool_name: mush_ai::types::ToolName::from("ipython"),
                content: vec![mush_ai::types::ToolResultContentPart::Text(
                    mush_ai::types::TextContent {
                        text: "shown to the model".into(),
                    },
                )],
                outcome: mush_ai::types::ToolOutcome::Success,
                timestamp_ms: mush_ai::types::Timestamp::zero(),
                details: Some(serde_json::json!({
                    "stdout": "x\n".repeat(mush_ai::types::DETAIL_TEXT_LIMIT)
                })),
            },
        ));
    session.push_message(crate::harness::user("after the bloat"));
    dot.store().save(&session).unwrap();
    let id = session.meta.id.clone();
    dot.sync().await;
    esme.sync().await;

    assert_eq!(dot.store().slim_history(&id, true).unwrap().capped, 1);
    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
    assert_eq!(esme.sync().await.pulled, [id.to_string()]);
    assert_eq!(esme.history(&id), dot.history(&id));
    assert_eq!(esme.texts(&id), ["after the bloat"]);

    let generations: std::collections::BTreeSet<String> = files_under(root.path())
        .into_iter()
        .filter_map(|path| {
            let text = path.to_string_lossy().into_owned();
            let (_, rest) = text.split_once("/chunks/")?;
            Some(rest.split('/').next()?.to_owned())
        })
        .collect();
    assert_eq!(generations, ["0".to_owned(), "1".to_owned()].into());
}

#[tokio::test]
async fn deleting_reaches_other_machines_but_the_server_keeps_the_data() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("doomed", &["keep me somewhere"]);
    dot.sync().await;
    esme.sync().await;
    let stored = files_under(root.path()).len();

    dot.store().delete(&id).unwrap();
    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
    assert_eq!(esme.sync().await.deleted, [id.to_string()]);
    assert!(esme.ids().is_empty());
    assert!(files_under(root.path()).len() >= stored);
    assert!(dot.sync().await.quiet());
    assert!(esme.sync().await.quiet());
}

#[tokio::test]
async fn edits_on_a_machine_that_missed_the_delete_bring_the_session_back() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("revived", &["start"]);
    dot.sync().await;
    esme.sync().await;

    dot.store().delete(&id).unwrap();
    dot.sync().await;
    esme.append(&id, "still working on it");
    esme.sync().await;
    dot.sync().await;

    assert_eq!(dot.texts(&id), ["start", "still working on it"]);
    assert_eq!(esme.texts(&id), ["start", "still working on it"]);
}

#[tokio::test]
async fn a_tampered_chunk_is_refused_and_nothing_local_changes() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("tamper", &["genuine"]);
    dot.sync().await;
    let chunk = files_under(root.path())
        .into_iter()
        .find(|path| path.to_string_lossy().contains("/chunks/"))
        .unwrap();
    let mut bytes = std::fs::read(&chunk).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x55;
    std::fs::write(&chunk, bytes).unwrap();

    let report = esme.sync().await;
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(esme.ids().is_empty());
    assert!(!esme.store().history_path(&id).exists());
}

#[tokio::test]
async fn losing_the_sync_bookkeeping_never_forks_identical_sessions() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("steady", &["one", "two"]);
    dot.sync().await;
    esme.sync().await;

    std::fs::remove_dir_all(esme.data().join("sync")).unwrap();
    let report = esme.sync().await;
    assert!(report.forked.is_empty(), "{report:?}");
    assert!(report.pushed.is_empty(), "{report:?}");
    dot.append(&id, "three");
    dot.sync().await;
    assert_eq!(esme.sync().await.pulled, [id.to_string()]);
    assert_eq!(esme.texts(&id), ["one", "two", "three"]);
}

#[tokio::test]
async fn a_replaced_server_is_refilled_from_the_machines() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let first = server_at(first_root.path()).await;
    let (dot, esme) = (machine("dot", &first), machine("esme", &first));
    let shared = dot.create("shared", &["both have this"]);
    dot.sync().await;
    esme.sync().await;
    let only_esme = esme.create("only esme", &["esme alone"]);
    drop(first);

    let second = server_at(second_root.path()).await;
    let (dot, esme) = (dot.moved_to(&second), esme.moved_to(&second));
    dot.sync().await;
    let report = esme.sync().await;
    assert!(report.forked.is_empty(), "{report:?}");
    assert_eq!(report.pushed, [only_esme.to_string()]);
    dot.sync().await;
    assert_eq!(dot.texts(&only_esme), ["esme alone"]);
    assert_eq!(esme.texts(&shared), ["both have this"]);
}

#[tokio::test]
async fn artifacts_follow_their_session_and_conflicting_edits_keep_both() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("notes", &["start"]);
    std::fs::create_dir_all(dot.artifact(&id, "harness")).unwrap();
    std::fs::write(dot.artifact(&id, "notes.md"), "v1").unwrap();
    std::fs::write(dot.artifact(&id, "harness/state.json"), "{}").unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "notes.md")).unwrap(),
        "v1"
    );
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "harness/state.json")).unwrap(),
        "{}"
    );

    std::fs::write(dot.artifact(&id, "notes.md"), "dot's v2").unwrap();
    std::fs::write(esme.artifact(&id, "notes.md"), "esme's v2").unwrap();
    dot.sync().await;
    let report = esme.sync().await;
    assert_eq!(report.sidecars.len(), 1, "{report:?}");
    assert!(report.forked.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "notes.md")).unwrap(),
        "dot's v2"
    );
    dot.sync().await;
    let sidecar = std::fs::read_dir(dot.artifact(&id, ""))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("sync-conflict-esme")
        })
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(sidecar.path()).unwrap(),
        "esme's v2"
    );
}

#[tokio::test]
async fn kernel_state_waits_until_the_session_closes_and_respects_its_cap() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let limits = Limits {
        kernel_state: 1024,
        ..Limits::default()
    };
    let dot = machine_with("dot", &server.url, crate::harness::MASTER, limits);
    let esme = machine_with("esme", &server.url, crate::harness::MASTER, limits);
    let id = dot.create("kernel", &["start"]);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), "pickled").unwrap();

    let open = dot.store().acquire(&id).unwrap();
    dot.sync().await;
    esme.sync().await;
    assert!(esme.store().history_path(&id).exists());
    assert!(!esme.artifact(&id, "kernel-state.pickle").exists());

    drop(open);
    dot.sync().await;
    esme.sync().await;
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "kernel-state.pickle")).unwrap(),
        "pickled"
    );

    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), vec![b'x'; 4096]).unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "kernel-state.pickle")).unwrap(),
        "pickled"
    );

    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), "stale").unwrap();
    let month_ago = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(dot.artifact(&id, "kernel-state.pickle"))
        .unwrap()
        .set_modified(month_ago)
        .unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "kernel-state.pickle")).unwrap(),
        "pickled"
    );
    assert!(dot.engine.queue().unwrap().up.is_empty());
    assert!(dot.sync().await.quiet());

    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), vec![b'y'; 4096]).unwrap();
    assert!(dot.sync().await.quiet());
    assert!(dot.engine.queue().unwrap().up.is_empty());
    let dot = dot.moved_to(&server);
    assert_eq!(dot.engine.queue().unwrap().up, [id.to_string()]);
    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
    let esme = esme.moved_to(&server);
    esme.sync().await;
    assert_eq!(
        std::fs::read(esme.artifact(&id, "kernel-state.pickle")).unwrap(),
        vec![b'y'; 4096]
    );
}

#[tokio::test]
async fn a_session_whose_kernel_state_is_too_big_to_sync_counts_as_synced() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let limits = Limits {
        kernel_state: 1024,
        ..Limits::default()
    };
    let dot = machine_with("dot", &server.url, crate::harness::MASTER, limits);
    let id = dot.create("big kernel", &["start"]);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), vec![b'k'; 4096]).unwrap();
    assert_eq!(dot.engine.queue().unwrap().up, [id.to_string()]);
    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
    assert!(dot.engine.queue().unwrap().up.is_empty());
    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), vec![b'k'; 8192]).unwrap();
    assert!(dot.sync().await.quiet());
    assert!(dot.engine.queue().unwrap().up.is_empty());
    std::fs::write(dot.artifact(&id, "notes.md"), "small").unwrap();
    assert_eq!(dot.engine.queue().unwrap().up, [id.to_string()]);
    assert_eq!(dot.sync().await.pushed, [id.to_string()]);
}

#[tokio::test]
async fn a_half_written_line_waits_until_it_is_complete() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("partial", &["whole"]);
    let path = dot.store().history_path(&id);
    let complete = std::fs::read(&path).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, b"{\"half").unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(esme.history(&id), complete);

    std::io::Write::write_all(&mut file, b"\":1}\n").unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(esme.history(&id), dot.history(&id));
}

#[tokio::test]
async fn opening_a_session_brings_its_family_up_to_date_first() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("parent", &["start"]);
    dot.sync().await;
    esme.sync().await;

    dot.append(&id, "more from dot");
    let mut child = mush_session::Session::new("test-model", "/tmp");
    child.meta.parent = Some(id.clone());
    child.push_message(crate::harness::user("child work"));
    dot.store().save(&child).unwrap();
    dot.sync().await;

    let lease = esme.store().acquire(&id).unwrap();
    let Opened::Claimed(opened) =
        checkout(esme.engine.clone(), &lease, Duration::from_secs(60), false)
            .await
            .unwrap()
    else {
        panic!("esme should reach the server");
    };
    assert_eq!(esme.texts(&id), ["start", "more from dot"]);
    assert_eq!(esme.texts(&child.meta.id), ["child work"]);
    opened.finish().await;
}

#[tokio::test]
async fn an_unreachable_server_opens_offline() {
    let dot = machine_with(
        "dot",
        "http://127.0.0.1:9",
        crate::harness::MASTER,
        Limits::default(),
    );
    let id = dot.create("offline", &["start"]);
    let lease = dot.store().acquire(&id).unwrap();
    let opened = checkout(dot.engine.clone(), &lease, Duration::from_secs(60), false)
        .await
        .unwrap();
    assert!(matches!(opened, Opened::Offline(_)), "{opened:?}");
    let error = dot
        .engine
        .pass(Scope::All, Direction::Both, None, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(error.offline(), "{error}");
}

#[tokio::test]
async fn a_validly_sealed_chunk_swapped_into_the_wrong_history_is_caught() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let dot = machine("dot", &server);
    let id = dot.create("swap", &["start"]);
    dot.sync().await;
    let before = dot.history(&id).len();
    dot.append(&id, "pay alice");
    dot.sync().await;
    let real = dot.history(&id)[before..].to_vec();
    let forged = String::from_utf8(real.clone())
        .unwrap()
        .replace("pay alice", "pay mallo");
    assert_eq!(forged.len(), real.len());

    let keys = mush_sync::Keys::from_master(&crate::harness::MASTER);
    let sid = keys.sid(id.as_str());
    let racing = keys
        .seal(
            &mush_sync::protocol::chunk_aad(&sid, 0, 1),
            forged.as_bytes(),
        )
        .unwrap();
    dot.engine
        .remote()
        .stage(&sid, "racer", 0, 1, racing)
        .await
        .unwrap();
    let files = files_under(root.path());
    let staged = files
        .iter()
        .find(|path| path.to_string_lossy().ends_with("/staging/racer-0-1"))
        .unwrap();
    let committed = files
        .iter()
        .find(|path| path.to_string_lossy().ends_with("/chunks/0/1"))
        .unwrap();
    std::fs::copy(staged, committed).unwrap();

    let val = machine("val", &server);
    let report = val.sync().await;
    assert!(
        report
            .failed
            .iter()
            .any(|(session, error)| session == id.as_str() && error.contains("does not match")),
        "{report:?}"
    );
    assert!(!val.store().history_path(&id).exists());
}

#[tokio::test]
async fn a_session_that_failed_is_retried_after_the_cursor_moves_on() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("flaky", &["first"]);
    dot.sync().await;
    let chunk = files_under(root.path())
        .into_iter()
        .find(|path| path.to_string_lossy().ends_with("/chunks/0/0"))
        .unwrap();
    let genuine = std::fs::read(&chunk).unwrap();
    std::fs::write(&chunk, b"garbage").unwrap();
    assert_eq!(esme.sync().await.failed.len(), 1);

    std::fs::write(&chunk, genuine).unwrap();
    let other = dot.create("unrelated", &["moves the cursor"]);
    dot.sync().await;
    let report = esme.sync().await;
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(esme.texts(&id), ["first"]);
    assert_eq!(esme.texts(&other), ["moves the cursor"]);
}

fn copy_session(
    from: &crate::harness::Machine,
    to: &crate::harness::Machine,
    id: &mush_session::SessionId,
) {
    let (source, target) = (from.store(), to.store());
    std::fs::create_dir_all(to.data().join("sessions")).unwrap();
    std::fs::copy(source.history_path(id), target.history_path(id)).unwrap();
    std::fs::copy(source.session_path(id), target.session_path(id)).unwrap();
}

#[tokio::test]
async fn hand_copied_sessions_join_sync_without_forking() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let older = dot.create("older on esme", &["one"]);
    let newer = dot.create("newer on esme", &["one"]);
    let same = dot.create("same everywhere", &["one"]);
    for id in [&older, &newer, &same] {
        copy_session(&dot, &esme, id);
    }
    dot.append(&older, "two from dot");
    esme.append(&newer, "two from esme");

    dot.sync().await;
    let report = esme.sync().await;
    assert!(report.forked.is_empty(), "{report:?}");
    assert_eq!(esme.texts(&older), ["one", "two from dot"]);
    assert_eq!(esme.texts(&newer), ["one", "two from esme"]);
    assert_eq!(report.pushed, [newer.to_string()], "{report:?}");
    assert_eq!(report.matched, [same.to_string()], "{report:?}");
    assert_eq!(report.pulled, [older.to_string()], "{report:?}");
    dot.sync().await;
    assert_eq!(dot.texts(&newer), ["one", "two from esme"]);
    assert_eq!(dot.history(&older), esme.history(&older));
    assert_eq!(dot.history(&newer), esme.history(&newer));
    assert!(esme.sync().await.quiet());
}

#[tokio::test]
async fn a_first_pass_cut_short_keeps_what_it_matched_when_it_resumes() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let first = dot.create("first", &["one"]);
    let second = dot.create("second", &["one"]);
    for id in [&first, &second] {
        copy_session(&dot, &esme, id);
    }
    dot.sync().await;

    let partial = esme
        .engine
        .pass(
            Scope::Sessions(vec![first.to_string()]),
            Direction::Both,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(partial.matched, [first.to_string()], "{partial:?}");
    let resumed = esme.sync().await;
    assert_eq!(resumed.matched, [second.to_string()], "{resumed:?}");
    assert!(resumed.forked.is_empty());
}

#[tokio::test]
async fn a_merged_artifact_that_ends_up_equal_to_the_server_copy_settles() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("shared log", &["one"]);
    copy_session(&dot, &esme, &id);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    std::fs::create_dir_all(esme.artifact(&id, "")).unwrap();
    std::fs::write(dot.artifact(&id, "log.jsonl"), "{\"a\":1}\n{\"b\":2}\n").unwrap();
    std::fs::write(esme.artifact(&id, "log.jsonl"), "{\"a\":1}\n").unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "log.jsonl")).unwrap(),
        "{\"a\":1}\n{\"b\":2}\n"
    );
    esme.sync().await;
    assert!(esme.engine.queue().unwrap().up.is_empty());
    assert!(esme.sync().await.quiet());
}

#[tokio::test]
async fn a_hand_copy_that_really_diverged_is_still_kept() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("split", &["one"]);
    copy_session(&dot, &esme, &id);
    dot.append(&id, "dot went this way");
    esme.append(&id, "esme went that way");
    dot.sync().await;
    let report = esme.sync().await;
    assert_eq!(report.forked.len(), 1, "{report:?}");
    let fork = mush_session::SessionId::from(report.forked[0].1.as_str());
    assert_eq!(esme.texts(&id), ["one", "dot went this way"]);
    assert_eq!(esme.texts(&fork), ["one", "esme went that way"]);
}

#[tokio::test]
async fn when_only_the_titles_differ_the_newer_wins_and_the_other_is_kept_beside_it() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("first title", &["one"]);
    copy_session(&dot, &esme, &id);
    let store = esme.store();
    let mut renamed = store.load(&id).unwrap();
    renamed.meta.title = Some("renamed on esme".into());
    renamed.meta.updated_at =
        mush_ai::types::Timestamp::from_ms(renamed.meta.updated_at.as_ms() + 5_000);
    store.save(&renamed).unwrap();

    dot.sync().await;
    let report = esme.sync().await;
    assert!(report.forked.is_empty(), "{report:?}");
    assert_eq!(esme.title(&id).as_deref(), Some("renamed on esme"));
    dot.sync().await;
    assert_eq!(dot.title(&id).as_deref(), Some("renamed on esme"));
    let kept: Vec<_> = std::fs::read_dir(esme.data().join("sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("sync-conflict"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let sidecar = std::fs::read_to_string(esme.data().join("sessions").join(&kept[0])).unwrap();
    assert!(sidecar.contains("first title"));
}

fn remember(machine: &crate::harness::Machine, id: &str, content: &str) {
    use mush_session::harness::{Action, Edit, Kind, Scope, State};
    let dir = machine.data().join("harness");
    let mut state = State::load(&dir, Scope::Global);
    let edit = Edit {
        action: if state.entry(Kind::Memory, id).is_some() {
            Action::Update
        } else {
            Action::Create
        },
        kind: Kind::Memory,
        id: Some(id.to_owned()),
        title: Some(id.to_owned()),
        content: Some(content.to_owned()),
        path: None,
        reference: None,
        arguments: None,
        metadata: None,
        reason: None,
    };
    state.upsert(&edit, Scope::Global, "agent").unwrap();
    state.save(&dir).unwrap();
}

fn forget(machine: &crate::harness::Machine, id: &str) {
    use mush_session::harness::{Kind, Scope, State};
    let dir = machine.data().join("harness");
    let mut state = State::load(&dir, Scope::Global);
    state.remove(Kind::Memory, id).unwrap();
    state.save(&dir).unwrap();
}

fn memories(machine: &crate::harness::Machine) -> Vec<(String, String)> {
    use mush_session::harness::{Kind, Scope, State};
    State::load(&machine.data().join("harness"), Scope::Global)
        .list(Some(Kind::Memory))
        .into_iter()
        .map(|entry| (entry.id.clone(), entry.content.clone()))
        .collect()
}

#[tokio::test]
async fn harness_memories_from_both_machines_merge_and_deletions_travel() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    remember(&dot, "jj", "use jj, not git");
    remember(&esme, "kaomoji", "rowan likes kaomoji");
    dot.sync().await;
    esme.sync().await;
    dot.sync().await;
    let both = vec![
        ("jj".to_owned(), "use jj, not git".to_owned()),
        ("kaomoji".to_owned(), "rowan likes kaomoji".to_owned()),
    ];
    assert_eq!(memories(&dot), both);
    assert_eq!(memories(&esme), both);

    forget(&dot, "kaomoji");
    remember(&esme, "jj", "use jj, never git");
    dot.sync().await;
    esme.sync().await;
    dot.sync().await;
    let expected = vec![("jj".to_owned(), "use jj, never git".to_owned())];
    assert_eq!(memories(&esme), expected);
    assert_eq!(memories(&dot), expected);
    assert!(dot.sync().await.quiet());
    assert!(esme.sync().await.quiet());
}

#[tokio::test]
async fn the_same_memory_edited_on_both_machines_keeps_both_edits() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    remember(&dot, "style", "short");
    dot.sync().await;
    esme.sync().await;
    remember(&dot, "style", "short and kind");
    remember(&esme, "style", "short and precise");
    dot.sync().await;
    esme.sync().await;
    dot.sync().await;
    let kept = memories(&esme);
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert_eq!(memories(&dot), kept);
    let contents: std::collections::BTreeSet<&str> =
        kept.iter().map(|(_, content)| content.as_str()).collect();
    assert_eq!(
        contents,
        ["short and kind", "short and precise"]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn a_sessions_own_harness_entries_merge_when_both_sides_add_some() {
    use mush_session::harness::{Action, Edit, Kind, Scope, State};
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("local harness", &["start"]);
    let add = |machine: &crate::harness::Machine, memory: &str| {
        let dir = machine.artifact(&id, "harness");
        let mut state = State::load(&dir, Scope::Local);
        let edit = Edit {
            action: Action::Create,
            kind: Kind::Memory,
            id: Some(memory.to_owned()),
            title: Some(memory.to_owned()),
            content: Some(memory.to_owned()),
            path: None,
            reference: None,
            arguments: None,
            metadata: None,
            reason: None,
        };
        state.upsert(&edit, Scope::Local, "agent").unwrap();
        state.save(&dir).unwrap();
    };
    add(&dot, "first");
    dot.sync().await;
    esme.sync().await;
    add(&dot, "from dot");
    add(&esme, "from esme");
    dot.sync().await;
    let report = esme.sync().await;
    assert!(report.sidecars.is_empty(), "{report:?}");
    dot.sync().await;
    for machine in [&dot, &esme] {
        let state = State::load(&machine.artifact(&id, "harness"), Scope::Local);
        let ids: Vec<&str> = state
            .list(Some(Kind::Memory))
            .into_iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids, ["first", "from dot", "from esme"]);
    }
}

#[tokio::test]
async fn a_conflicting_kernel_state_stays_beside_it_locally_and_is_not_uploaded() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("kernels", &["start"]);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), "base").unwrap();
    dot.sync().await;
    esme.sync().await;
    std::fs::write(dot.artifact(&id, "kernel-state.pickle"), "dot's kernel").unwrap();
    std::fs::write(esme.artifact(&id, "kernel-state.pickle"), "esme's kernel").unwrap();
    dot.sync().await;
    esme.sync().await;
    dot.sync().await;

    let beside = |machine: &crate::harness::Machine| -> Vec<String> {
        std::fs::read_dir(machine.artifact(&id, ""))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("sync-conflict"))
            .collect()
    };
    assert_eq!(beside(&esme).len(), 1);
    assert!(beside(&dot).is_empty());
    assert_eq!(
        std::fs::read_to_string(esme.artifact(&id, "kernel-state.pickle")).unwrap(),
        "dot's kernel"
    );
}
