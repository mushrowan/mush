use super::*;
use serde_json::json;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn rereviewer_unscoped_worker_is_ambiguous_across_registered_supervisors() {
    let (dir, mut registration) = fixture();
    let (_other, other) = fixture();
    let queue = dir.path().join("queue.json");
    let review = ReviewSource {
        path: queue.clone(),
        schema: ReviewSchema::OperatorQueueV1,
    };
    registration.roots[0].review_queue = Some(review.clone());
    let mut second = other.roots[0].clone();
    second.review_queue = Some(review);
    registration.roots.push(second);
    fs::write(&queue, json!({"updated_at":now(), "root_owner":"operator", "items":[
        {"id":"one-task", "worker":"one", "state":"accepted", "accepted":true, "reviewed":"one-task-review.json"}
    ]}).to_string()).unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    for root in &loader.snapshot.roots {
        assert_eq!(
            root.tasks[0].review,
            ReviewState::Unknown,
            "one unscoped task was associated with duplicate worker roles in separate supervisors"
        );
    }
}

#[test]
fn reviewer_duplicate_operator_roles_do_not_discard_unrelated_worker_reviews() {
    let (dir, mut registration) = fixture();
    let queue = dir.path().join("queue.json");
    registration.roots[0].review_queue = Some(ReviewSource {
        path: queue.clone(),
        schema: ReviewSchema::OperatorQueueV1,
    });
    fs::write(&queue, json!({"updated_at":now(), "root_owner":"interactive_root", "items":[
        {"id":"worker-task", "worker":"one", "state":"accepted", "accepted":true, "reviewed":"fixture-receipt.json"},
        {"id":"installed-pipeline", "worker":"interactive_root", "state":"accepted", "accepted":true, "reviewed":"pipeline.json"},
        {"id":"npm-packaging", "worker":"interactive_root", "state":"independent_review", "accepted":null}
    ]}).to_string()).unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].review_error, None);
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Accepted
    );
}

#[test]
fn reviewer_stale_queue_cache_does_not_transfer_to_a_new_supervisor_root() {
    let (dir, mut registration) = fixture();
    let (_other, other) = fixture();
    let queue = dir.path().join("queue.json");
    registration.roots[0].review_queue = Some(ReviewSource {
        path: queue.clone(),
        schema: ReviewSchema::OperatorQueueV1,
    });
    fs::write(
        &queue,
        json!({"updated_at":now(), "root_owner":"operator", "items":[
            {"id":"old-root-task", "worker":"one", "state":"integrated", "accepted":true,
             "reviewed":"old-root.json", "integrated":"0123456789012345678901234567890123456789"}
        ]})
        .to_string(),
    )
    .unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Integrated
    );
    registration.roots[0].directory = other.roots[0].directory.clone();
    fs::write(&queue, "{").unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].review_error,
        Some(ReadError::Malformed)
    );
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Missing
    );
}

#[test]
fn reviewer_shared_queue_requires_a_supervisor_binding_for_duplicate_worker_roles() {
    let (dir, mut registration) = fixture();
    let (_other, other) = fixture();
    let queue = dir.path().join("queue.json");
    let review = ReviewSource {
        path: queue.clone(),
        schema: ReviewSchema::OperatorQueueV1,
    };
    registration.roots[0].review_queue = Some(review.clone());
    let mut second = other.roots[0].clone();
    second.review_queue = Some(review);
    registration.roots.push(second);
    fs::write(&queue, json!({"updated_at":now(), "root_owner":"operator", "items":[
        {"id":"first-supervisor-one", "worker":"one", "supervisor_root":registration.roots[0].directory,
         "state":"accepted", "accepted":true, "reviewed":"first-supervisor-review.json"}
    ]}).to_string()).unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Accepted
    );
    assert_eq!(
        loader.snapshot.roots[1].tasks[0].review,
        ReviewState::Missing
    );
}

#[test]
fn reviewer_open_directory_survives_path_replacement_without_following_symlink() {
    let (dir, registration) = fixture();
    let root = &registration.roots[0].directory;
    let pinned = files::Directory::open(root).unwrap();
    let old = dir.path().join("old");
    fs::rename(root, &old).unwrap();
    let foreign = dir.path().join("foreign");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("supervisor.json"), r#"{"state":"foreign"}"#).unwrap();
    symlink(&foreign, root).unwrap();
    let state = pinned
        .json::<SupervisorState>("supervisor.json", 4096, &mut files::Budget::new())
        .unwrap();
    assert_eq!(state.state, "running");
    assert!(matches!(
        files::Directory::open(root),
        Err(ReadError::Unsafe)
    ));
}

fn now() -> DateTime<Utc> {
    "2026-09-14T12:00:00Z".parse().unwrap()
}

fn fixture() -> (tempfile::TempDir, Registration) {
    let dir = crate::tempdir().unwrap();
    let root = dir.path().join("run");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("one")).unwrap();
    fs::create_dir(root.join("two")).unwrap();
    fs::write(
        root.join("supervisor.json"),
        r#"{"state":"running","pid":123,"started_at":"2026-09-14T12:00:00Z"}"#,
    )
    .unwrap();
    fs::write(
        root.join("manifest.json"),
        r#"{"workers":[{"name":"one"},{"name":"two"}]}"#,
    )
    .unwrap();
    for worker in ["one", "two"] {
        fs::write(
            root.join(worker).join("state.json"),
            json!({"state":"queued","at":now(),"round":1}).to_string(),
        )
        .unwrap();
    }
    let registration = Registration {
        version: 1,
        session_id: "session-one".into(),
        cwd: dir.path().into(),
        roots: vec![Source {
            directory: root,
            review_queue: None,
            task_bindings: Vec::new(),
        }],
    };
    (dir, registration)
}

#[test]
fn transitions_preserve_execution_and_review_truth() {
    let (_dir, mut registration) = fixture();
    let root = registration.roots[0].directory.clone();
    let mut loader = Loader::default();
    for (state, expected) in [
        ("queued", RunState::Queued),
        ("waiting", RunState::GateWaiting),
        ("running", RunState::Running),
        ("blocked", RunState::Blocked),
        ("awaiting_input", RunState::AwaitingInput),
        ("done", RunState::Finished),
        ("error", RunState::Failed),
        ("cancelled", RunState::Cancelled),
        ("alien", RunState::Unknown),
    ] {
        fs::write(
            root.join("one/state.json"),
            json!({"state":state,"at":now(),"detail":"needs dependency two"}).to_string(),
        )
        .unwrap();
        loader.refresh(&registration, now());
        let task = &loader.snapshot.roots[0].tasks[0];
        assert_eq!(task.run, expected);
        assert_eq!(task.review, ReviewState::Missing);
        assert_eq!(task.activity, "needs dependency two");
        assert_eq!(task.age_seconds(now()), Some(0));
    }
    let queue = root.join("queue.json");
    registration.roots[0].review_queue = Some(ReviewSource {
        path: queue.clone(),
        schema: ReviewSchema::OperatorQueueV1,
    });
    for (state, accepted, reviewed, integrated, expected) in [
        (
            "awaiting_review",
            None,
            None,
            None,
            ReviewState::AwaitingReview,
        ),
        ("integrated", None, None, None, ReviewState::Unknown),
        (
            "integrated",
            Some(true),
            Some("receipt.json"),
            Some("root_authored"),
            ReviewState::Unknown,
        ),
        (
            "integrated",
            Some(true),
            Some("receipt.json"),
            Some("0123456789012345678901234567890123456789"),
            ReviewState::Integrated,
        ),
        (
            "accepted",
            Some(true),
            Some("receipt.json"),
            None,
            ReviewState::Accepted,
        ),
        (
            "rejected",
            Some(false),
            Some("receipt.json"),
            None,
            ReviewState::Rejected,
        ),
    ] {
        fs::write(&queue, json!({"root_owner":"operator", "updated_at":"2026-09-14T11:00:00Z", "items":[{"id":"task title","worker":"one","state":state,"accepted":accepted,"reviewed":reviewed,"integrated":integrated}]}).to_string()).unwrap();
        loader.refresh(&registration, now());
        let task = &loader.snapshot.roots[0].tasks[0];
        assert_eq!(task.run, RunState::Unknown);
        assert_eq!(task.review, expected);
        assert!(task.review_predates_worker());
        assert_eq!(
            loader.snapshot.roots[0].tasks[1].review,
            ReviewState::Missing
        );
    }
    fs::write(queue, "{").unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].review_error,
        Some(ReadError::Malformed)
    );
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Rejected
    );
}

#[test]
fn damaged_files_retain_last_good_and_other_workers() {
    let (_dir, registration) = fixture();
    let root = &registration.roots[0].directory;
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    for content in ["", "{", "[]", "not json", &"x".repeat(16 * 1024 + 1)] {
        fs::write(root.join("one/state.json"), content).unwrap();
        loader.refresh(&registration, now());
        let tasks = &loader.snapshot.roots[0].tasks;
        assert_eq!(tasks[0].run, RunState::Queued);
        assert!(tasks[0].read_error.is_some());
        assert_eq!(tasks[0].updated_at, Some(now()));
        assert_eq!(tasks[1].read_error, None);
    }
    fs::remove_file(root.join("one/state.json")).unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Missing)
    );
    fs::write(
        root.join("replacement"),
        json!({"state":"done", "at":now()}).to_string(),
    )
    .unwrap();
    fs::rename(root.join("replacement"), root.join("one/state.json")).unwrap();
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].tasks[0].run, RunState::Finished);
    assert_eq!(loader.snapshot.roots[0].tasks[0].read_error, None);
    fs::rename(root, root.with_file_name("removed-run")).unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].read_error,
        Some(ReadError::Missing)
    );
    assert_eq!(loader.snapshot.roots[0].tasks.len(), 2);
}

#[test]
fn file_boundaries_reject_links_permissions_fifos_and_traversal() {
    let (dir, mut registration) = fixture();
    let root = registration.roots[0].directory.clone();
    let state = root.join("one/state.json");
    let outside = dir.path().join("outside.json");
    fs::write(&outside, r#"{"state":"done"}"#).unwrap();
    fs::remove_file(&state).unwrap();
    symlink(&outside, &state).unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Unsafe)
    );
    fs::remove_file(&state).unwrap();
    fs::hard_link(&outside, &state).unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Unsafe)
    );
    fs::remove_file(&state).unwrap();
    mkfifo(&state);
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Unsafe)
    );
    fs::remove_file(&state).unwrap();
    fs::write(&state, r#"{"state":"done"}"#).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o666)).unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Unsafe)
    );
    fs::rename(root.join("one"), dir.path().join("old-one")).unwrap();
    symlink(dir.path().join("old-one"), root.join("one")).unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Unsafe)
    );
    fs::write(
        root.join("manifest.json"),
        r#"{"workers":[{"name":"../outside"},{"name":"two"}]}"#,
    )
    .unwrap();
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].tasks.len(), 2);
    assert_eq!(loader.snapshot.roots[0].tasks[1].worker, "two");
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Unsafe)
    );
    registration.roots[0].directory = root.join("../run");
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].read_error, Some(ReadError::Unsafe));
}

#[test]
fn registration_is_persistent_bounded_and_session_bound() {
    let (dir, registration) = fixture();
    let store = RegistrationStore::new(
        dir.path().into(),
        registration.session_id.clone(),
        dir.path().into(),
    )
    .unwrap();
    assert_eq!(store.load().unwrap_err(), ReadError::Missing);
    store
        .update(|r| {
            r.roots = registration.roots.clone();
            Ok(())
        })
        .unwrap();
    assert_eq!(store.load().unwrap(), registration);
    let other =
        RegistrationStore::new(dir.path().into(), "other".into(), dir.path().into()).unwrap();
    assert_eq!(other.load().unwrap_err(), ReadError::Missing);
    let wrong_cwd = RegistrationStore::new(
        dir.path().into(),
        registration.session_id.clone(),
        dir.path().join("other"),
    )
    .unwrap();
    assert_eq!(wrong_cwd.load().unwrap_err(), ReadError::Identity);
    assert!(
        RegistrationStore::new(dir.path().into(), "../escape".into(), dir.path().into()).is_err()
    );
    assert_eq!(
        store
            .update(|r| {
                r.roots = vec![registration.roots[0].clone(); MAX_ROOTS + 1];
                Ok(())
            })
            .unwrap_err(),
        ReadError::Oversized
    );
    assert_eq!(store.load().unwrap(), registration);
    fs::write(store.path(), "{").unwrap();
    assert_eq!(
        store
            .update(|r| {
                r.roots.clear();
                Ok(())
            })
            .unwrap_err(),
        ReadError::Malformed
    );
}

#[test]
fn stale_empty_unknown_and_control_text_are_explicit() {
    let (_dir, mut registration) = fixture();
    fs::write(registration.roots[0].directory.join("one/state.json"), json!({"state":"running","at":now() - chrono::Duration::seconds(500),"detail":"\u{1b}]52;c;secret\u{7}\n\r\u{202e}text"}).to_string()).unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    let task = &loader.snapshot.roots[0].tasks[0];
    assert!(task.record_is_old(now()));
    assert_eq!(task.run, RunState::Running);
    assert!(!task.activity.chars().any(char::is_control));
    assert!(!task.activity.contains('\u{202e}'));
    assert_eq!(safe_text(&"界".repeat(900)).chars().count(), MAX_TEXT);
    registration.roots.clear();
    loader.refresh(&registration, now());
    assert!(loader.snapshot.roots.is_empty());
}

#[test]
fn root_failures_and_limits_do_not_hide_other_roots() {
    let (_dir, mut registration) = fixture();
    let (_other, other) = fixture();
    registration.roots.push(other.roots[0].clone());
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    for i in 0..65 {
        fs::write(
            registration.roots[0].directory.join(format!("entry-{i}")),
            "",
        )
        .unwrap();
    }
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].read_error,
        Some(ReadError::Oversized)
    );
    assert_eq!(loader.snapshot.roots[0].tasks.len(), 2);
    assert_eq!(loader.snapshot.roots[1].read_error, None);
    let root = &registration.roots[1].directory;
    for i in 0..MAX_WORKERS + 1 {
        let worker = root.join(format!("worker-{i}"));
        fs::create_dir(&worker).unwrap();
        fs::write(worker.join("state.json"), r#"{"state":"queued"}"#).unwrap();
    }
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[1].read_error,
        Some(ReadError::Oversized)
    );
    registration.session_id = "other-session".into();
    loader.refresh(&registration, now());
    assert!(loader.snapshot.roots[0].tasks.is_empty());
    assert_eq!(loader.snapshot.roots[1].tasks.len(), MAX_WORKERS);
    registration.roots = vec![registration.roots[0].clone(); MAX_ROOTS + 1];
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots.len(), MAX_ROOTS);
    assert_eq!(
        loader.snapshot.registration_error,
        Some(ReadError::Oversized)
    );
}

#[test]
fn exact_terminal_reasons_and_headless_evidence_do_not_claim_liveness() {
    let (_dir, registration) = fixture();
    let root = &registration.roots[0].directory;
    let mut loader = Loader::default();
    for (detail, expected) in [
        ("cancelled", RunState::Cancelled),
        ("deadline", RunState::Cancelled),
        ("round limit", RunState::Failed),
        ("something else", RunState::Unknown),
    ] {
        fs::write(
            root.join("one/state.json"),
            json!({"state":"stopped","detail":detail}).to_string(),
        )
        .unwrap();
        loader.refresh(&registration, now());
        assert_eq!(loader.snapshot.roots[0].tasks[0].run, expected);
    }
    fs::write(
        root.join("one/state.json"),
        json!({"state":"running","round":1,"at":now()}).to_string(),
    )
    .unwrap();
    fs::write(
        root.join("one/round-001.json"),
        r#"{"state":"completed","session_id":"synthetic"}"#,
    )
    .unwrap();
    loader.refresh(&registration, now());
    let task = &loader.snapshot.roots[0].tasks[0];
    assert_eq!(task.run, RunState::Running);
    assert_eq!(task.review, ReviewState::Missing);
    assert!(
        task.headless_activity
            .as_ref()
            .unwrap()
            .contains("completed")
    );
    fs::remove_file(root.join("one/round-001.json")).unwrap();
    loader.refresh(&registration, now());
    let task = &loader.snapshot.roots[0].tasks[0];
    assert_eq!(task.headless_error, Some(ReadError::Missing));
    assert!(
        task.headless_activity
            .as_ref()
            .unwrap()
            .contains("completed")
    );
    let event = crate::inbox::Event {
        event_id: "run.one.1".into(),
        run_id: "run".into(),
        worker_id: "one".into(),
        assignment_id: 1,
        round: 1,
        session_id: None,
        reason: TerminalReason::Cancelled,
        report_path: "/synthetic/report.json".into(),
        review: None,
    };
    fs::write(
        root.join("one/state.json"),
        json!({"state":"error","round":1,"at":now(),"terminal_event":event}).to_string(),
    )
    .unwrap();
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].tasks[0].run, RunState::Cancelled);
    assert_eq!(loader.snapshot.roots[0].tasks[0].headless_activity, None);
}

#[test]
fn request_budget_and_future_timestamps_are_bounded() {
    let (_dir, registration) = fixture();
    let directory = files::Directory::open(&registration.roots[0].directory).unwrap();
    let mut budget = files::Budget::new();
    for _ in 0..MAX_FILES {
        directory
            .json::<SupervisorState>("supervisor.json", 4096, &mut budget)
            .unwrap();
    }
    assert!(matches!(
        directory.json::<SupervisorState>("supervisor.json", 4096, &mut budget),
        Err(ReadError::Oversized)
    ));
    fs::write(
        registration.roots[0].directory.join("one/state.json"),
        json!({"state":"running","at":now()+chrono::Duration::seconds(1)}).to_string(),
    )
    .unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].read_error,
        Some(ReadError::Identity)
    );
}

#[test]
fn registration_rejects_symlink_fifo_and_writable_fixtures() {
    let (dir, registration) = fixture();
    let store = RegistrationStore::new(
        dir.path().into(),
        registration.session_id.clone(),
        dir.path().into(),
    )
    .unwrap();
    store
        .update(|r| {
            *r = registration.clone();
            Ok(())
        })
        .unwrap();
    let copy = dir.path().join("copy.json");
    fs::copy(store.path(), &copy).unwrap();
    fs::remove_file(store.path()).unwrap();
    symlink(&copy, store.path()).unwrap();
    assert_eq!(store.load().unwrap_err(), ReadError::Unsafe);
    assert_eq!(store.update(|_| Ok(())).unwrap_err(), ReadError::Unsafe);
    fs::remove_file(store.path()).unwrap();
    mkfifo(store.path());
    assert_eq!(store.load().unwrap_err(), ReadError::Unsafe);
    fs::remove_file(store.path()).unwrap();
    fs::copy(&copy, store.path()).unwrap();
    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(store.load().unwrap_err(), ReadError::Unsafe);
    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let parent = store.path().parent().unwrap().to_path_buf();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(store.load().unwrap_err(), ReadError::Unsafe);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let mut loader = Loader::default();
    let mut empty = registration.clone();
    fs::remove_dir_all(empty.roots[0].directory.join("one")).unwrap();
    fs::remove_dir_all(empty.roots[0].directory.join("two")).unwrap();
    loader.refresh(&empty, now());
    assert!(loader.snapshot.roots[0].tasks.is_empty());
    let link = dir.path().join("linked-root");
    symlink(&empty.roots[0].directory, &link).unwrap();
    empty.roots[0].directory = link;
    loader.refresh(&empty, now());
    assert_eq!(loader.snapshot.roots[0].read_error, Some(ReadError::Unsafe));
}

#[test]
fn durable_sources_do_not_open_prompts_manifests_or_sessions() {
    let (_dir, registration) = fixture();
    let root = &registration.roots[0].directory;
    fs::create_dir(root.join("inbox")).unwrap();
    fs::remove_file(root.join("manifest.json")).unwrap();
    for path in [
        root.join("manifest.json"),
        root.join("one/prompt.md"),
        root.join("one/session.jsonl"),
    ] {
        mkfifo(path);
    }
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].tasks.len(), 2);
    assert_eq!(
        loader.snapshot.roots[0].supervisor.as_ref().unwrap().pid,
        Some(123)
    );
    fs::write(root.join("supervisor.json"), "{").unwrap();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].supervisor.as_ref().unwrap().pid,
        Some(123)
    );
    assert_eq!(
        loader.snapshot.roots[0].supervisor_error,
        Some(ReadError::Malformed)
    );
    assert_eq!(loader.snapshot.roots[0].tasks[0].read_error, None);
}

fn review_queue(path: &std::path::Path, items: serde_json::Value) {
    fs::write(
        path,
        json!({"updated_at":now(),"root_owner":"synthetic","items":items}).to_string(),
    )
    .unwrap();
}

fn integrated_item(id: &str, worker: &str) -> serde_json::Value {
    json!({"id":id,"worker":worker,"state":"integrated","accepted":true,"reviewed":"synthetic-receipt","integrated":"0123456789012345678901234567890123456789"})
}

#[test]
fn explicit_bindings_and_scoped_queue_items_isolate_reused_worker_roles() {
    let (dir, mut registration) = fixture();
    let (_other, other) = fixture();
    registration.roots.push(other.roots[0].clone());
    let path = dir.path().join("queue.json");
    for source in &mut registration.roots {
        source.review_queue = Some(ReviewSource {
            path: path.clone(),
            schema: ReviewSchema::OperatorQueueV1,
        });
    }
    review_queue(
        &path,
        json!([integrated_item("task-first","one"), {"id":"task-second","worker":"one","state":"awaiting_review"}]),
    );
    registration.roots[0].task_bindings.push(TaskBinding {
        worker: "one".into(),
        task_id: "task-first".into(),
    });
    registration.roots[1].task_bindings.push(TaskBinding {
        worker: "one".into(),
        task_id: "task-second".into(),
    });
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].tasks[0].name, "task-first");
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Integrated
    );
    assert_eq!(loader.snapshot.roots[1].tasks[0].name, "task-second");
    assert_eq!(
        loader.snapshot.roots[1].tasks[0].review,
        ReviewState::AwaitingReview
    );
    assert!(
        loader
            .snapshot
            .roots
            .iter()
            .all(|r| r.review_error.is_none())
    );
    registration.roots[1].task_bindings[0].task_id = "task-first".into();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.registration_error,
        Some(ReadError::Identity)
    );
    assert!(loader.snapshot.roots.is_empty());
    for source in &mut registration.roots {
        source.task_bindings.clear();
    }
    let mut first = integrated_item("task-first", "one");
    first["supervisor_root"] = json!(registration.roots[0].directory);
    let second = json!({"id":"task-second","worker":"one","state":"awaiting_review","supervisor_root":registration.roots[1].directory});
    review_queue(&path, json!([first, second]));
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Integrated
    );
    assert_eq!(
        loader.snapshot.roots[1].tasks[0].review,
        ReviewState::AwaitingReview
    );
    registration.roots[1].task_bindings.push(TaskBinding {
        worker: "one".into(),
        task_id: "task-first".into(),
    });
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[1].tasks[0].review,
        ReviewState::Unknown
    );
}

#[test]
fn duplicate_task_ids_are_invalid_but_repeated_worker_roles_are_not() {
    let (dir, mut registration) = fixture();
    let path = dir.path().join("queue.json");
    registration.roots[0].review_queue = Some(ReviewSource {
        path: path.clone(),
        schema: ReviewSchema::OperatorQueueV1,
    });
    review_queue(
        &path,
        json!([
            integrated_item("duplicate", "one"),
            integrated_item("duplicate", "two")
        ]),
    );
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(
        loader.snapshot.roots[0].review_error,
        Some(ReadError::Malformed)
    );
    assert!(
        loader.snapshot.roots[0]
            .tasks
            .iter()
            .all(|task| task.review == ReviewState::Missing)
    );
    review_queue(
        &path,
        json!([
            integrated_item("first", "one"),
            integrated_item("second", "one"),
            integrated_item("third", "two")
        ]),
    );
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots[0].review_error, None);
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Unknown
    );
    assert_eq!(
        loader.snapshot.roots[0].tasks[1].review,
        ReviewState::Integrated
    );
}

#[test]
fn review_cache_expires_on_every_registration_identity_change() {
    let (dir, original) = fixture();
    let mut original = original;
    let path = dir.path().join("queue.json");
    original.roots[0].review_queue = Some(ReviewSource {
        path: path.clone(),
        schema: ReviewSchema::OperatorQueueV1,
    });
    for change in 0..6 {
        let mut loader = Loader::default();
        review_queue(&path, json!([integrated_item("first", "one")]));
        loader.refresh(&original, now());
        assert_eq!(
            loader.snapshot.roots[0].tasks[0].review,
            ReviewState::Integrated
        );
        fs::write(&path, "{").unwrap();
        loader.refresh(&original, now());
        assert_eq!(
            loader.snapshot.roots[0].tasks[0].review,
            ReviewState::Integrated
        );
        let mut changed = original.clone();
        match change {
            0 => changed.session_id = "other-session".into(),
            1 => changed.cwd = dir.path().join("other-cwd"),
            2 => changed.roots[0].task_bindings.push(TaskBinding {
                worker: "one".into(),
                task_id: "first".into(),
            }),
            3 => {
                changed.roots[0].review_queue.as_mut().unwrap().path =
                    dir.path().join("missing-queue.json")
            }
            4 => changed.roots[0].review_queue = None,
            5 => changed.version = 2,
            _ => unreachable!(),
        }
        loader.refresh(&changed, now());
        assert!(
            loader.snapshot.roots[0]
                .tasks
                .iter()
                .all(|t| t.review == ReviewState::Missing)
        );
    }
    let mut loader = Loader::default();
    review_queue(&path, json!([integrated_item("first", "one")]));
    loader.refresh(&original, now());
    loader.registration_failed(ReadError::Identity);
    assert!(loader.snapshot.roots.is_empty());
    fs::write(&path, "{").unwrap();
    loader.refresh(&original, now());
    assert_eq!(
        loader.snapshot.roots[0].tasks[0].review,
        ReviewState::Missing
    );
}

#[test]
fn shared_queue_is_read_once_and_historical_roots_remain_bounded() {
    let dir = crate::tempdir().unwrap();
    let store =
        RegistrationStore::new(dir.path().into(), "history".into(), dir.path().into()).unwrap();
    let queue = dir.path().join("queue.json");
    let mut roots = Vec::new();
    let mut items = Vec::new();
    for index in 0..MAX_ROOTS {
        let root = dir.path().join(format!("run-{index}"));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("worker")).unwrap();
        fs::write(
            root.join("worker/state.json"),
            json!({"state":"done","at":now()}).to_string(),
        )
        .unwrap();
        fs::write(root.join("supervisor.json"), r#"{"state":"done"}"#).unwrap();
        let id = format!("task-{index}");
        roots.push(Source {
            directory: root,
            review_queue: Some(ReviewSource {
                path: queue.clone(),
                schema: ReviewSchema::OperatorQueueV1,
            }),
            task_bindings: vec![TaskBinding {
                worker: "worker".into(),
                task_id: id.clone(),
            }],
        });
        let mut item = integrated_item(&id, "worker");
        item["reviewer"] = json!("x".repeat(400));
        items.push(item);
    }
    review_queue(&queue, json!(items));
    assert!(fs::metadata(&queue).unwrap().len() > REQUEST_BYTES / MAX_ROOTS as u64);
    store
        .update(|registration| {
            registration.roots = roots;
            Ok(())
        })
        .unwrap();
    let registration = store.load().unwrap();
    let mut loader = Loader::default();
    loader.refresh(&registration, now());
    assert_eq!(loader.snapshot.roots.len(), 64);
    assert_eq!(loader.snapshot.registration_error, None);
    assert!(loader.snapshot.roots.iter().all(|r| r.read_error.is_none()
        && r.review_error.is_none()
        && r.tasks[0].review == ReviewState::Integrated));
    assert_eq!(
        store
            .update(|r| {
                r.roots.push(r.roots[0].clone());
                Ok(())
            })
            .unwrap_err(),
        ReadError::Oversized
    );
    assert_eq!(store.load().unwrap(), registration);
}

fn mkfifo(path: impl AsRef<std::path::Path>) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_ref().as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}
