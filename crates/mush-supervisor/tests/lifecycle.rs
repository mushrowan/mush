use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use tokio::process::Command;

struct Fixture {
    dir: tempfile::TempDir,
    manifest: PathBuf,
    run: PathBuf,
    cwd: PathBuf,
}

#[expect(clippy::unwrap_used)]
impl Fixture {
    fn new(script: &str, seconds: i64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        let run = dir.path().join("run");
        fs::create_dir(&cwd).unwrap();
        let executable = dir.path().join("fake-mush");
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let prompt = dir.path().join("prompt.txt");
        fs::write(&prompt, "perform the fixture task").unwrap();
        let manifest = dir.path().join("manifest.json");
        fs::write(&manifest, serde_json::to_vec(&json!({
            "directory":run, "executable":executable, "until":Utc::now() + ChronoDuration::seconds(seconds),
            "concurrency":1, "attempt_seconds":15, "silence_seconds":10, "max_rounds":4,
            "model":"primary", "fallback_model":"fallback", "status_file":true,
            "workers":[{"name":"test", "cwd":cwd, "prompt":prompt,"report":cwd.join("report.json"),"pause_seconds":0}]
        })).unwrap()).unwrap();
        Self {
            dir,
            manifest,
            run,
            cwd,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"));
        command
            .arg("run")
            .arg(&self.manifest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn run(&self) {
        let output = tokio::time::timeout(Duration::from_secs(12), self.command().output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.run.join("test/state.json")).unwrap()).unwrap()
    }
}

const ARGUMENTS: &str = r#"
model= status= resume=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --model) model=$2; shift 2 ;;
    --status-file) status=$2; shift 2 ;;
    --continue) resume=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s:%s\n' "$model" "$resume" >>calls
printf '{"session_id":"fixture","state":"completed"}\n' >"$status"
"#;

#[tokio::test]
async fn unfinished_worker_is_resumed_and_done_worker_is_not_restarted() {
    let fixture = Fixture::new(
        &format!(
            r#"{ARGUMENTS}
if [ -e first ]; then echo '{{"state":"done"}}' >report.json
else touch first; echo '{{"state":"working"}}' >report.json
fi"#
        ),
        20,
    );
    fixture.run().await;
    assert_eq!(
        fs::read_to_string(fixture.cwd.join("calls")).unwrap(),
        "primary:\nprimary:fixture\n"
    );
    assert_eq!(fixture.state()["state"], "done");
    assert!(fixture.dir.path().exists());
}

#[tokio::test]
async fn quota_switches_provider_once() {
    let fixture = Fixture::new(
        &format!(
            r#"{ARGUMENTS}
if [ "$model" = primary ]; then
  echo '{{"session_id":"fixture","state":"error","error_class":"quota"}}' >"$status"
  exit 1
fi
echo '{{"state":"done"}}' >report.json"#
        ),
        20,
    );
    fixture.run().await;
    assert_eq!(
        fs::read_to_string(fixture.cwd.join("calls")).unwrap(),
        "primary:\nfallback:fixture\n"
    );
    assert_eq!(fixture.state()["state"], "done");
}

#[tokio::test]
async fn a_blocked_worker_does_not_get_another_prompt() {
    let fixture = Fixture::new(
        &format!("{ARGUMENTS}\necho '{{\"state\":\"blocked\"}}' >report.json"),
        20,
    );
    fixture.run().await;
    assert_eq!(
        fs::read_to_string(fixture.cwd.join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(fixture.state()["state"], "blocked");
}

#[expect(clippy::unwrap_used)]
async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

fn process_is_running(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| rest.starts_with('Z')))
        .is_some_and(|zombie| !zombie)
}

#[tokio::test]
async fn stop_terminates_a_worker_and_its_child() {
    let fixture = Fixture::new("sleep 300 &\necho $! >child\nwait", 20);
    let mut supervisor = fixture.command().spawn().unwrap();
    wait_for(&fixture.cwd.join("child")).await;
    let pid = fs::read_to_string(fixture.cwd.join("child")).unwrap();
    let pid = pid.trim();
    fs::write(fixture.run.join("STOP"), "").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), supervisor.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!process_is_running(pid));
    assert_eq!(fixture.state()["state"], "stopped");
}

#[tokio::test]
async fn deadline_terminates_a_silent_worker() {
    let fixture = Fixture::new("sleep 300 &\necho $! >child\nwait", 2);
    fixture.run().await;
    let pid = fs::read_to_string(fixture.cwd.join("child")).unwrap();
    assert!(!process_is_running(pid.trim()));
    assert_eq!(fixture.state()["state"], "stopped");
}
