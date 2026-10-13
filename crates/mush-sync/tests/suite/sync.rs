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
