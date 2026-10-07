use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mush_ai::types::*;
use mush_session::{
    Session, SessionStore,
    inbox::{Binding, Consumer, Event, Inbox, TerminalReason, append},
};

fn event(path: &Path) -> Event {
    Event {
        event_id: "run.worker.1".into(),
        run_id: "run".into(),
        worker_id: "worker".into(),
        assignment_id: 1,
        round: 2,
        session_id: Some("worker-session".into()),
        reason: TerminalReason::Completed,
        report_path: path.join("private-report.json"),
        review: None,
    }
}

#[test]
fn cross_process_saved_prefix_and_acknowledgment() {
    if let Some(path) = std::env::var_os("MUSH_INBOX_TEST_CHILD") {
        let path = Path::new(&path);
        let inbox = Inbox::open(&path.join("inbox")).unwrap();
        let id: SessionId = inbox.binding().unwrap().session_id.into();
        let store = SessionStore::new(path.join("sessions"));
        let consumer = Consumer::open(&path.join("inbox"), id.clone(), path).unwrap();
        let mut session = store.load(&id).unwrap();
        let event = event(path);
        if std::env::var("MUSH_INBOX_TEST_PHASE").unwrap() == "append" {
            assert!(!consumer.acknowledge_saved(&event.event_id, &store).unwrap());
            assert!(append(&mut session.conversation, &event).unwrap());
            store.save(&session).unwrap();
        } else {
            assert!(!append(&mut session.conversation, &event).unwrap());
            assert!(consumer.acknowledge_saved(&event.event_id, &store).unwrap());
            assert!(consumer.acknowledge_saved(&event.event_id, &store).unwrap());
        }
        return;
    }
    let dir = crate::tempdir().unwrap();
    let mut session = Session::new("fixture-model", &dir.path().display().to_string());
    session.system_prompt = Some("saved system prefix".into());
    session.push_message(Message::User(
        UserMessage::builder()
            .content(UserContent::Text("original task".into()))
            .timestamp_ms(Timestamp::zero())
            .build(),
    ));
    session.push_message(Message::Assistant(
        AssistantMessage::builder()
            .model("fixture-model".into())
            .provider(Provider::Custom("fixture".into()))
            .api(Api::OpenaiResponses)
            .timestamp_ms(Timestamp::zero())
            .content(vec![AssistantContentPart::Text(TextContent {
                text: "original answer".into(),
            })])
            .build(),
    ));
    let prefix = serde_json::to_value(session.context()).unwrap();
    let tree_prefix = serde_json::to_value(session.conversation.entries()).unwrap();
    let id = session.meta.id.clone();
    let store = SessionStore::new(dir.path().join("sessions"));
    store.save(&session).unwrap();
    let inbox = Inbox::register(
        &dir.path().join("inbox"),
        Binding {
            session_id: id.to_string(),
            cwd: dir.path().to_owned(),
            workers: vec![dir.path().to_owned()],
        },
    )
    .unwrap();
    inbox.enqueue(event(dir.path()), dir.path()).unwrap();
    for phase in ["append", "ack", "ack"] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "inbox::cross_process_saved_prefix_and_acknowledgment",
                "--nocapture",
            ])
            .env("MUSH_INBOX_TEST_CHILD", dir.path())
            .env("MUSH_INBOX_TEST_PHASE", phase)
            .env("HOME", dir.path())
            .env_remove("MUSH_ORCHESTRATOR_INBOX")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("inbox child timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if phase == "append" {
            assert_eq!(inbox.pending().unwrap().len(), 1);
        }
    }
    let saved = store.load(&id).unwrap();
    assert_eq!(saved.meta.id, id);
    assert_eq!(saved.system_prompt, session.system_prompt);
    assert_eq!(serde_json::to_value(&saved.context()[..2]).unwrap(), prefix);
    assert_eq!(
        serde_json::to_value(&saved.conversation.entries()[..2]).unwrap(),
        tree_prefix
    );
    assert_eq!(saved.context().len(), 3);
    assert!(inbox.pending().unwrap().is_empty());
    let mut files: Vec<_> = fs::read_dir(dir.path().join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files, [format!("{id}.history.jsonl"), format!("{id}.json")]);
}

#[test]
fn event_receipt_survives_compaction() {
    let dir = crate::tempdir().unwrap();
    let mut session = Session::new("fixture", "/tmp");
    let event = event(dir.path());
    assert!(append(&mut session.conversation, &event).unwrap());
    session.conversation.compact_at_leaf("summary".into(), 1);
    assert!(!append(&mut session.conversation, &event).unwrap());
}

#[test]
fn review_payload_survives_parent_reconnect_before_ack() {
    let dir = crate::tempdir().unwrap();
    let mut session = Session::new("fixture", &dir.path().display().to_string());
    let id = session.meta.id.clone();
    let store = SessionStore::new(dir.path().join("sessions"));
    store.save(&session).unwrap();
    let inbox = Inbox::register(
        &dir.path().join("inbox"),
        Binding {
            session_id: id.to_string(),
            cwd: dir.path().into(),
            workers: vec![dir.path().into()],
        },
    )
    .unwrap();
    let mut event = event(dir.path());
    event.review = Some(mush_supervisor::review::Notice {
        pairing: None,
        verdict: mush_supervisor::review::Verdict {
            task: "slice-task".into(),
            version: 1,
            assignment: "slice-review".into(),
            generation: 1,
            candidate: "1".repeat(40),
            reviewer: mush_supervisor::review::Identity {
                run_id: "review".into(),
                worker: "reviewer".into(),
                assignment: 1,
                round: 1,
                session: "review-session".into(),
            },
            result: mush_supervisor::review::VerdictResult::Blocked,
            complete: false,
            checks: vec![],
            findings: vec![],
            blockers: vec!["choose correction scope".into()],
            evidence: vec![],
            regression_commit: None,
        },
        capture_sha256: "a".repeat(64),
        producer_action: "not_rejected".into(),
        evidence_directory: dir.path().join("capture"),
    });
    inbox.enqueue(event.clone(), dir.path()).unwrap();
    let consumer = Consumer::open(&dir.path().join("inbox"), id.clone(), dir.path()).unwrap();
    assert_eq!(consumer.pending().unwrap(), vec![event.clone()]);
    assert!(!consumer.acknowledge_saved(&event.event_id, &store).unwrap());
    append(&mut session.conversation, &event).unwrap();
    store.save(&session).unwrap();
    drop(consumer);
    let consumer = Consumer::open(&dir.path().join("inbox"), id.clone(), dir.path()).unwrap();
    assert_eq!(consumer.pending().unwrap(), vec![event.clone()]);
    let loaded = store.load(&id).unwrap();
    let Message::Event(message) = loaded.context().last().unwrap().clone() else {
        panic!("missing event")
    };
    assert!(message.content.contains("choose correction scope"));
    assert!(message.content.contains("no new tool or scope permissions"));
    assert!(consumer.acknowledge_saved(&event.event_id, &store).unwrap());
    assert!(inbox.pending().unwrap().is_empty());
}

#[test]
fn supervised_session_lease_excludes_another_launcher() {
    let dir = crate::tempdir().unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let session = Session::new("fixture", &dir.path().display().to_string());
    let store = SessionStore::new(dir.path().to_owned());
    let lock = mush_supervisor::admission::session_lock(dir.path(), session.meta.id.as_str(), None)
        .unwrap();
    assert!(store.is_leased(&session.meta.id));
    assert!(matches!(
        store.acquire(&session.meta.id),
        Err(mush_session::store::StoreError::Busy(_))
    ));
    drop(lock);
    let lease = store.acquire(&session.meta.id).unwrap();
    assert!(store.is_leased(&session.meta.id));
    drop(lease);
    assert!(!store.is_leased(&session.meta.id));
}

#[test]
fn a_fresh_session_is_not_leased_until_someone_holds_it() {
    let dir = crate::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_owned());
    let id = mush_ai::types::SessionId::new();
    assert!(!store.is_leased(&id));
    let first = store.acquire(&id).unwrap();
    assert_eq!(first.id(), &id);
    assert!(matches!(
        store.acquire(&id),
        Err(mush_session::store::StoreError::Busy(_))
    ));
}
