use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{Duration, Utc};
use serde_json::{Value, json};

struct Fixture {
    temp: tempfile::TempDir,
    run: PathBuf,
    workspace: PathBuf,
    manifest: PathBuf,
    prompt: PathBuf,
}

#[expect(clippy::unwrap_used)]
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let run = temp.path().join("run");
        let workspace = temp.path().join("workspace");
        let worker = run.join("one");
        fs::create_dir_all(worker.join("mush-data/sessions")).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::write(
            workspace.join("report.json"),
            r#"{"state":"done","completed":["original work"]}"#,
        )
        .unwrap();
        fs::write(worker.join("state.json"), r#"{"name":"one","state":"done","round":1,"model":"primary","session_id":"saved","retries":1,"stalled":1}"#).unwrap();
        fs::write(
            worker.join("round-001.json"),
            r#"{"state":"completed","session_id":"saved"}"#,
        )
        .unwrap();
        fs::write(worker.join("round-001.log"), "original log").unwrap();
        fs::write(
            worker.join("mush-data/sessions/saved.json"),
            "original session",
        )
        .unwrap();
        fs::write(
            run.join("supervisor.json"),
            r#"{"state":"stopped","errors":[]}"#,
        )
        .unwrap();
        let base = temp.path().join("base.md");
        fs::write(&base, "original assignment: local only").unwrap();
        let prompt = temp.path().join("follow-up.md");
        fs::write(&prompt, "investigate the next result and refactor").unwrap();
        let executable = temp.path().join("fake-mush");
        fs::write(
            &executable,
            r##"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
 case "$1" in
  --continue) printf '%s' "$2" >resumed; shift 2 ;;
  --status-file) status=$2; shift 2 ;;
  -p) printf '%s' "$2" >received; shift 2 ;;
  *) shift ;;
 esac
done
printf '{"state":"completed","session_id":"saved"}' >"$status"
printf '{"state":"done","completed":["follow-up"]}' >report.json
"##,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let manifest = run.join("manifest.json");
        write(
            &manifest,
            &json!({
                "directory":run,"executable":executable,"until":Utc::now()+Duration::minutes(5),
                "concurrency":1,"attempt_seconds":10,"silence_seconds":5,"max_rounds":4,
                "model":"primary","fallback_model":"fallback","status_file":true,
                "workers":[{"name":"one","cwd":workspace,"prompt":base,"report":workspace.join("report.json"),"pause_seconds":0}]
            }),
        );
        Self {
            temp,
            run,
            workspace,
            manifest,
            prompt,
        }
    }

    fn queue(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
            .arg("follow-up")
            .arg(&self.run)
            .arg("one")
            .arg("--prompt-file")
            .arg(&self.prompt)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

#[expect(clippy::unwrap_used)]
fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

#[test]
fn explicit_follow_up_resumes_the_same_session_and_preserves_previous_results() {
    let f = Fixture::new();
    let worker = f.run.join("one");
    let original_state = fs::read(worker.join("state.json")).unwrap();
    let original_report = fs::read(f.workspace.join("report.json")).unwrap();
    let original_manifest = fs::read(&f.manifest).unwrap();
    let result = f.queue();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&f.manifest).unwrap(), original_manifest);
    assert_eq!(
        fs::read(worker.join("round-001.log")).unwrap(),
        b"original log"
    );
    assert_eq!(
        fs::read(worker.join("mush-data/sessions/saved.json")).unwrap(),
        b"original session"
    );
    let history = worker.join("followups/after-001");
    assert_eq!(
        fs::read(history.join("state.json")).unwrap(),
        original_state
    );
    assert_eq!(
        fs::read(history.join("report.json")).unwrap(),
        original_report
    );
    let queued: Value =
        serde_json::from_slice(&fs::read(worker.join("state.json")).unwrap()).unwrap();
    assert_eq!(queued["round"], 1);
    assert_eq!(queued["retries"], 1);
    assert_eq!(queued["stalled"], 1);
    assert!(!f.queue().status.success());
    let result = Command::new("timeout")
        .args([
            "--kill-after=2s",
            "15s",
            env!("CARGO_BIN_EXE_mush-supervisor"),
            "run",
        ])
        .arg(&f.manifest)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.workspace.join("resumed")).unwrap(),
        "saved"
    );
    let prompt = fs::read_to_string(f.workspace.join("received")).unwrap();
    assert!(prompt.contains("original assignment: local only"));
    assert!(prompt.contains("investigate the next result and refactor"));
    assert!(prompt.contains("continue authorised work within the task scope"));
    assert!(prompt.contains(
        "stop and report blocked when the next step needs approval or essential information"
    ));
    assert_eq!(
        fs::read(worker.join("mush-data/sessions/saved.json")).unwrap(),
        b"original session"
    );
    assert_eq!(
        fs::read(f.temp.path().join("base.md")).unwrap(),
        b"original assignment: local only"
    );
    assert!(worker.join("round-002.log").exists());
    assert!(f.temp.path().exists());
}

#[test]
fn stop_expiry_budget_and_active_runs_reject_follow_up_without_modification() {
    for fault in [
        "stop",
        "deadline",
        "budget",
        "busy",
        "missing-session",
        "unknown-worker",
    ] {
        let f = Fixture::new();
        let mut lock = None;
        match fault {
            "stop" => fs::write(f.run.join("STOP"), "").unwrap(),
            "deadline" | "budget" => {
                let mut manifest: Value =
                    serde_json::from_slice(&fs::read(&f.manifest).unwrap()).unwrap();
                if fault == "deadline" {
                    manifest["until"] = json!(Utc::now() - Duration::seconds(1));
                } else {
                    manifest["max_rounds"] = json!(1);
                }
                write(&f.manifest, &manifest);
            }
            "busy" => {
                let f = fs::File::create(f.run.join("supervisor.lock")).unwrap();
                f.lock().unwrap();
                lock = Some(f);
            }
            "missing-session" => {
                fs::remove_file(f.run.join("one/mush-data/sessions/saved.json")).unwrap()
            }
            "unknown-worker" => {
                let mut manifest: Value =
                    serde_json::from_slice(&fs::read(&f.manifest).unwrap()).unwrap();
                manifest["workers"][0]["name"] = json!("another");
                write(&f.manifest, &manifest);
            }
            _ => unreachable!(),
        }
        let state = fs::read(f.run.join("one/state.json")).unwrap();
        let report = fs::read(f.workspace.join("report.json")).unwrap();
        assert!(!f.queue().status.success(), "{fault}");
        assert_eq!(
            fs::read(f.run.join("one/state.json")).unwrap(),
            state,
            "{fault}"
        );
        assert_eq!(
            fs::read(f.workspace.join("report.json")).unwrap(),
            report,
            "{fault}"
        );
        assert!(!f.run.join("one/followups").exists(), "{fault}");
        drop(lock);
    }
}

#[test]
fn blocked_work_can_receive_an_explicit_follow_up_without_resetting_budgets() {
    let f = Fixture::new();
    let path = f.run.join("one/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["state"] = json!("blocked");
    state["model"] = json!("fallback");
    write(&path, &state);
    let output = f.queue();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let queued: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(queued["model"], "fallback");
    assert_eq!(queued["retries"], 1);
    assert_eq!(queued["stalled"], 1);
    assert_eq!(
        fs::metadata(f.run.join("one/followups/after-001/session.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn empty_or_linked_prompt_cannot_reopen_completed_work() {
    for linked in [false, true] {
        let f = Fixture::new();
        if linked {
            fs::remove_file(&f.prompt).unwrap();
            std::os::unix::fs::symlink(f.temp.path().join("base.md"), &f.prompt).unwrap();
        } else {
            fs::write(&f.prompt, " \n").unwrap();
        }
        assert!(!f.queue().status.success());
        let state: Value =
            serde_json::from_slice(&fs::read(f.run.join("one/state.json")).unwrap()).unwrap();
        assert_eq!(state["state"], "done");
        assert!(!f.run.join("one/followups").exists());
    }
}
