use mush_ai::types::Timestamp;
use mush_session::{SessionId, SessionMeta};
use mush_tui::session_picker::{SessionPickerState, SessionScope, filtered_sessions, listed};

#[test]
fn session_picker_module_exposes_state_and_filtering() {
    let sessions = vec![
        SessionMeta {
            title: Some("local work".into()),
            message_count: 1,
            ..meta("local", "/tmp/project")
        },
        SessionMeta {
            title: Some("remote work".into()),
            message_count: 2,
            ..meta("remote", "/tmp/other")
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
        title: Some("old".into()),
        created_at: Timestamp::from_ms(100),
        updated_at: Timestamp::from_ms(100),
        ..meta("a", "/tmp/project")
    };
    let newer = SessionMeta {
        title: Some("new".into()),
        created_at: Timestamp::from_ms(200),
        updated_at: Timestamp::from_ms(200),
        ..meta("b", "/tmp/project")
    };
    tx.send(older).unwrap();
    tx.send(newer).unwrap();
    std::thread::sleep(Duration::from_millis(1));

    assert!(picker.drain_incoming());
    assert_eq!(picker.sessions.len(), 2);
    assert_eq!(picker.sessions[0].title.as_deref(), Some("new"));
    assert_eq!(picker.sessions[1].title.as_deref(), Some("old"));
    assert!(
        picker.loading,
        "sender still alive, loader should stay in loading state"
    );

    drop(tx);
    assert!(
        picker.drain_incoming(),
        "loading state changes should request a redraw"
    );
    assert!(!picker.loading, "loader done after sender dropped");
}

fn meta(id: &str, cwd: &str) -> SessionMeta {
    SessionMeta {
        paused: None,
        rlm_max_depth: None,
        spawn_view: None,
        id: SessionId::from(id),
        title: Some(id.to_string()),
        model_id: "m".into(),
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        message_count: 1,
        cwd: cwd.into(),
        parent: None,
        subagent_model: None,
        accounts: Default::default(),
        lifecycle: mush_session::Lifecycle::Live,
    }
}

#[test]
fn arming_a_delete_picks_the_highlighted_visible_session() {
    let mut picker = SessionPickerState::new(
        vec![meta("a", "/p"), meta("b", "/elsewhere"), meta("c", "/p")],
        "/p".into(),
    );
    picker.selected = 1;

    assert!(picker.arm_delete());

    assert_eq!(
        picker.confirm_delete.as_ref().map(|m| m.id.as_str()),
        Some("c")
    );
    picker.cancel_delete();
    assert!(picker.confirm_delete.is_none());
}

#[test]
fn removing_the_last_row_moves_the_selection_up() {
    let mut picker = SessionPickerState::new(vec![meta("a", "/p"), meta("b", "/p")], "/p".into());
    picker.selected = 1;

    picker.remove(&SessionId::from("b"));

    assert_eq!(picker.selected, 0);
    assert_eq!(filtered_sessions(&picker).len(), 1);
}

#[test]
fn arming_with_nothing_visible_does_nothing() {
    let mut picker = SessionPickerState::new(vec![], "/p".into());

    assert!(!picker.arm_delete());
    assert!(picker.confirm_delete.is_none());
}

#[test]
fn the_agents_view_lists_neither_archived_nor_child_sessions() {
    let live = meta("live", "/tmp");
    let archived = SessionMeta {
        lifecycle: mush_session::Lifecycle::Archived,
        ..meta("archived", "/tmp")
    };
    let child = SessionMeta {
        parent: Some(SessionId::from("live")),
        ..meta("child", "/tmp")
    };
    assert!(listed(&live));
    assert!(!listed(&archived));
    assert!(!listed(&child));
}
