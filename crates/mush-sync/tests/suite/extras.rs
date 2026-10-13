use std::time::{Duration, SystemTime};

use mush_sync::status::{LocalStatus, review};

use crate::harness::{machine, server_at};

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn modified_ms(path: &std::path::Path) -> u64 {
    u64::try_from(
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

#[tokio::test]
async fn saved_tool_output_follows_to_the_other_machine_with_its_age_and_nothing_else() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let outputs = dot.data().join("tool-output");
    std::fs::create_dir_all(&outputs).unwrap();
    let fresh = format!("tool_{}.txt", now_ms() - 60_000);
    std::fs::write(outputs.join(&fresh), "the full output of a long command").unwrap();
    let stale = format!("tool_{}.txt", now_ms() - 8 * 24 * 60 * 60 * 1000);
    std::fs::write(outputs.join(&stale), "a week old").unwrap();
    std::fs::File::options()
        .write(true)
        .open(outputs.join(&stale))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60))
        .unwrap();
    for private in ["oauth.json", "api-keys.json", "notes.txt"] {
        std::fs::write(outputs.join(private), "never leaves dot").unwrap();
    }
    dot.sync().await;
    esme.sync().await;

    let arrived = esme.data().join("tool-output");
    assert_eq!(
        std::fs::read_to_string(arrived.join(&fresh)).unwrap(),
        "the full output of a long command"
    );
    assert_eq!(
        modified_ms(&arrived.join(&fresh)),
        modified_ms(&outputs.join(&fresh))
    );
    let mut names: Vec<String> = std::fs::read_dir(&arrived)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, std::slice::from_ref(&fresh));

    let reply = format!("rejected_{}.txt", now_ms());
    std::fs::write(arrived.join(&reply), "written on esme").unwrap();
    esme.sync().await;
    dot.sync().await;
    assert_eq!(
        std::fs::read_to_string(outputs.join(&reply)).unwrap(),
        "written on esme"
    );
    assert!(dot.sync().await.quiet());
    assert!(esme.sync().await.quiet());
}

#[tokio::test]
async fn credentials_beside_a_session_never_leave_the_machine() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("with secrets nearby", &["hello"]);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    for name in [
        "oauth.json",
        "api-keys.json",
        "sync.key",
        ".env",
        "deploy.pem",
        "notes.md",
    ] {
        std::fs::write(dot.artifact(&id, name), "secret").unwrap();
    }
    dot.sync().await;
    esme.sync().await;
    let mut names: Vec<String> = std::fs::read_dir(esme.artifact(&id, ""))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["notes.md".to_owned()]);
}

#[tokio::test]
async fn a_large_artifact_travels_in_frames_and_arrives_whole() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("big artifact", &["hello"]);
    std::fs::create_dir_all(dot.artifact(&id, "")).unwrap();
    let bytes: Vec<u8> = (0..(mush_sync::blob::FRAME * 3 + 777))
        .map(|i| u8::try_from((i * 31) % 251).unwrap())
        .collect();
    std::fs::write(dot.artifact(&id, "data.bin"), &bytes).unwrap();
    dot.sync().await;
    esme.sync().await;
    assert_eq!(
        std::fs::read(esme.artifact(&id, "data.bin")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn a_header_edited_offline_survives_even_when_its_clock_says_it_is_older() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let id = dot.create("shared", &["start"]);
    dot.sync().await;
    esme.sync().await;

    let path = dot.data().join("sessions").join(format!("{id}.json"));
    let mut header: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    header["meta"]["title"] = "renamed on dot".into();
    header["meta"]["updated_at"] = 1_000.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&header).unwrap()).unwrap();
    esme.append(&id, "esme moved on");
    esme.sync().await;

    let report = dot.sync().await;
    assert_eq!(report.sidecars.len(), 1, "{report:?}");
    assert_eq!(dot.texts(&id), ["start", "esme moved on"]);
    let pending = review(dot.engine.layout()).unwrap();
    assert_eq!(pending.sidecars.len(), 1);
    let kept = std::fs::read_to_string(&pending.sidecars[0].1).unwrap();
    assert!(kept.contains("renamed on dot"), "{kept}");
}

#[tokio::test]
async fn every_machine_reports_its_sync_to_the_server_and_keeps_a_local_status() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    dot.create("one", &["hello"]);
    dot.sync().await;
    esme.sync().await;

    let machines = dot
        .engine
        .remote()
        .machines(Duration::from_secs(5))
        .await
        .unwrap();
    let names: Vec<&str> = machines
        .machines
        .iter()
        .map(|machine| machine.name.as_str())
        .collect();
    assert_eq!(names, ["dot", "esme"]);
    let esme_report = &machines.machines[1].report;
    assert!(esme_report.rev > 0);
    assert_eq!(esme_report.sessions, 1);
    assert_eq!(esme_report.failed, 0);

    let status = LocalStatus::load(esme.engine.layout());
    assert!(status.last_ok_ms.is_some());
    assert_eq!(status.last_error, None);
    assert!(status.rev > 0);

    drop(server);
    let offline = esme
        .engine
        .pass(
            mush_sync::Scope::All,
            mush_sync::Direction::Both,
            None,
            Duration::from_secs(1),
        )
        .await;
    assert!(offline.is_err());
    let status = LocalStatus::load(esme.engine.layout());
    assert!(status.offline_since_ms.is_some());
    assert!(status.last_error.is_some());
    assert!(status.last_ok_ms.is_some());
}

#[tokio::test]
async fn a_pass_woken_by_the_server_only_looks_at_what_changed_there() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let (dot, esme) = (machine("dot", &server), machine("esme", &server));
    let here = dot.create("edited on dot", &["one"]);
    let there = dot.create("edited on esme", &["one"]);
    dot.sync().await;
    esme.sync().await;
    assert!(dot.sync().await.quiet());
    dot.append(&here, "two, not pushed yet");
    esme.append(&there, "two from esme");
    esme.sync().await;

    let woken = dot
        .engine
        .pass(
            mush_sync::Scope::Changed(Vec::new()),
            mush_sync::Direction::Both,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(woken.pulled, [there.to_string()], "{woken:?}");
    assert!(woken.pushed.is_empty(), "{woken:?}");
    assert_eq!(dot.sync().await.pushed, [here.to_string()]);
}

#[tokio::test]
async fn half_written_files_an_interrupted_sync_left_are_cleared_once_stale() {
    let root = tempfile::tempdir().unwrap();
    let server = server_at(root.path()).await;
    let dot = machine("dot", &server);
    let id = dot.create("interrupted", &["one"]);
    let sessions = dot.data().join("sessions");
    let stale = sessions.join(format!("{id}.history.jsonl.sync-in-0123456789abcdef"));
    let fresh = sessions.join(format!("{id}.history.jsonl.sync-in-fedcba9876543210"));
    let artifact_dir = dot.artifact(&id, "");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let stale_artifact = artifact_dir.join("notes.md.tmp-1-2-3");
    for path in [&stale, &fresh, &stale_artifact] {
        std::fs::write(path, "partial").unwrap();
    }
    for path in [&stale, &stale_artifact] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
            .unwrap();
    }
    dot.sync().await;
    assert!(!stale.exists());
    assert!(!stale_artifact.exists());
    assert!(fresh.exists());
}
