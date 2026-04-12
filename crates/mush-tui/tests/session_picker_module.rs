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
