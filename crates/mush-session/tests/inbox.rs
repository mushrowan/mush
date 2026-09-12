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
    let dir = tempfile::tempdir().unwrap();
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
                "cross_process_saved_prefix_and_acknowledgment",
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
    let files: Vec<_> = fs::read_dir(dir.path().join("sessions")).unwrap().collect();
    assert_eq!(files.len(), 1);
}

#[test]
fn event_receipt_survives_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::new("fixture", "/tmp");
    let event = event(dir.path());
    assert!(append(&mut session.conversation, &event).unwrap());
    session.conversation.compact_at_leaf("summary".into(), 1);
    assert!(!append(&mut session.conversation, &event).unwrap());
}
