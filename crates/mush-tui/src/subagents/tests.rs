use super::*;
use crossterm::event::{KeyCode, KeyModifiers};
use mush_ai::types::{ImageMimeType, TokenCount};
use mush_supervisor::status::{Registration, Source};

#[tokio::test]
async fn reviewer_invalid_session_store_clears_previous_session_rows() {
    let (_dir, store) = fixture();
    let mut panel = SubagentPanel::default();
    panel.open = true;
    let now = Instant::now();
    settle(&mut panel, &store, now).await;
    assert!(!panel.snapshot.roots.is_empty());
    let invalid = RegistrationStore::new(
        std::path::PathBuf::from("/tmp"),
        "../untrusted-session".into(),
        std::path::PathBuf::from("/tmp"),
    );
    assert_eq!(invalid, Err(ReadError::Unsafe));
    panel.poll(invalid, now + Duration::from_secs(3)).await;
    assert_eq!(panel.snapshot.registration_error, Some(ReadError::Unsafe));
    assert!(
        panel.snapshot.roots.is_empty(),
        "previous session rows crossed an invalid store identity"
    );
}

fn toggle() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
}

#[test]
fn default_and_remapped_keys_preserve_draft_selection_attachments_and_modal() {
    for mode in [
        crate::app::AppMode::Insert,
        crate::app::AppMode::Normal,
        crate::app::AppMode::Visual,
        crate::app::AppMode::ModelPicker,
        crate::app::AppMode::ToolConfirm,
    ] {
        let mut app = crate::App::new("synthetic".into(), TokenCount::new(4096));
        app.input.text = "界 draft\ntext".into();
        app.input.cursor = 3;
        app.input.anchor = Some(4);
        app.input.images.push(crate::app::PendingImage {
            data: vec![1, 2, 3],
            mime_type: ImageMimeType::Png,
            dimensions: None,
        });
        app.interaction.mode = mode.clone();
        app.start_streaming();
        let text = app.input.text.clone();
        let cursor = app.input.cursor;
        let anchor = app.input.anchor;
        assert!(crate::input::handle_key(&mut app, toggle()).is_none());
        assert!(app.subagents.open);
        app.handle_paste("paste\u{13}");
        crate::input::handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        crate::input::handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.subagents.open);
        assert_eq!(app.interaction.mode, mode);
        assert_eq!(
            (app.input.text.as_str(), app.input.cursor, app.input.anchor),
            (text.as_str(), cursor, anchor)
        );
        assert_eq!(app.input.images[0].data, vec![1, 2, 3]);
        assert!(app.stream.active);
    }
    let mut app = crate::App::new("synthetic".into(), TokenCount::new(4096));
    let config: crate::keybinds::KeysConfig =
        toml::from_str("[shared]\ntoggle_subagents = 'ctrl+p'\n[subagents]\ncancel = 'ctrl+x'")
            .unwrap();
    assert!(app.keymap.override_scoped(&config.scoped()).is_empty());
    crate::input::handle_key(&mut app, toggle());
    assert!(!app.subagents.open);
    assert!(app.input.text.is_empty());
    let remapped = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
    crate::input::handle_key(&mut app, remapped);
    assert!(app.subagents.open);
    crate::input::handle_key(&mut app, remapped);
    assert!(!app.subagents.open);
    crate::input::handle_key(&mut app, remapped);
    crate::input::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
    );
    assert!(!app.subagents.open);
}

async fn settle(panel: &mut SubagentPanel, store: &RegistrationStore, now: Instant) {
    panel.poll(Ok(store.clone()), now).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while panel.pending.is_some() {
            tokio::task::yield_now().await;
            panel.poll(Ok(store.clone()), now).await;
        }
    })
    .await
    .unwrap();
}

fn fixture() -> (tempfile::TempDir, RegistrationStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("run");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join("worker")).unwrap();
    std::fs::write(
        root.join("supervisor.json"),
        r#"{"state":"running","pid":123,"started_at":"2026-09-14T12:00:00Z"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("manifest.json"),
        r#"{"workers":[{"name":"worker"}]}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("worker/state.json"),
        r#"{"state":"running","at":"2026-09-14T12:00:00Z"}"#,
    )
    .unwrap();
    let store =
        RegistrationStore::new(dir.path().into(), "synthetic".into(), dir.path().into()).unwrap();
    store
        .update(|registration| {
            registration.roots.push(Source {
                directory: root,
                review_queue: None,
                task_bindings: Vec::new(),
            });
            Ok(())
        })
        .unwrap();
    (dir, store)
}

#[tokio::test]
async fn polling_pauses_when_closed_and_isolates_instances_and_session_switches() {
    let (_dir, store) = fixture();
    let mut panel = SubagentPanel::default();
    let now = Instant::now();
    panel.poll(Ok(store.clone()), now).await;
    assert!(panel.pending.is_none());
    panel.open = true;
    settle(&mut panel, &store, now).await;
    assert_eq!(
        panel.snapshot.roots[0].tasks[0].run,
        mush_supervisor::status::RunState::Running
    );
    for _ in 0..200 {
        panel.poll(Ok(store.clone()), now).await;
        assert!(panel.pending.is_none());
    }
    panel.open = false;
    let later = now + Duration::from_secs(10);
    panel.poll(Ok(store.clone()), later).await;
    assert!(panel.pending.is_none());
    let (other_dir, other_store) = fixture();
    let other = RegistrationStore::new(
        other_dir.path().into(),
        "other-session".into(),
        other_dir.path().into(),
    )
    .unwrap();
    panel.open = true;
    settle(&mut panel, &other, later).await;
    assert!(panel.snapshot.roots.is_empty());
    settle(&mut panel, &store, later).await;
    assert_eq!(panel.snapshot.roots.len(), 1);
    let mut independent = SubagentPanel::default();
    independent.open = true;
    settle(&mut independent, &other_store, later).await;
    assert_ne!(
        independent.snapshot.roots[0].source,
        panel.snapshot.roots[0].source
    );
}

#[tokio::test]
async fn pending_refresh_does_not_accumulate_or_cross_registration_generations() {
    let (_dir, store) = fixture();
    let mut panel = SubagentPanel::default();
    panel.open = true;
    let now = Instant::now();
    settle(&mut panel, &store, now).await;
    let mut old = Loader::default();
    old.refresh(&store.load().unwrap(), Utc::now());
    let (release, wait) = std::sync::mpsc::channel();
    let identity = store.clone();
    let generation = panel.generation;
    panel.pending = Some(tokio::task::spawn_blocking(move || {
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        (identity, generation, old)
    }));
    let pending_id = panel.pending.as_ref().unwrap().id();
    panel.invalidate();
    store
        .update(|r| {
            r.roots.clear();
            Ok(())
        })
        .unwrap();
    for _ in 0..200 {
        panel.open = !panel.open;
        panel
            .poll(Ok(store.clone()), now + Duration::from_secs(10))
            .await;
        assert_eq!(panel.pending.as_ref().unwrap().id(), pending_id);
    }
    panel.open = false;
    release.send(()).unwrap();
    settle(&mut panel, &store, now + Duration::from_secs(10)).await;
    assert!(panel.snapshot.roots.is_empty());
    assert!(panel.pending.is_none());
    panel.open = true;
    settle(&mut panel, &store, now + Duration::from_secs(10)).await;
    assert!(panel.snapshot.roots.is_empty());
}

#[tokio::test]
async fn malformed_registration_retains_rows_and_selection_tracks_worker_identity() {
    let (_dir, store) = fixture();
    let mut panel = SubagentPanel::default();
    panel.open = true;
    let now = Instant::now();
    settle(&mut panel, &store, now).await;
    let root = panel.snapshot.roots[0].source.directory.clone();
    std::fs::create_dir(root.join("first")).unwrap();
    std::fs::write(root.join("first/state.json"), r#"{"state":"queued"}"#).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        r#"{"workers":[{"name":"first"},{"name":"worker"}]}"#,
    )
    .unwrap();
    settle(&mut panel, &store, now + Duration::from_secs(3)).await;
    assert_eq!(panel.selected, 1);
    std::fs::write(store.path(), "{").unwrap();
    settle(&mut panel, &store, now + Duration::from_secs(6)).await;
    assert_eq!(
        panel.snapshot.registration_error,
        Some(ReadError::Malformed)
    );
    assert_eq!(panel.snapshot.roots[0].tasks.len(), 2);
    std::fs::remove_file(store.path()).unwrap();
    settle(&mut panel, &store, now + Duration::from_secs(9)).await;
    assert_eq!(panel.snapshot.registration_error, Some(ReadError::Missing));
    assert_eq!(panel.snapshot.roots[0].tasks.len(), 2);
}

#[test]
fn rendering_scrolls_resizes_and_sanitises_data() {
    let mut panel = SubagentPanel::default();
    let mut loader = Loader::default();
    let (dir, store) = fixture();
    let mut registration: Registration = store.load().unwrap();
    let root = &registration.roots[0].directory;
    std::fs::remove_dir_all(root.join("worker")).unwrap();
    let workers: Vec<_> = (0..20)
        .map(|i| serde_json::json!({"name":format!("worker{i}")}))
        .collect();
    std::fs::write(
        root.join("manifest.json"),
        serde_json::json!({"workers":workers}).to_string(),
    )
    .unwrap();
    for i in 0..20 {
        let path = root.join(format!("worker{i}"));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("state.json"), serde_json::json!({"state":"blocked","at":"2026-09-14T12:00:00Z","detail":"needs dependency 界\u{1b}[31m"}).to_string()).unwrap();
    }
    let now = "2026-09-14T12:00:01Z".parse().unwrap();
    loader.refresh(&registration, now);
    panel.snapshot = loader.snapshot().clone();
    panel.open = true;
    for (width, height) in [(100, 24), (45, 15), (20, 8), (8, 3), (1, 1), (0, 0)] {
        let buffer = crate::test_support::draw_to_buffer(width, height, |frame| {
            render::render(frame, &panel, &crate::theme::Theme::default(), now)
        });
        assert!(!crate::test_support::buffer_to_string(&buffer).contains('\u{1b}'));
        if width == 100 {
            assert!(crate::test_support::buffer_to_string(&buffer).contains("blocked"));
        }
    }
    panel.handle_key(
        &KeyMap::default(),
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
    );
    assert_eq!(panel.selected, 19);
    panel.handle_key(
        &KeyMap::default(),
        KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
    );
    assert_eq!(panel.selected, 0);
    panel.visible_rows.set(4);
    panel.handle_key(
        &KeyMap::default(),
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
    );
    assert_eq!(panel.selected, 4);
    panel.handle_key(
        &KeyMap::default(),
        KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
    );
    assert_eq!(panel.selected, 3);
    registration.roots.clear();
    loader.refresh(&registration, now);
    panel.snapshot = loader.snapshot().clone();
    let buffer = crate::test_support::draw_to_buffer(80, 24, |frame| {
        render::render(frame, &panel, &crate::theme::Theme::default(), now)
    });
    assert!(crate::test_support::buffer_to_string(&buffer).contains("no supervisors registered"));
    drop(dir);
}

#[test]
fn styled_snapshots_cover_status_truth_and_details() {
    use mush_supervisor::status::{
        ReviewSchema, ReviewSource, ReviewState, RootStatus, RunState, TaskStatus,
    };
    let now: chrono::DateTime<Utc> = "2026-09-14T12:00:00Z".parse().unwrap();
    let cases = [
        (
            RunState::Queued,
            ReviewState::Missing,
            "queued task",
            "waiting for execution",
        ),
        (
            RunState::Running,
            ReviewState::Missing,
            "running 界",
            "streaming record",
        ),
        (
            RunState::GateWaiting,
            ReviewState::Missing,
            "gate",
            "coordinator handoff",
        ),
        (
            RunState::Blocked,
            ReviewState::Missing,
            "blocked dependency",
            "needs workspace-setup and an operator decision",
        ),
        (
            RunState::Finished,
            ReviewState::AwaitingReview,
            "implementation finished",
            "review remains pending",
        ),
        (
            RunState::Finished,
            ReviewState::Integrated,
            "integrated task",
            "operator queue has reviewed commit evidence",
        ),
        (
            RunState::Failed,
            ReviewState::Missing,
            "failed task",
            "exit 1",
        ),
        (
            RunState::Cancelled,
            ReviewState::Missing,
            "cancelled task",
            "cancelled",
        ),
        (
            RunState::Unknown,
            ReviewState::Unknown,
            "unknown task",
            "missing transition",
        ),
    ];
    let tasks = cases
        .into_iter()
        .enumerate()
        .map(|(index, (run, review, name, activity))| TaskStatus {
            worker: format!("worker-{index}"),
            name: name.into(),
            run,
            recorded_state: run.label().into(),
            round: 1,
            activity: activity.into(),
            headless_activity: None,
            headless_error: None,
            updated_at: Some(now - chrono::Duration::seconds(10)),
            read_error: None,
            review,
            review_updated_at: Some(now - chrono::Duration::seconds(3600)),
            review_detail: "operator synthetic / queue evidence".into(),
        })
        .collect();
    let mut panel = SubagentPanel::default();
    panel.open = true;
    panel.snapshot.roots.push(RootStatus {
        source: Source {
            directory: "/synthetic/run".into(),
            task_bindings: Vec::new(),
            review_queue: Some(ReviewSource {
                path: "/synthetic/queue.json".into(),
                schema: ReviewSchema::OperatorQueueV1,
            }),
        },
        tasks,
        read_error: None,
        review_error: None,
        supervisor: None,
        supervisor_error: None,
    });
    let theme = crate::theme::Theme::default();
    for (name, width, height, details, selected) in [
        ("wide", 112, 32, false, 0),
        ("narrow", 45, 16, false, 4),
        ("details", 60, 18, true, 3),
        ("tiny", 12, 4, false, 0),
    ] {
        panel.details = details;
        panel.selected = selected;
        let buffer = crate::test_support::draw_to_buffer(width, height, |frame| {
            render::render(frame, &panel, &theme, now)
        });
        let text = crate::test_support::buffer_to_string(&buffer);
        if name == "wide" {
            assert!(text.contains("awaiting review"));
            assert!(text.contains("integrated"));
            assert!(text.contains("cancelled"));
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| cell.style().fg == theme.picker_selected.fg
                        && cell.modifier == theme.picker_selected.add_modifier)
            );
        }
        if name == "details" {
            assert!(text.contains("needs workspace-setup"));
        }
        if let Some(directory) = std::env::var_os("MUSH_PANEL_SNAPSHOTS") {
            let directory = std::path::PathBuf::from(directory);
            let cells: Vec<_> = buffer.content.iter().enumerate().filter(|(_,cell)| cell.symbol() != " ").map(|(index,cell)| serde_json::json!({"x":index%usize::from(width),"y":index/usize::from(width),"text":cell.symbol(),"style":format!("{:?}",cell.style())})).collect();
            std::fs::write(directory.join(format!("snapshot-{name}.txt")), text).unwrap();
            std::fs::write(
                directory.join(format!("snapshot-{name}.json")),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"width":width,"height":height,"cells":cells}),
                )
                .unwrap(),
            )
            .unwrap();
        }
    }
}

#[tokio::test]
async fn unsafe_store_retires_inflight_snapshot_without_leaking_or_accumulating() {
    let (_dir, store) = fixture();
    let mut panel = SubagentPanel::default();
    panel.open = true;
    let now = Instant::now();
    settle(&mut panel, &store, now).await;
    panel.selected = 4;
    panel.details = true;
    panel.detail_scroll = 15;
    let mut old = Loader::default();
    old.refresh(&store.load().unwrap(), Utc::now());
    let identity = store.clone();
    let generation = panel.generation;
    let (release, wait) = std::sync::mpsc::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    panel.pending = Some(tokio::task::spawn_blocking(move || {
        started.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        (identity, generation, old)
    }));
    ready.await.unwrap();
    let pending_id = panel.pending.as_ref().unwrap().id();
    assert!(panel.poll(Err(ReadError::Unsafe), now).await);
    assert!(panel.snapshot.roots.is_empty());
    assert!(panel.store.is_none());
    assert!(panel.pending.is_none());
    assert_eq!(panel.selected, 0);
    assert!(!panel.details);
    assert_eq!(panel.detail_scroll, 0);
    for _ in 0..100 {
        panel
            .poll(Ok(store.clone()), now + Duration::from_secs(10))
            .await;
        assert!(panel.snapshot.roots.is_empty());
        assert!(panel.pending.is_none());
        assert_eq!(panel.retired.as_ref().unwrap().id(), pending_id);
        panel.poll(Err(ReadError::Unsafe), now).await;
    }
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while panel.retired.is_some() {
            tokio::task::yield_now().await;
            panel.poll(Err(ReadError::Unsafe), now).await;
        }
    })
    .await
    .unwrap();
    assert!(panel.snapshot.roots.is_empty());
    settle(&mut panel, &store, now + Duration::from_secs(10)).await;
    assert_eq!(panel.snapshot.roots.len(), 1);
}
