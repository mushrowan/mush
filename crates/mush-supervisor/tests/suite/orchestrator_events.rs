use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mush_supervisor::inbox::{Binding, Event, Inbox, TerminalReason};
use serde_json::{Value, json};
use tokio::process::Command;

const PRELUDE: &str = r#"
umask 077
status= resume=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --status-file) status=$2; shift 2 ;;
    --resume) resume=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s\n' "$resume" >>calls
printf '{"session_id":"fixture","state":"completed"}' >"$status"
mkdir -p "$MUSH_DATA_DIR/sessions"
printf '{"saved":"prefix"}' >"$MUSH_DATA_DIR/sessions/fixture.json"
report() {
  printf '{"run_id":"%s","worker_id":"%s","assignment_id":%s,"round":%s,"state":"%s","private":"secret report body"}' "$MUSH_RUN_ID" "$MUSH_WORKER_ID" "$MUSH_ASSIGNMENT_ID" "$MUSH_ROUND" "$1" >report.json
}
"#;

struct Fixture {
    dir: tempfile::TempDir,
    manifest: PathBuf,
    run: PathBuf,
    cwd: PathBuf,
    inbox: PathBuf,
}

#[expect(clippy::unwrap_used)]
impl Fixture {
    fn new(script: &str) -> Self {
        let dir = crate::tempdir().unwrap();
        let run = dir.path().join("run");
        fs::DirBuilder::new().mode(0o700).create(&run).unwrap();
        let cwd = dir.path().join("worker");
        fs::create_dir(&cwd).unwrap();
        let executable = dir.path().join("fake-mush");
        fs::write(
            &executable,
            format!("#!/bin/sh\nset -eu\n{PRELUDE}\n{script}\n"),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let prompt = dir.path().join("prompt.txt");
        fs::write(&prompt, "fixture task").unwrap();
        let inbox = dir.path().join("inbox");
        Inbox::register(
            &inbox,
            Binding {
                session_id: "orchestrator".into(),
                cwd: dir.path().to_owned(),
                workers: vec![cwd.clone()],
            },
        )
        .unwrap();
        let manifest = dir.path().join("manifest.json");
        fs::write(&manifest, serde_json::to_vec(&json!({
            "directory":run,"executable":executable,"until":chrono::Utc::now()+chrono::Duration::seconds(30),
            "concurrency":1,"attempt_seconds":15,"silence_seconds":10,"max_rounds":3,
            "model":"primary","fallback_model":"fallback","status_file":true,"orchestrator_inbox":inbox,
            "workers":[{"name":"worker","cwd":cwd,"prompt":prompt,"report":cwd.join("report.json"),"pause_seconds":0}]
        })).unwrap()).unwrap();
        Self {
            dir,
            manifest,
            run,
            cwd,
            inbox,
        }
    }

    fn command(&self, operation: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"));
        command
            .args([operation])
            .arg(&self.manifest)
            .env("HOME", self.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn run(&self) {
        let output = tokio::time::timeout(Duration::from_secs(10), self.command("run").output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn events(&self) -> Vec<Event> {
        Inbox::open(&self.inbox).unwrap().pending().unwrap()
    }

    fn alter(&self, update: impl FnOnce(&mut Value)) {
        let mut value: Value = serde_json::from_slice(&fs::read(&self.manifest).unwrap()).unwrap();
        update(&mut value);
        fs::write(&self.manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}

#[expect(clippy::unwrap_used)]
async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn tool_calls_agent_end_and_automatic_rounds_do_not_complete_a_worker() {
    let fixture = Fixture::new(
        r#"
if [ "$MUSH_ROUND" = 1 ]; then
  report done
  echo 'ToolExecStart AgentEnd AgentEnd ToolExecEnd'
  touch between-tools
  while [ ! -e release ]; do sleep 0.02; done
  report working
else
  touch next-round
  while [ ! -e finish ]; do sleep 0.02; done
  report done
fi
"#,
    );
    let mut process = fixture.command("run").spawn().unwrap();
    wait_for(&fixture.cwd.join("between-tools")).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(fixture.events().is_empty());
    fs::write(fixture.cwd.join("release"), "").unwrap();
    wait_for(&fixture.cwd.join("next-round")).await;
    assert!(fixture.events().is_empty());
    fs::write(fixture.cwd.join("finish"), "").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let events = fixture.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason, TerminalReason::Completed);
    assert_eq!(events[0].round, 2);
    assert_eq!(events[0].assignment_id, 1);
    assert_eq!(events[0].session_id.as_deref(), Some("fixture"));
    assert!(
        !serde_json::to_string(&events[0])
            .unwrap()
            .contains("secret")
    );
    fixture.run().await;
    assert_eq!(fixture.events(), events);
    assert_eq!(
        fs::read_to_string(fixture.cwd.join("calls")).unwrap(),
        "\nfixture\n"
    );
}

#[tokio::test]
async fn stale_wrong_key_and_old_round_reports_do_not_complete() {
    for script in [
        "echo '{\"status\":\"done\"}' >report.json",
        "report done; sed -i 's/\"round\":1/\"round\":0/' report.json",
        "report done; sed -i 's/\"worker_id\":\"worker\"/\"worker_id\":\"other\"/' report.json",
        "MUSH_RUN_ID=old-run; report done",
        "MUSH_ASSIGNMENT_ID=9; report done",
        "true",
    ] {
        let fixture = Fixture::new(script);
        fixture.alter(|m| m["max_rounds"] = json!(1));
        fs::write(fixture.cwd.join("report.json"), r#"{"state":"done"}"#).unwrap();
        fixture.run().await;
        assert_eq!(fixture.events()[0].reason, TerminalReason::RoundLimit);
    }
}

#[tokio::test]
async fn repeat_done_is_an_automatic_continuation() {
    let fixture = Fixture::new("report done");
    fixture.alter(|m| {
        m["max_rounds"] = json!(2);
        m["workers"][0]["repeat_done"] = json!(true);
    });
    fixture.run().await;
    assert_eq!(fixture.events().len(), 1);
    assert_eq!(fixture.events()[0].reason, TerminalReason::RoundLimit);
    assert_eq!(fixture.events()[0].round, 2);
}

#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the supervisor reads processes and boot id from /proc"
)]
async fn terminal_reasons_do_not_report_failed_work_as_completed() {
    for (script, reason) in [
        ("report blocked", TerminalReason::Blocked),
        ("report awaiting_input", TerminalReason::AwaitingInput),
        ("report done; exit 7", TerminalReason::Failed),
        (
            "report done; echo '{\"state\":\"running\"}' >\"$status\"",
            TerminalReason::Failed,
        ),
        (
            "report done; echo '{\"state\":\"cancelled\"}' >\"$status\"",
            TerminalReason::Cancelled,
        ),
        (
            "report done; sleep 30 & echo $! >child",
            TerminalReason::ChildSurvived,
        ),
    ] {
        let fixture = Fixture::new(script);
        fixture.run().await;
        assert_eq!(fixture.events().len(), 1);
        assert_eq!(fixture.events()[0].reason, reason);
        if fixture.cwd.join("child").exists() {
            let pid = fs::read_to_string(fixture.cwd.join("child")).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stat = fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
            assert!(stat.is_empty() || stat.contains(") Z "));
        }
    }
}

#[tokio::test]
async fn timeout_deadline_and_stop_are_distinct() {
    for reason in [
        TerminalReason::Timeout,
        TerminalReason::Deadline,
        TerminalReason::Cancelled,
    ] {
        let fixture = Fixture::new("report done; touch ready; sleep 30");
        fixture.alter(|m| {
            if reason == TerminalReason::Timeout {
                m["attempt_seconds"] = json!(1);
            }
            if reason == TerminalReason::Deadline {
                m["until"] = json!(chrono::Utc::now() + chrono::Duration::seconds(2));
            }
        });
        let mut process = fixture.command("run").spawn().unwrap();
        wait_for(&fixture.cwd.join("ready")).await;
        if reason == TerminalReason::Cancelled {
            fs::write(fixture.run.join("STOP"), "").unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(8), process.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert_eq!(fixture.events()[0].reason, reason);
    }
}

#[tokio::test]
async fn follow_up_keeps_session_and_archives_each_assignment_event() {
    let fixture = Fixture::new("report done");
    fixture.run().await;
    let first = fixture.events().remove(0);
    let first_report = fs::read(&first.report_path).unwrap();
    let prompt = fixture.dir.path().join("follow-up.txt");
    fs::write(&prompt, "second assignment").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .arg("follow-up")
        .arg(&fixture.run)
        .args(["worker", "--prompt-file"])
        .arg(prompt)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.run().await;
    let events = fixture.events();
    assert_eq!(events.len(), 2);
    assert_ne!(events[0].event_id, events[1].event_id);
    assert_eq!(events[1].assignment_id, 2);
    assert_eq!(events[1].session_id, first.session_id);
    assert_eq!(fs::read(first.report_path).unwrap(), first_report);
    assert_eq!(
        fs::read_to_string(fixture.cwd.join("calls")).unwrap(),
        "\nfixture\n"
    );
}

#[tokio::test]
async fn simultaneous_workers_and_replay_preserve_events() {
    let fixture = Fixture::new("report done");
    let second = fixture.dir.path().join("second");
    fs::create_dir(&second).unwrap();
    fs::remove_dir_all(&fixture.inbox).unwrap();
    Inbox::register(
        &fixture.inbox,
        Binding {
            session_id: "orchestrator".into(),
            cwd: fixture.dir.path().to_owned(),
            workers: vec![fixture.cwd.clone(), second.clone()],
        },
    )
    .unwrap();
    fixture.alter(|m| {
        m["concurrency"] = json!(2);
        let mut worker = m["workers"][0].clone();
        worker["name"] = json!("second");
        worker["cwd"] = json!(second);
        worker["report"] = json!(second.join("report.json"));
        m["workers"].as_array_mut().unwrap().push(worker);
    });
    fixture.run().await;
    let events = fixture.events();
    assert_eq!(events.len(), 2);
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    for event in &events {
        inbox.acknowledge(&event.event_id).unwrap();
    }
    assert!(
        fixture
            .command("replay-events")
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(fixture.events().is_empty());
}

#[tokio::test]
async fn crashed_supervisor_replays_failure_and_does_not_launch_another_child() {
    let fixture = Fixture::new("report done; touch ready; sleep 30 & echo $! >child; wait");
    let mut process = fixture.command("run").spawn().unwrap();
    wait_for(&fixture.cwd.join("ready")).await;
    tokio::time::sleep(Duration::from_millis(2200)).await;
    process.kill().await.unwrap();
    process.wait().await.unwrap();
    assert!(fixture.events().is_empty());
    fixture.run().await;
    assert_eq!(fixture.events().len(), 1);
    assert_eq!(fixture.events()[0].reason, TerminalReason::SupervisorError);
    assert_eq!(fs::read_to_string(fixture.cwd.join("calls")).unwrap(), "\n");
    let pid = fs::read_to_string(fixture.cwd.join("child")).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid.trim())).unwrap_or_default();
    assert!(stat.is_empty() || stat.contains(") Z "));
}

#[tokio::test]
async fn an_offline_inbox_keeps_the_terminal_outbox_for_replay() {
    let fixture =
        Fixture::new("report done; touch ready; while [ ! -e finish ]; do sleep 0.02; done");
    let mut process = fixture.command("run").spawn().unwrap();
    wait_for(&fixture.cwd.join("ready")).await;
    let offline = fixture.dir.path().join("offline-inbox");
    fs::rename(&fixture.inbox, &offline).unwrap();
    fs::write(fixture.cwd.join("finish"), "").unwrap();
    assert!(
        !tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.run.join("worker/state.json")).unwrap()).unwrap();
    assert_eq!(state["terminal_event"]["reason"], "completed");
    fs::rename(offline, &fixture.inbox).unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            fixture.command("replay-events").status()
        )
        .await
        .unwrap()
        .unwrap()
        .success()
    );
    assert_eq!(
        fixture.events()[0].event_id,
        state["terminal_event"]["event_id"]
    );
}

#[tokio::test]
async fn events_are_opt_in_and_spawn_failures_have_a_terminal_event() {
    let fixture = Fixture::new("report done");
    fs::remove_file(fixture.dir.path().join("fake-mush")).unwrap();
    fixture.run().await;
    assert_eq!(fixture.events()[0].reason, TerminalReason::SpawnFailed);
    let fixture = Fixture::new("echo '{\"state\":\"done\"}' >report.json");
    fixture.alter(|m| {
        m.as_object_mut().unwrap().remove("orchestrator_inbox");
    });
    fixture.run().await;
    assert!(fixture.events().is_empty());
    assert!(!fixture.run.join("event-run.json").exists());
}

#[tokio::test]
async fn committed_terminal_report_recovers_a_crashed_state_writer() {
    let fixture = Fixture::new("report done");
    fixture.run().await;
    let event = fixture.events().remove(0);
    let saved_report = fs::read(&event.report_path).unwrap();
    let state_path = fixture.run.join("worker/state.json");
    fs::write(
        &state_path,
        r#"{"state":"running","round":1,"model":"primary"}"#,
    )
    .unwrap();
    fs::write(state_path.with_extension("pending-write"), "{broken").unwrap();
    fixture.run().await;
    assert_eq!(fixture.events(), vec![event.clone()]);
    assert_eq!(fs::read(event.report_path).unwrap(), saved_report);
    assert_eq!(fs::read_to_string(fixture.cwd.join("calls")).unwrap(), "\n");
}
