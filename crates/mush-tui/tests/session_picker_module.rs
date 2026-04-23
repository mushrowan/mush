use mush_ai::types::Timestamp;
use mush_session::{SessionId, SessionMeta};
use mush_tui::session_picker::{SessionPickerState, SessionScope, filtered_sessions};

#[test]
fn session_picker_module_exposes_state_and_filtering() {
    let sessions = vec![
        SessionMeta {
            id: SessionId::from("local"),
            title: Some("local work".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 1,
            cwd: "/tmp/project".into(),
        },
        SessionMeta {
            id: SessionId::from("remote"),
            title: Some("remote work".into()),
            model_id: "m".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 2,
            cwd: "/tmp/other".into(),
        },
    ];

    let mut picker = SessionPickerState::new(sessions, "/tmp/project".into());
    assert_eq!(picker.scope, SessionScope::ThisDir);
    assert_eq!(filtered_sessions(&picker).len(), 1);

    picker.scope = SessionScope::AllDirs;
    picker.filter = "remote".into();
    let filtered = filtered_sessions(&picker);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, SessionId::from("remote"));
}

#[test]
fn drain_incoming_appends_and_flips_loading() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel::<SessionMeta>();
    let mut picker = SessionPickerState::new_streaming(rx, "/tmp/project".into());
    assert!(picker.sessions.is_empty());
    assert!(picker.loading, "streaming picker starts in loading state");

    let older = SessionMeta {
        id: SessionId::from("a"),
        title: Some("old".into()),
        model_id: "m".into(),
        created_at: Timestamp::from_ms(100),
        updated_at: Timestamp::from_ms(100),
        message_count: 1,
        cwd: "/tmp/project".into(),
    };
    let newer = SessionMeta {
        id: SessionId::from("b"),
        title: Some("new".into()),
        model_id: "m".into(),
        created_at: Timestamp::from_ms(200),
        updated_at: Timestamp::from_ms(200),
        message_count: 1,
        cwd: "/tmp/project".into(),
    };
    tx.send(older).unwrap();
    tx.send(newer).unwrap();
    // small yield so producer side is visible to try_recv in every scheduler
    std::thread::sleep(Duration::from_millis(1));

    assert!(picker.drain_incoming());
    assert_eq!(picker.sessions.len(), 2);
    // sorted newest-first regardless of arrival order
    assert_eq!(picker.sessions[0].title.as_deref(), Some("new"));
    assert_eq!(picker.sessions[1].title.as_deref(), Some("old"));
    assert!(
        picker.loading,
        "sender still alive, loader should stay in loading state"
    );

    drop(tx);
    // second drain detects disconnect and flips loading off
    picker.drain_incoming();
    assert!(!picker.loading, "loader done after sender dropped");
}
