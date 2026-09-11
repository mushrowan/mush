use std::fs;
use std::path::Path;
use std::process::Command;

use chrono::{Duration, Utc};
use serde_json::{Value, json};

#[expect(clippy::unwrap_used)]
fn jj(root: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .arg("--color=never")
        .args([
            "--config",
            "user.name=fixture",
            "--config",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[expect(clippy::unwrap_used)]
fn prepare(root: &Path, tasks: &Path, run: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .arg("prepare")
        .arg(tasks)
        .arg("--repository")
        .arg(root)
        .arg("--directory")
        .arg(run)
        .args([
            "--prefix",
            "research",
            "--model",
            "primary",
            "--fallback-model",
            "fallback",
        ])
        .arg("--executable")
        .arg(env!("CARGO_BIN_EXE_mush-supervisor"))
        .arg("--until")
        .arg((Utc::now() + Duration::hours(1)).to_rfc3339())
        .output()
        .unwrap()
}

#[test]
fn tasks_create_isolated_workspaces_without_changing_the_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    jj(&root, &["git", "init"]);
    fs::write(root.join(".gitignore"), "workspaces/\ndata/\n").unwrap();
    fs::write(root.join("code"), "committed").unwrap();
    jj(&root, &["commit", "-m", "feat(test): seed the repository"]);
    let parent = jj(&root, &["log", "--no-graph", "-r", "@-", "-T", "commit_id"]);
    fs::write(root.join("code"), "user edit").unwrap();
    let working = jj(&root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let tasks = temp.path().join("tasks.json");
    fs::write(
        &tasks,
        serde_json::to_vec(&json!([
            {"name":"crawl", "prompt":"implement the index planner"},
            {"name":"registry", "prompt":"add registry metadata"}
        ]))
        .unwrap(),
    )
    .unwrap();
    let run = temp.path().join("run");
    let output = prepare(&root, &tasks, &run);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["workers"].as_array().unwrap().len(), 2);
    for worker in manifest["workers"].as_array().unwrap() {
        let cwd = Path::new(worker["cwd"].as_str().unwrap());
        assert_eq!(fs::read_to_string(cwd.join("code")).unwrap(), "committed");
        assert_eq!(
            jj(cwd, &["log", "--no-graph", "-r", "@-", "-T", "commit_id"]),
            parent
        );
        assert!(Path::new(worker["prompt"].as_str().unwrap()).is_file());
        assert!(
            worker["report"]
                .as_str()
                .unwrap()
                .starts_with(cwd.to_str().unwrap())
        );
    }
    assert_eq!(fs::read_to_string(root.join("code")).unwrap(), "user edit");
    assert_eq!(
        jj(&root, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]),
        working
    );
    assert!(!prepare(&root, &tasks, &run).status.success());
    let report = Path::new(manifest["workers"][0]["report"].as_str().unwrap());
    fs::write(report, r#"{"state":"working","next":["review fixtures"]}"#).unwrap();
    let polled = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .arg("status")
        .arg(run.join("run"))
        .output()
        .unwrap();
    assert!(polled.status.success());
    let polled: Value = serde_json::from_slice(&polled.stdout).unwrap();
    assert_eq!(polled["workers"][0]["report"]["next"][0], "review fixtures");
    assert!(
        polled["workers"][0]["log"]
            .as_str()
            .unwrap()
            .ends_with("round-000.log")
    );
}

#[test]
fn invalid_task_lists_make_no_workspaces() {
    let temp = tempfile::tempdir().unwrap();
    jj(temp.path(), &["git", "init"]);
    fs::write(
        temp.path().join(".gitignore"),
        "tasks.json\nrun/\nworkspaces/\n",
    )
    .unwrap();
    jj(
        temp.path(),
        &["commit", "-m", "chore(test): ignore fixtures"],
    );
    for rows in [
        json!([]),
        json!([{"name":"../escape", "prompt":"task"}]),
        json!([
            {"name":"one", "prompt":"task"}, {"name":"one", "prompt":"other"}
        ]),
    ] {
        let path = temp.path().join("tasks.json");
        fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
        assert!(
            !prepare(temp.path(), &path, &temp.path().join("run"))
                .status
                .success()
        );
        assert!(!temp.path().join("workspaces").exists());
    }
}

#[test]
fn a_workspace_symlink_cannot_redirect_task_creation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let outside = temp.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    jj(&root, &["git", "init"]);
    fs::write(root.join(".gitignore"), "workspaces/\n").unwrap();
    jj(&root, &["commit", "-m", "chore(test): ignore workspaces"]);
    std::os::unix::fs::symlink(&outside, root.join("workspaces")).unwrap();
    let tasks = temp.path().join("tasks.json");
    fs::write(&tasks, r#"[{"name":"task","prompt":"work"}]"#).unwrap();
    assert!(
        !prepare(&root, &tasks, &temp.path().join("run"))
            .status
            .success()
    );
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}

#[test]
fn logs_are_bounded_and_terminal_workers_reject_undeliverable_prompts() {
    let temp = tempfile::tempdir().unwrap();
    let worker = temp.path().join("worker");
    fs::create_dir(&worker).unwrap();
    fs::write(worker.join("state.json"), r#"{"state":"done","round":1}"#).unwrap();
    fs::write(worker.join("round-001.log"), "old output\nlatest\n").unwrap();
    let binary = env!("CARGO_BIN_EXE_mush-supervisor");
    let output = Command::new(binary)
        .arg("logs")
        .arg(temp.path())
        .args(["worker", "--bytes", "7"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"latest\n");
    let output = Command::new(binary)
        .arg("poke")
        .arg(temp.path())
        .args(["worker", "review the diff"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!temp.path().join("inbox").exists());
    fs::write(
        worker.join("state.json"),
        r#"{"state":"running","round":1}"#,
    )
    .unwrap();
    let output = Command::new(binary)
        .arg("poke")
        .arg(temp.path())
        .args(["worker", "review the diff"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let messages: Vec<_> = fs::read_dir(temp.path().join("inbox/worker"))
        .unwrap()
        .collect();
    assert_eq!(messages.len(), 1);
}
