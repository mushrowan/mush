use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mush_supervisor::inbox::{Binding, Inbox};
use mush_supervisor::review::digest;
use serde_json::{Value, json};

#[expect(clippy::unwrap_used)]
fn write(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[expect(clippy::unwrap_used)]
fn directory(path: &Path) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
}

struct Fixture {
    root: tempfile::TempDir,
    config: PathBuf,
    route: PathBuf,
    inbox: PathBuf,
    run: PathBuf,
    cwd: PathBuf,
}

#[expect(clippy::unwrap_used)]
impl Fixture {
    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
            .args(args)
            .env("HOME", self.root.path())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn new(resume: bool, parent: bool, outcome: &str) -> Self {
        Self::new_worker(resume, parent, outcome, "")
    }

    fn new_worker(resume: bool, parent: bool, outcome: &str, before_resume: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        for name in ["run", "work", "review", "review-work", "pool", "bin"] {
            directory(&base.join(name));
        }
        let run = base.join("run");
        let cwd = base.join("work");
        let route = base.join("route");
        let config = base.join("assignment.json");
        let inbox = base.join("inbox");
        let fixture = Self {
            root,
            config,
            route,
            inbox,
            run,
            cwd,
        };
        let base = fixture.root.path();
        let head = "1".repeat(40);
        let jj = base.join("bin/jj");
        fs::write(&jj, format!("#!/bin/sh\nset -eu\ncase \"$*\" in *'conflicts() & @'*) exit 0;; esac\nprintf '%s' '{head}'\n")).unwrap();
        fs::set_permissions(&jj, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = base.join("worker");
        fs::write(&executable, r#"#!/bin/sh
set -eu
umask 077
status= resume= prompt=
while [ "$#" -gt 0 ]; do
case "$1" in
--status-file) status=$2; shift 2;;
--continue) resume=$2; shift 2;;
-p) prompt=$2; shift 2;;
*) shift;;
esac
done
test "$resume" = original
printf '%s' "$prompt" >../received
printf 'launch\n' >>../calls
printf '{"state":"completed","session_id":"original"}' >"$status"
printf '{"state":"done","run_id":"%s","worker_id":"producer","assignment_id":%s,"round":%s}' "$MUSH_RUN_ID" "$MUSH_ASSIGNMENT_ID" "$MUSH_ROUND" >report.json
"#).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let script = fs::read_to_string(&executable).unwrap();
        fs::write(
            &executable,
            script.replace(
                "test \"$resume\" = original",
                &format!("{before_resume}\ntest \"$resume\" = original"),
            ),
        )
        .unwrap();
        let until = chrono::Utc::now() + chrono::Duration::minutes(10);
        let prompt = base.join("prompt");
        fs::write(&prompt, "original task and constraints").unwrap();
        let manifest = json!({"directory":fixture.run,"executable":executable,"until":until,"concurrency":1,
            "attempt_seconds":5,"silence_seconds":3,"max_rounds":3,"model":"fixture","fallback_model":"fixture",
            "status_file":true,"durable_events":true,"admission":base.join("pool"),
            "workers":[{"name":"producer","cwd":fixture.cwd,"prompt":prompt,"report":fixture.cwd.join("report.json"),"pause_seconds":0}]});
        write(&fixture.run.join("manifest.json"), &manifest);
        write(
            &fixture.run.join("supervisor.json"),
            &json!({"state":"stopped"}),
        );
        write(
            &fixture.run.join("event-run.json"),
            &json!({"run_id":"producer-run"}),
        );
        directory(&fixture.run.join("producer"));
        directory(&fixture.run.join("producer/mush-data"));
        directory(&fixture.run.join("producer/mush-data/sessions"));
        let checkpoint = fixture.run.join("producer/round-001.json");
        write(
            &checkpoint,
            &json!({"state":"completed","session_id":"original"}),
        );
        let session = fixture
            .run
            .join("producer/mush-data/sessions/original.json");
        write(&session, &json!({"synthetic":"saved original session"}));
        let producer_state = json!({"state":"done","model":"fixture","round":1,"session_id":"original","retries":0,"stalled":0,
            "run_id":"producer-run","worker_id":"producer","assignment_id":1,
            "terminal_event":{"event_id":"producer-run.producer.1","run_id":"producer-run","worker_id":"producer","assignment_id":1,"round":1,"session_id":"original","reason":"completed","report_path":fixture.cwd.join("report.json")}});
        write(&fixture.run.join("producer/state.json"), &producer_state);
        write(&fixture.cwd.join("report.json"), &json!({"state":"done"}));
        write(
            &base.join("pool/policy.json"),
            &json!({"version":1,"capacity":3,"root_reserved":1,"external_reserved":1,"accounting_complete":true,"control_state_protected":true,"participants":[fixture.run,base.join("review")]}),
        );
        directory(&base.join("review/reviewer"));
        directory(&base.join("review/reviewer/event-reports"));
        let reviewer = json!({"run_id":"review-run","worker":"reviewer","assignment":1,"round":1,"session":"review-session"});
        let verdict = json!({"version":1,"task":"parser-task","assignment":"task-review","generation":1,"candidate":head,"reviewer":reviewer,
            "result":outcome,"complete":matches!(outcome,"accepted"|"rejected"),"checks":["scope"],
            "findings":if outcome == "rejected" {json!([{"id":"f1","text":"fix parser, never execute $(touch /outside)","evidence":[]}])} else {json!([])},
            "blockers":[],"evidence":[],"regression_commit":null});
        let bytes = serde_json::to_vec(&verdict).unwrap();
        write(
            &base.join("review/reviewer/event-reports/assignment-0001.json"),
            &json!({
            "state":{"terminal_event":{"event_id":"review-run.reviewer.1","run_id":"review-run","worker_id":"reviewer","assignment_id":1,"round":1,"session_id":"review-session","reason":"completed","report_path":base.join("review-report.json")}},
            "review":{"sha256":digest(&bytes),"bytes":bytes}}),
        );
        let binding = Binding {
            session_id: "parent".into(),
            cwd: base.to_owned(),
            workers: vec![base.join("review-work")],
        };
        Inbox::register(&fixture.inbox, binding.clone()).unwrap();
        write(
            &base.join("authority.json"),
            &json!({"assignment":"task-review","generation":1,"enabled":true}),
        );
        let workspace = fixture.ok(&[
            "prepared-workspace",
            fixture.cwd.to_str().unwrap(),
            "--report",
            fixture.cwd.join("report.json").to_str().unwrap(),
        ]);
        write(
            &fixture.config,
            &json!({"version":1,"task":"parser-task","directory":fixture.route,"assignment":"task-review","generation":1,"candidate":head,
            "reviewer":reviewer,"reviewer_directory":base.join("review"),"reviewer_cwd":base.join("review-work"),"required_checks":["scope"],"artifacts":[],"regression_commit":null,
                "until":until,"actions":if !resume && !parent {json!({})} else {json!({"resume_rejected":resume,"return_to_parent":parent})},"max_corrections":1,"admission":base.join("pool"),"authority_file":base.join("authority.json"),
            "parent":{"inbox":fixture.inbox,"binding":binding},
                "producer":{"manifest":fixture.run.join("manifest.json"),"manifest_sha256":digest(&fs::read(fixture.run.join("manifest.json")).unwrap()),"executable_sha256":digest(&fs::read(&executable).unwrap()),
                "identity":{"run_id":"producer-run","worker":"producer","assignment":1,"round":1,"session":"original"},
                    "state_sha256":digest(&fs::read(fixture.run.join("producer/state.json")).unwrap()),"report_sha256":digest(&fs::read(fixture.cwd.join("report.json")).unwrap()),"checkpoint_sha256":digest(&fs::read(checkpoint).unwrap()),
                "session_sha256":digest(&fs::read(session).unwrap()),"prompt_sha256":digest(&fs::read(prompt).unwrap()),"head":head,
                "workspace_sha256":String::from_utf8(workspace.stdout).unwrap().trim(),"correction":"authorised parser correction only"}}),
        );
        fixture.ok(&["register-review", fixture.config.to_str().unwrap()]);
        fixture
    }

    fn route(&self) {
        self.ok(&["route-reviews", self.route.to_str().unwrap(), "--once"]);
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.route.join("state.json")).unwrap()).unwrap()
    }
}

#[test]
fn independent_actions_and_duplicate_replay() {
    for resume in [false, true] {
        for parent in [false, true] {
            let fixture = Fixture::new(resume, parent, "rejected");
            fixture.route();
            fixture.route();
            let calls = fs::read_to_string(fixture.root.path().join("calls")).unwrap_or_default();
            assert_eq!(
                calls.lines().count(),
                usize::from(resume),
                "{}",
                fixture.state()
            );
            if resume {
                let state: Value = serde_json::from_slice(
                    &fs::read(fixture.run.join("producer/state.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(state["state"], "done");
                assert_eq!(state["terminal_event"]["reason"], "completed");
                assert_eq!(state["session_id"], "original");
            }
            let events = Inbox::open(&fixture.inbox).unwrap().pending().unwrap();
            assert_eq!(
                events.len(),
                if parent {
                    if resume { 2 } else { 1 }
                } else {
                    0
                }
            );
            if parent {
                let notice = events[0].review.as_ref().unwrap();
                assert_eq!(notice.verdict.assignment, "task-review");
                assert!(notice.verdict.complete);
                assert!(
                    notice.verdict.findings[0]
                        .text
                        .contains("$(touch /outside)")
                );
            }
            assert!(!Path::new("/outside").exists());
            fixture.ok(&["review-status", fixture.route.to_str().unwrap()]);
        }
    }
}

#[test]
fn non_rejections_never_launch() {
    for result in ["accepted", "incomplete", "blocked", "error"] {
        let fixture = Fixture::new(true, true, result);
        fixture.route();
        assert!(!fixture.root.path().join("calls").exists());
        assert_eq!(
            Inbox::open(&fixture.inbox)
                .unwrap()
                .pending()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn drift_stop_and_capacity() {
    for scenario in ["drift", "stop", "capacity", "superseded"] {
        let fixture = Fixture::new(true, false, "rejected");
        match scenario {
            "drift" => fs::write(fixture.cwd.join("manual"), "preserve this").unwrap(),
            "stop" => {
                fixture.ok(&["review-disable", fixture.route.to_str().unwrap()]);
            }
            "capacity" => write(
                &fixture.root.path().join("pool/policy.json"),
                &json!({"version":1,"capacity":2,"root_reserved":1,"external_reserved":1,"accounting_complete":true,"control_state_protected":true,"participants":[fixture.run]}),
            ),
            "superseded" => write(
                &fixture.root.path().join("authority.json"),
                &json!({"assignment":"other","generation":2,"enabled":true}),
            ),
            _ => unreachable!(),
        }
        fixture.route();
        assert!(!fixture.root.path().join("calls").exists());
        if scenario == "capacity" {
            assert_eq!(fixture.state()["producer"], "queued_capacity");
        }
    }
}

#[test]
fn parent_unavailable_reconnect_and_changed_verdict() {
    let fixture = Fixture::new(true, true, "rejected");
    let detached = fixture.root.path().join("detached");
    fs::rename(&fixture.inbox, &detached).unwrap();
    fixture.route();
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(fixture.state()["parent"], "pending");
    fs::rename(detached, &fixture.inbox).unwrap();
    let source = fixture
        .root
        .path()
        .join("review/reviewer/event-reports/assignment-0001.json");
    let mut journal: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    journal["review"]["sha256"] = json!("b".repeat(64));
    write(&source, &journal);
    fixture.route();
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    assert_eq!(inbox.pending().unwrap().len(), 2);
    for event in inbox.pending().unwrap() {
        assert!(event.review.is_some());
        inbox.acknowledge(&event.event_id).unwrap();
    }
    fixture.route();
    assert!(inbox.pending().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn malformed_stale_partial_and_ambiguous_claim_refuse() {
    for scenario in [
        "malformed",
        "stale",
        "incomplete_rejection",
        "capture_partial",
        "claim",
        "archive_partial",
        "short_prompt",
        "budget",
        "session_drift",
        "report_drift",
        "executable_drift",
    ] {
        let fixture = Fixture::new(true, true, "rejected");
        let source = fixture
            .root
            .path()
            .join("review/reviewer/event-reports/assignment-0001.json");
        match scenario {
            "malformed" => write(&source, &json!({"invalid":true})),
            "stale" | "incomplete_rejection" => {
                let mut journal: Value =
                    serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
                let raw: Vec<u8> =
                    serde_json::from_value(journal["review"]["bytes"].clone()).unwrap();
                let mut verdict: Value = serde_json::from_slice(&raw).unwrap();
                if scenario == "stale" {
                    verdict["generation"] = json!(2);
                } else {
                    verdict["complete"] = json!(false);
                }
                let bytes = serde_json::to_vec(&verdict).unwrap();
                journal["review"] = json!({"sha256":digest(&bytes),"bytes":bytes});
                write(&source, &journal);
            }
            "capture_partial" => directory(&fixture.route.join("capture-pending")),
            "claim" => {
                let mut state = fixture.state();
                state["launch_claimed"] = json!(true);
                state["producer"] = json!("launch_claimed");
                write(&fixture.route.join("history/0001.json"), &state);
                write(&fixture.route.join("state.json"), &state);
            }
            "archive_partial" => {
                directory(&fixture.run.join("producer/followups"));
                directory(&fixture.run.join("producer/followups/preparing-001"));
            }
            "short_prompt" => fs::write(fixture.root.path().join("prompt"), "short").unwrap(),
            "budget" => {
                let path = fixture.run.join("manifest.json");
                let mut manifest: Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                manifest["max_rounds"] = json!(1);
                write(&path, &manifest);
            }
            "session_drift" => fs::write(
                fixture
                    .run
                    .join("producer/mush-data/sessions/original.json"),
                "changed",
            )
            .unwrap(),
            "report_drift" => {
                fs::write(fixture.cwd.join("report.json"), "{\"manual\":true}").unwrap()
            }
            "executable_drift" => {
                fs::write(fixture.root.path().join("worker"), "#!/bin/sh\nexit 0\n").unwrap()
            }
            _ => unreachable!(),
        }
        fixture.route();
        fixture.route();
        assert!(!fixture.root.path().join("calls").exists(), "{scenario}");
        assert!(
            fixture.state()["producer"]
                .as_str()
                .unwrap()
                .contains("input"),
            "{scenario}: {}",
            fixture.state()
        );
    }
}

#[test]
fn late_verdict_capacity_release_and_default_manual() {
    let fixture = Fixture::new(true, false, "rejected");
    let source = fixture
        .root
        .path()
        .join("review/reviewer/event-reports/assignment-0001.json");
    let saved = fs::read(&source).unwrap();
    fs::remove_file(&source).unwrap();
    fixture.route();
    assert!(!fixture.root.path().join("calls").exists());
    fs::write(&source, saved).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    let pool = fixture.root.path().join("pool/policy.json");
    let original = fs::read(&pool).unwrap();
    let mut policy: Value = serde_json::from_slice(&original).unwrap();
    policy["external_reserved"] = json!(2);
    write(&pool, &policy);
    fixture.route();
    assert_eq!(fixture.state()["producer"], "queued_capacity");
    fs::write(&pool, original).unwrap();
    fixture.route();
    assert!(fixture.root.path().join("calls").exists());
    let fixture = Fixture::new(false, false, "rejected");
    fixture.route();
    assert_eq!(fixture.state()["producer"], "manual");
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn unsafe_paths_and_duplicate_keys() {
    for scenario in ["symlink", "oversized", "duplicate"] {
        let fixture = Fixture::new(true, false, "rejected");
        let source = fixture
            .root
            .path()
            .join("review/reviewer/event-reports/assignment-0001.json");
        match scenario {
            "symlink" => {
                let original = fixture.root.path().join("original-journal");
                fs::rename(&source, &original).unwrap();
                std::os::unix::fs::symlink(original, &source).unwrap();
            }
            "oversized" => {
                fs::write(&source, vec![b'x'; 3 * 1024 * 1024 + 1]).unwrap();
            }
            "duplicate" => {
                fs::write(&source, b"{\"state\":{},\"state\":{}}").unwrap();
            }
            _ => unreachable!(),
        }
        fixture.route();
        assert!(!fixture.root.path().join("calls").exists());
        assert_eq!(fixture.state()["producer"], "needs_input_review");
    }
}

#[test]
fn concurrent_router_and_manual_follow_up_cannot_duplicate() {
    let fixture = Fixture::new_worker(true, false, "rejected", "sleep 1");
    let mut first = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .args(["route-reviews", fixture.route.to_str().unwrap(), "--once"])
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !fixture.run.join("review-reservation.json").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        !fixture
            .command(&["route-reviews", fixture.route.to_str().unwrap(), "--once"])
            .status
            .success()
    );
    assert!(
        !fixture
            .command(&[
                "follow-up",
                fixture.run.to_str().unwrap(),
                "producer",
                "--prompt-file",
                fixture.root.path().join("prompt").to_str().unwrap()
            ])
            .status
            .success()
    );
    assert!(first.wait().unwrap().success());
    fixture.route();
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn crash_after_spawn_preserves_claim_and_refuses_relaunch() {
    let fixture = Fixture::new_worker(
        true,
        true,
        "rejected",
        "printf '%s' \"$$\" >../spawned\nsleep 30",
    );
    let mut first = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .args(["route-reviews", fixture.route.to_str().unwrap(), "--once"])
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let marker = fixture.root.path().join("spawned");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    first.kill().unwrap();
    first.wait().unwrap();
    let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
    assert!(pid > 1);
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let process_journal = fixture.run.join("producer/process.json");
    if process_journal.exists() {
        fs::remove_file(process_journal).unwrap();
    }
    let ledger_path = fixture.root.path().join("pool/claims.json");
    let mut lost_receipt: Value = serde_json::from_slice(&fs::read(&ledger_path).unwrap()).unwrap();
    for claim in lost_receipt["claims"].as_object_mut().unwrap().values_mut() {
        claim["processes"] = json!({});
        claim["state"] = json!("reserved");
    }
    write(&ledger_path, &lost_receipt);
    fixture.route();
    assert_eq!(fixture.state()["producer"], "needs_input_launch_unknown");
    assert!(!fixture.root.path().join("calls").exists());
    assert!(
        !fixture
            .command(&["review-retry", fixture.route.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        !fixture
            .command(&["run", fixture.run.join("manifest.json").to_str().unwrap()])
            .status
            .success()
    );
    let ledger: Value =
        serde_json::from_slice(&fs::read(fixture.root.path().join("pool/claims.json")).unwrap())
            .unwrap();
    assert_eq!(ledger["claims"].as_object().unwrap().len(), 1);
    assert_ne!(
        ledger["claims"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["state"],
        "released"
    );
}

#[test]
fn stop_before_spawn_and_enable_are_observable() {
    let fixture = Fixture::new(true, true, "rejected");
    fixture.ok(&["review-disable", fixture.route.to_str().unwrap()]);
    fixture.route();
    assert!(!fixture.root.path().join("calls").exists());
    fixture.ok(&["review-enable", fixture.route.to_str().unwrap()]);
    fs::write(fixture.run.join("STOP"), "stop").unwrap();
    fixture.route();
    assert!(!fixture.root.path().join("calls").exists());
    assert_eq!(fixture.state()["producer"], "needs_input");
    fs::remove_file(fixture.run.join("STOP")).unwrap();
    fixture.route();
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        Inbox::open(&fixture.inbox)
            .unwrap()
            .pending()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn capture_retry_preserves_abandoned_evidence() {
    let fixture = Fixture::new(false, true, "rejected");
    directory(&fixture.route.join("capture-pending"));
    fs::write(fixture.route.join("capture-pending/verdict.json"), "short").unwrap();
    fixture.route();
    fixture.ok(&["review-retry", fixture.route.to_str().unwrap()]);
    fixture.route();
    assert_eq!(
        Inbox::open(&fixture.inbox)
            .unwrap()
            .pending()
            .unwrap()
            .len(),
        1
    );
    assert!(fs::read_dir(&fixture.route).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("abandoned-capture-")
    }));
}

#[test]
fn immutable_capture_survives_source_removal_and_ack_is_not_launch() {
    let fixture = Fixture::new(false, true, "accepted");
    fixture.route();
    let source = fixture
        .root
        .path()
        .join("review/reviewer/event-reports/assignment-0001.json");
    fs::remove_file(source).unwrap();
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    let event = inbox.pending().unwrap().remove(0);
    inbox.acknowledge(&event.event_id).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["parent"], "acknowledged");
    assert_eq!(fixture.state()["launch_claimed"], false);
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn registered_configuration_is_immutable() {
    let fixture = Fixture::new(false, false, "rejected");
    let path = fixture.route.join("config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["actions"]["resume_rejected"] = json!(true);
    write(&path, &config);
    assert!(
        !fixture
            .command(&["route-reviews", fixture.route.to_str().unwrap(), "--once"])
            .status
            .success()
    );
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn independent_reviewer_run_captures_verdict_before_late_routing() {
    let fixture = Fixture::new(true, true, "rejected");
    let root = fixture.root.path();
    let review = root.join("review");
    let journal = review.join("reviewer/event-reports/assignment-0001.json");
    fs::remove_file(&journal).unwrap();
    write(
        &review.join("event-run.json"),
        &json!({"run_id":"review-run"}),
    );
    let executable = root.join("reviewer");
    fs::write(&executable, r#"#!/bin/sh
set -eu
umask 077
status=
while [ "$#" -gt 0 ]; do
case "$1" in
--status-file) status=$2; shift 2;;
*) shift;;
esac
done
printf '{"state":"completed","session_id":"review-session"}' >"$status"
printf '{"state":"done","run_id":"%s","worker_id":"reviewer","assignment_id":%s,"round":%s}' "$MUSH_RUN_ID" "$MUSH_ASSIGNMENT_ID" "$MUSH_ROUND" >report.json
printf '{"version":1,"task":"parser-task","assignment":"task-review","generation":1,"candidate":"1111111111111111111111111111111111111111","reviewer":{"run_id":"%s","worker":"reviewer","assignment":%s,"round":%s,"session":"review-session"},"result":"rejected","complete":true,"checks":["scope"],"findings":[{"id":"f1","text":"correct parser","evidence":[]}],"blockers":[],"evidence":[],"regression_commit":null}' "$MUSH_RUN_ID" "$MUSH_ASSIGNMENT_ID" "$MUSH_ROUND" >../verdict.json
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = root.join("review-manifest.json");
    write(
        &manifest,
        &json!({"directory":review,"executable":executable,"until":chrono::Utc::now()+chrono::Duration::minutes(5),"concurrency":1,
        "attempt_seconds":5,"silence_seconds":3,"max_rounds":1,"model":"fixture","fallback_model":"fixture","status_file":true,
        "workers":[{"name":"reviewer","cwd":root.join("review-work"),"prompt":root.join("prompt"),"report":root.join("review-work/report.json"),"review_verdict":root.join("verdict.json"),"pause_seconds":0}]}),
    );
    fixture.route();
    assert!(!root.join("calls").exists());
    fixture.ok(&["run", manifest.to_str().unwrap()]);
    let captured: Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    assert!(captured["review"]["bytes"].is_array());
    fs::write(root.join("verdict.json"), "replaced worker output").unwrap();
    fixture.route();
    assert_eq!(
        fs::read_to_string(root.join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    let events = inbox.pending().unwrap();
    assert_eq!(events.len(), 2);
    inbox.acknowledge(&events[1].event_id).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["parent"], "queued");
    inbox.acknowledge(&events[0].event_id).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["parent"], "acknowledged");
}

#[test]
fn schema_examples_are_executable() {
    let fixture = Fixture::new(true, true, "rejected");
    fixture.route();
    assert_eq!(fixture.state()["producer"], "attempt_finished");
    for (label, path) in [
        ("assignment", fixture.config.clone()),
        ("producer_manifest", fixture.run.join("manifest.json")),
        (
            "admission_policy",
            fixture.root.path().join("pool/policy.json"),
        ),
        ("authority", fixture.root.path().join("authority.json")),
        ("verdict", fixture.route.join("capture/verdict.json")),
        ("parent_event", fixture.route.join("parent-events.json")),
        ("state", fixture.route.join("state.json")),
    ] {
        let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        println!("{}", json!({"kind":label,"value":value}));
    }
}

#[test]
fn bound_regression_evidence_is_captured_and_worker_paths_cannot_redirect_it() {
    for malicious in [false, true] {
        let mut fixture = Fixture::new(false, true, "rejected");
        fixture.route = fixture.root.path().join("evidence-route");
        let artifact = fixture.root.path().join("approved-test.patch");
        fs::write(&artifact, "synthetic regression test").unwrap();
        let sha = digest(&fs::read(&artifact).unwrap());
        let mut config: Value =
            serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
        config["directory"] = json!(fixture.route);
        config["task"] = json!("evidence-task");
        config["authority_file"] = json!(fixture.root.path().join("evidence-authority.json"));
        write(
            &fixture.root.path().join("evidence-authority.json"),
            &json!({"assignment":"task-review","generation":1,"enabled":true}),
        );
        config["artifacts"] = json!([{"id":"regression","path":artifact,"sha256":sha}]);
        write(&fixture.config, &config);
        fixture.ok(&["register-review", fixture.config.to_str().unwrap()]);
        let source = fixture
            .root
            .path()
            .join("review/reviewer/event-reports/assignment-0001.json");
        let mut journal: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
        let bytes: Vec<u8> = serde_json::from_value(journal["review"]["bytes"].clone()).unwrap();
        let mut verdict: Value = serde_json::from_slice(&bytes).unwrap();
        verdict["evidence"] = json!([{"id":"regression","sha256":sha}]);
        verdict["task"] = json!("evidence-task");
        verdict["findings"][0]["evidence"] = json!(["regression"]);
        if malicious {
            verdict["evidence"][0]["path"] = json!("/outside");
        }
        let bytes = serde_json::to_vec(&verdict).unwrap();
        journal["review"] = json!({"sha256":digest(&bytes),"bytes":bytes});
        write(&source, &journal);
        fixture.route();
        let captured = fixture.route.join("capture/regression.evidence");
        if malicious {
            assert!(!captured.exists());
            assert_eq!(fixture.state()["producer"], "needs_input_review");
        } else {
            assert_eq!(digest(&fs::read(&captured).unwrap()), sha);
            fs::write(&artifact, "changed after capture").unwrap();
            fixture.route();
            assert_eq!(digest(&fs::read(captured).unwrap()), sha);
            let event = Inbox::open(&fixture.inbox)
                .unwrap()
                .pending()
                .unwrap()
                .remove(0);
            assert_eq!(
                event.review.unwrap().verdict.findings[0].evidence,
                vec!["regression"]
            );
        }
    }
}

#[test]
fn disabling_an_active_correction_cancels_without_retry() {
    let fixture = Fixture::new_worker(
        true,
        true,
        "rejected",
        "printf '%s' \"$$\" >../spawned\nsleep 30",
    );
    let mut router = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .args(["route-reviews", fixture.route.to_str().unwrap(), "--once"])
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !fixture.root.path().join("spawned").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fixture.ok(&["review-disable", fixture.route.to_str().unwrap()]);
    assert!(router.wait().unwrap().success());
    assert!(!fixture.root.path().join("calls").exists());
    let producer: Value =
        serde_json::from_slice(&fs::read(fixture.run.join("producer/state.json")).unwrap())
            .unwrap();
    assert_eq!(producer["terminal_event"]["reason"], "cancelled");
    fixture.route();
    assert!(!fixture.root.path().join("calls").exists());
}

#[expect(clippy::unwrap_used)]
impl Fixture {
    fn reviewer_history(&self) {
        let root = self.root.path();
        let run = root.join("review");
        let dir = run.join("reviewer");
        for path in [dir.join("mush-data"), dir.join("mush-data/sessions")] {
            directory(&path);
        }
        write(
            &dir.join("mush-data/sessions/review-session.json"),
            &json!({"conversation":["original independent review"]}),
        );
        write(
            &dir.join("round-001.json"),
            &json!({"state":"completed","session_id":"review-session"}),
        );
        write(&run.join("event-run.json"), &json!({"run_id":"review-run"}));
        write(&run.join("supervisor.json"), &json!({"state":"stopped"}));
        write(
            &root.join("review-work/report.json"),
            &json!({"state":"done"}),
        );
        let event = json!({"event_id":"review-run.reviewer.1","run_id":"review-run","worker_id":"reviewer","assignment_id":1,"round":1,"session_id":"review-session","reason":"completed","report_path":root.join("review-work/report.json")});
        write(
            &dir.join("state.json"),
            &json!({"state":"done","round":1,"assignment_id":1,"model":"fixture","session_id":"review-session","terminal_event":event}),
        );
        let executable = root.join("review-worker");
        fs::write(&executable,r#"#!/bin/sh
set -eu
umask 077
status= resume= prompt=
while [ "$#" -gt 0 ]; do
case "$1" in
--status-file) status=$2; shift 2;;
--continue) resume=$2; shift 2;;
-p) prompt=$2; shift 2;;
*) shift;;
esac
done
test "$resume" = review-session
test "$(cat "$MUSH_DATA_DIR/sessions/review-session.json")" = '{"conversation":["original independent review"]}'
printf '%s' "$prompt" >../review-received
printf '%s:%s\n' "$resume" "$PWD" >>../review-calls
printf '{"state":"completed","session_id":"review-session"}' >"$status"
printf '{"state":"done","run_id":"%s","worker_id":"reviewer","assignment_id":%s,"round":%s}' "$MUSH_RUN_ID" "$MUSH_ASSIGNMENT_ID" "$MUSH_ROUND" >report.json
printf '{"version":1,"task":"parser-task","assignment":"task-review","generation":2,"candidate":"2222222222222222222222222222222222222222","reviewer":{"run_id":"review-run","worker":"reviewer","assignment":2,"round":2,"session":"review-session"},"result":"accepted","complete":true,"checks":["scope"],"findings":[],"blockers":[],"evidence":[],"regression_commit":null}' >../next-verdict.json
"#).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        write(
            &run.join("manifest.json"),
            &json!({"directory":run,"executable":executable,"until":chrono::Utc::now()+chrono::Duration::minutes(10),"concurrency":1,"attempt_seconds":5,"silence_seconds":3,"max_rounds":5,"model":"fixture","fallback_model":"fixture","status_file":true,"durable_events":true,"admission":root.join("pool"),"workers":[{"name":"reviewer","cwd":root.join("review-work"),"prompt":root.join("prompt"),"report":root.join("review-work/report.json"),"review_verdict":root.join("next-verdict.json"),"pause_seconds":0}]}),
        );
    }

    fn snapshot(&self, run: &Path, worker: &str) -> Value {
        let manifest_path = run.join("manifest.json");
        let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let dir = run.join(worker);
        let state: Value =
            serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap();
        let round = state["round"].as_u64().unwrap();
        let session = state["session_id"].as_str().unwrap();
        let job = &manifest["workers"][0];
        let workspace = self.ok(&[
            "prepared-workspace",
            job["cwd"].as_str().unwrap(),
            "--report",
            job["report"].as_str().unwrap(),
        ]);
        json!({"round":round,"assignment":state["terminal_event"]["assignment_id"],"manifest_sha256":digest(&fs::read(&manifest_path).unwrap()),"executable_sha256":digest(&fs::read(manifest["executable"].as_str().unwrap()).unwrap()),"state_sha256":digest(&fs::read(dir.join("state.json")).unwrap()),"report_sha256":digest(&fs::read(job["report"].as_str().unwrap()).unwrap()),"checkpoint_sha256":digest(&fs::read(dir.join(format!("round-{round:03}.json"))).unwrap()),"session_sha256":digest(&fs::read(dir.join("mush-data/sessions").join(format!("{session}.json"))).unwrap()),"prompt_sha256":digest(&fs::read(job["prompt"].as_str().unwrap()).unwrap()),"head":"1".repeat(40),"workspace_sha256":String::from_utf8(workspace.stdout).unwrap().trim(),"brief":"continue original independent review on the prepared candidate"})
    }

    fn pairing(&self) -> Value {
        serde_json::from_slice(
            &self
                .ok(&[
                    "review-pair",
                    self.root.path().join("pool").to_str().unwrap(),
                    "parser-task",
                ])
                .stdout,
        )
        .unwrap()
    }

    fn continuation(&self) -> PathBuf {
        let root = self.root.path();
        let jj = root.join("bin/jj");
        fs::write(&jj,format!("#!/bin/sh\nset -eu\ncase \"$*\" in *'conflicts() & @'*) exit 0;; esac\nprintf '%s' '{}'\n","2".repeat(40))).unwrap();
        let path = root.join("next.json");
        let mut producer = self.snapshot(&self.run, "producer");
        let mut reviewer = self.snapshot(&root.join("review"), "reviewer");
        producer["head"] = json!("2".repeat(40));
        reviewer["head"] = json!("2".repeat(40));
        write(
            &path,
            &json!({"directory":root.join("next-route"),"expected":self.pairing()["receipt"],"candidate":"2".repeat(40),"until":chrono::Utc::now()+chrono::Duration::minutes(5),"producer":producer,"reviewer":reviewer,"artifacts":[],"regression_commit":null}),
        );
        write(
            &root.join("authority.json"),
            &json!({"assignment":"task-review","generation":2,"enabled":true}),
        );
        path
    }
}

#[test]
fn task_continuation_reuses_original_reviewer_after_process_restart() {
    let fixture = Fixture::new_worker(true, true, "rejected", "sleep 0.1");
    fixture.reviewer_history();
    let offline = fixture.root.path().join("parent-offline");
    fs::rename(&fixture.inbox, &offline).unwrap();
    fixture.route();
    let original = fixture.pairing()["history"]["pair"].clone();
    let preparation = fixture.continuation();
    fixture.ok(&[
        "continue-review",
        fixture.route.to_str().unwrap(),
        preparation.to_str().unwrap(),
    ]);
    fs::rename(offline, &fixture.inbox).unwrap();
    fixture.ok(&[
        "continue-review",
        fixture.route.to_str().unwrap(),
        preparation.to_str().unwrap(),
    ]);
    fixture.route();
    fixture.route();
    assert!(
        fixture.root.path().join("review-calls").exists(),
        "{}",
        fs::read_to_string(fixture.root.path().join("next-route/state.json")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("review-calls")).unwrap(),
        format!(
            "review-session:{}\n",
            fixture.root.path().join("review-work").display()
        )
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let pairing = fixture.pairing();
    assert_eq!(pairing["history"]["pair"], original);
    assert_eq!(
        pairing["history"]["generations"].as_array().unwrap().len(),
        2
    );
    let events = Inbox::open(&fixture.inbox).unwrap().pending().unwrap();
    assert!(events.iter().any(
        |e| e.review.as_ref().is_some_and(|n| n.verdict.generation == 2
            && n.pairing.as_ref().is_some_and(
                |p| p.task == "parser-task" && p.pair.reviewer.session == "review-session"
            ))
    ));
    assert!(fixture.route.join("capture/verdict.json").exists());
    let request: Value = serde_json::from_slice(&fs::read(preparation).unwrap()).unwrap();
    println!("{}", json!({"kind":"continuation","value":request}));
    println!("{}", json!({"kind":"pairing","value":pairing}));
    assert!(
        events.iter().any(|event| event
            .review
            .as_ref()
            .is_some_and(|n| n.verdict.generation == 1
                && n.pairing
                    .as_ref()
                    .is_some_and(|p| p.pair.reviewer.session == "review-session")))
    );
}

#[test]
fn manual_and_parent_only_continuations_keep_pair_without_launching() {
    for parent in [false, true] {
        let fixture = Fixture::new(false, parent, "rejected");
        fixture.reviewer_history();
        fixture.route();
        let original = fixture.pairing()["history"]["pair"].clone();
        let preparation = fixture.continuation();
        fixture.ok(&[
            "continue-review",
            fixture.route.to_str().unwrap(),
            preparation.to_str().unwrap(),
        ]);
        fixture.route();
        assert!(!fixture.root.path().join("review-calls").exists());
        assert_eq!(fixture.pairing()["history"]["pair"], original);
        let state: Value = serde_json::from_slice(
            &fs::read(fixture.root.path().join("next-route/state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["reviewer_action"], "manual");
    }
}

#[test]
fn ordinary_registration_cannot_replace_pair_or_bypass_task_by_directory() {
    for field in [
        "session",
        "worker",
        "directory",
        "workspace",
        "producer",
        "generation",
        "task",
    ] {
        let fixture = Fixture::new(false, false, "accepted");
        fixture.route();
        let mut config: Value =
            serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
        config["directory"] = json!(fixture.root.path().join("different-directory"));
        match field {
            "session" => config["reviewer"]["session"] = json!("fresh-session"),
            "worker" => config["reviewer"]["worker"] = json!("fresh-reviewer"),
            "directory" => config["reviewer_directory"] = json!(fixture.run),
            "workspace" => config["reviewer_cwd"] = json!(fixture.cwd),
            "producer" => config["producer"]["identity"]["session"] = json!("fresh-producer"),
            "generation" => config["generation"] = json!(2),
            "task" => config["task"] = json!("pretend-new-task"),
            _ => unreachable!(),
        }
        let path = fixture.root.path().join("replacement.json");
        write(&path, &config);
        assert!(
            !fixture
                .command(&["register-review", path.to_str().unwrap()])
                .status
                .success(),
            "{field}"
        );
        assert_eq!(
            fixture.pairing()["history"]["generations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn busy_missing_blocked_reviewer_does_not_get_replaced() {
    for scenario in [
        "busy",
        "busy_session",
        "missing",
        "blocked",
        "stopped",
        "capacity",
        "workspace",
    ] {
        let fixture = Fixture::new_worker(true, false, "rejected", "sleep 0.1");
        fixture.reviewer_history();
        fixture.route();
        let preparation = fixture.continuation();
        fixture.ok(&[
            "continue-review",
            fixture.route.to_str().unwrap(),
            preparation.to_str().unwrap(),
        ]);
        let run = fixture.root.path().join("review");
        let mut lock = None;
        match scenario {
            "busy" => {
                let file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(run.join("supervisor.lock"))
                    .unwrap();
                file.lock().unwrap();
                lock = Some(file);
            }
            "busy_session" => {
                lock = Some(
                    mush_supervisor::admission::session_lock(
                        &run.join("reviewer/mush-data/sessions"),
                        "review-session",
                        None,
                    )
                    .unwrap(),
                );
            }
            "missing" => {
                fs::remove_file(run.join("reviewer/mush-data/sessions/review-session.json"))
                    .unwrap()
            }
            "blocked" => {
                let path = run.join("reviewer/state.json");
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value["state"] = json!("blocked");
                write(&path, &value);
            }
            "stopped" => fs::write(run.join("STOP"), "stop").unwrap(),
            "workspace" => fs::rename(
                fixture.root.path().join("review-work"),
                fixture.root.path().join("unavailable-review-work"),
            )
            .unwrap(),
            "capacity" => {
                let path = fixture.root.path().join("pool/policy.json");
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value["external_reserved"] = json!(2);
                write(&path, &value);
            }
            _ => unreachable!(),
        }
        fixture.route();
        assert!(!fixture.root.path().join("review-calls").exists());
        let state: Value = serde_json::from_slice(
            &fs::read(fixture.root.path().join("next-route/state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            state["reviewer_action"],
            if scenario == "busy" || scenario == "busy_session" {
                "queued_busy"
            } else if scenario == "capacity" {
                "queued_capacity"
            } else {
                "needs_input"
            }
        );
        drop(lock);
    }
}

#[test]
fn explicit_reassignment_is_receipt_bound_idempotent_and_cannot_bypass_denial() {
    for denied in [false, true] {
        let fixture = Fixture::new(false, false, if denied { "blocked" } else { "accepted" });
        fixture.reviewer_history();
        fixture.route();
        let replacement = Fixture::new(false, false, "accepted");
        replacement.reviewer_history();
        let replacement_state = replacement.root.path().join("review/reviewer/state.json");
        let mut saved: Value =
            serde_json::from_slice(&fs::read(&replacement_state).unwrap()).unwrap();
        saved["session_id"] = json!("intentional-new-reviewer");
        saved["terminal_event"]["session_id"] = json!("intentional-new-reviewer");
        write(&replacement_state, &saved);
        write(
            &replacement
                .root
                .path()
                .join("review/reviewer/round-001.json"),
            &json!({"state":"completed","session_id":"intentional-new-reviewer"}),
        );
        fs::rename(
            replacement
                .root
                .path()
                .join("review/reviewer/mush-data/sessions/review-session.json"),
            replacement
                .root
                .path()
                .join("review/reviewer/mush-data/sessions/intentional-new-reviewer.json"),
        )
        .unwrap();
        let original = fixture.pairing();
        let mut pair = original["history"]["pair"].clone();
        pair["reviewer"] = replacement.pairing()["history"]["pair"]["reviewer"].clone();
        pair["reviewer"]["session"] = json!("intentional-new-reviewer");
        let request = json!({"task":"parser-task","request":"replace-reviewer","expected":original["receipt"],"generation":1,"reason":"operator accepts loss of the previous reviewer context","pair":pair});
        let requests = fixture.root.path().join("pool/reassignments");
        directory(&requests);
        let path = requests.join("request.json");
        write(&path, &request);
        let args = [
            "review-reassign".to_owned(),
            fixture
                .root
                .path()
                .join("pool")
                .to_str()
                .unwrap()
                .to_owned(),
            path.to_str().unwrap().to_owned(),
        ];
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        let result = fixture.command(&args);
        assert_eq!(
            result.status.success(),
            !denied,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if !denied {
            fixture.ok(&args);
            let changed = fixture.pairing();
            assert_eq!(changed["history"]["original"], original["history"]["pair"]);
            assert_eq!(
                changed["history"]["pair"]["reviewer"]["session"],
                "intentional-new-reviewer"
            );
            assert_eq!(
                changed["history"]["reassignments"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            let mut stale = request.clone();
            stale["request"] = json!("stale-request");
            write(&path, &stale);
            assert!(!fixture.command(&args).status.success());
            let mut wrong_actor = request.clone();
            wrong_actor["actor"] = json!("reviewer");
            write(&path, &wrong_actor);
            assert!(!fixture.command(&args).status.success());
            let preparation = fixture.continuation();
            let mut next: Value = serde_json::from_slice(&fs::read(&preparation).unwrap()).unwrap();
            next["reviewer"] =
                replacement.snapshot(&replacement.root.path().join("review"), "reviewer");
            next["reviewer"]["head"] = json!("2".repeat(40));
            write(&preparation, &next);
            fixture.ok(&[
                "continue-review",
                fixture.route.to_str().unwrap(),
                preparation.to_str().unwrap(),
            ]);
            let registered: Value = serde_json::from_slice(
                &fs::read(fixture.root.path().join("next-route/config.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                registered["reviewer"]["session"],
                "intentional-new-reviewer"
            );
            assert_eq!(
                registered["reviewer_directory"],
                json!(replacement.root.path().join("review"))
            );
        }
        let worker_path = fixture.root.path().join("review-work/reassign.json");
        write(&worker_path, &request);
        assert!(
            !fixture
                .command(&[
                    "review-reassign",
                    fixture.root.path().join("pool").to_str().unwrap(),
                    worker_path.to_str().unwrap()
                ])
                .status
                .success()
        );
    }
}

#[test]
fn concurrent_generation_registration_and_crash_replay_preserve_history() {
    let fixture = Fixture::new(false, false, "accepted");
    fixture.reviewer_history();
    fixture.route();
    let preparation = fixture.continuation();
    let start = || {
        Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
            .args([
                "continue-review",
                fixture.route.to_str().unwrap(),
                preparation.to_str().unwrap(),
            ])
            .env("HOME", fixture.root.path())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fixture.root.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut first = start();
    let mut second = start();
    let one = first.wait().unwrap();
    let two = second.wait().unwrap();
    assert!(one.success() || two.success());
    fixture.ok(&[
        "continue-review",
        fixture.route.to_str().unwrap(),
        preparation.to_str().unwrap(),
    ]);
    let ledger_path = fixture.root.path().join("pool/review-tasks.json");
    let mut ledger: Value = serde_json::from_slice(&fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["tasks"]["parser-task"]["generations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    ledger["tasks"]["parser-task"]["generations"]
        .as_array_mut()
        .unwrap()
        .pop();
    write(&ledger_path, &ledger);
    fixture.ok(&[
        "continue-review",
        fixture.route.to_str().unwrap(),
        preparation.to_str().unwrap(),
    ]);
    assert_eq!(
        fixture.pairing()["history"]["generations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut old: Value = serde_json::from_slice(&fs::read(&preparation).unwrap()).unwrap();
    old["directory"] = json!(fixture.root.path().join("stale-third-route"));
    write(&preparation, &old);
    assert!(
        !fixture
            .command(&[
                "continue-review",
                fixture.route.to_str().unwrap(),
                preparation.to_str().unwrap()
            ])
            .status
            .success()
    );
}

#[test]
fn unrelated_task_can_establish_a_separate_pair_in_shared_registry() {
    let fixture = Fixture::new(false, false, "accepted");
    let other = Fixture::new(false, false, "accepted");
    let mut config: Value = serde_json::from_slice(&fs::read(&other.config).unwrap()).unwrap();
    config["task"] = json!("unrelated-task");
    config["admission"] = json!(fixture.root.path().join("pool"));
    config["directory"] = json!(other.root.path().join("unrelated-route"));
    let path = other.root.path().join("unrelated.json");
    write(&path, &config);
    other.ok(&["register-review", path.to_str().unwrap()]);
    let first = fixture.pairing();
    let second: Value = serde_json::from_slice(
        &other
            .ok(&[
                "review-pair",
                fixture.root.path().join("pool").to_str().unwrap(),
                "unrelated-task",
            ])
            .stdout,
    )
    .unwrap();
    assert_ne!(
        first["history"]["pair"]["reviewer"]["workspace"],
        second["history"]["pair"]["reviewer"]["workspace"]
    );
}

#[test]
fn running_owner_discovers_prepared_successor_without_another_wake() {
    let fixture = Fixture::new_worker(true, false, "rejected", "sleep 0.1");
    fixture.reviewer_history();
    let mut router = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .args(["route-reviews", fixture.route.to_str().unwrap()])
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while fixture.state()["producer"] != "attempt_finished" {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let preparation = fixture.continuation();
    fixture.ok(&[
        "continue-review",
        fixture.route.to_str().unwrap(),
        preparation.to_str().unwrap(),
    ]);
    while !fixture.root.path().join("review-calls").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    fs::write(fixture.root.path().join("next-route/STOP"), "stop").unwrap();
    assert!(router.wait().unwrap().success());
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("review-calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn stopped_generation_and_candidate_mismatch_cannot_advance() {
    for scenario in ["stop", "disabled", "candidate"] {
        let fixture = Fixture::new(false, false, "accepted");
        fixture.reviewer_history();
        fixture.route();
        let preparation = fixture.continuation();
        match scenario {
            "stop" => fs::write(fixture.route.join("STOP"), "stop").unwrap(),
            "disabled" => {
                fixture.ok(&["review-disable", fixture.route.to_str().unwrap()]);
            }
            "candidate" => {
                let mut value: Value =
                    serde_json::from_slice(&fs::read(&preparation).unwrap()).unwrap();
                value["candidate"] = json!("3".repeat(40));
                write(&preparation, &value);
            }
            _ => unreachable!(),
        }
        assert!(
            !fixture
                .command(&[
                    "continue-review",
                    fixture.route.to_str().unwrap(),
                    preparation.to_str().unwrap()
                ])
                .status
                .success()
        );
        assert_eq!(
            fixture.pairing()["history"]["generations"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(!fixture.root.path().join("review-calls").exists());
    }
}

#[test]
fn reviewer_launch_crash_keeps_original_session_and_never_replays() {
    let fixture = Fixture::new_worker(true, false, "rejected", "sleep 0.1");
    fixture.reviewer_history();
    fixture.route();
    let worker = fixture.root.path().join("review-worker");
    let script = fs::read_to_string(&worker).unwrap();
    fs::write(
        &worker,
        script.replace(
            "test \"$resume\" = review-session",
            "printf '%s' \"$$\" >../review-spawned\nsleep 30\ntest \"$resume\" = review-session",
        ),
    )
    .unwrap();
    let preparation = fixture.continuation();
    fixture.ok(&[
        "continue-review",
        fixture.route.to_str().unwrap(),
        preparation.to_str().unwrap(),
    ]);
    let mut router = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .args(["route-reviews", fixture.route.to_str().unwrap(), "--once"])
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let marker = fixture.root.path().join("review-spawned");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    router.kill().unwrap();
    router.wait().unwrap();
    let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
    assert!(pid > 1);
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    fixture.route();
    fixture.route();
    let state: Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join("next-route/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(state["reviewer_action"], "needs_input_launch_unknown");
    assert_eq!(
        fixture.pairing()["history"]["pair"]["reviewer"]["session"],
        "review-session"
    );
    assert!(!fixture.root.path().join("review-calls").exists());
    assert!(
        !fixture
            .command(&[
                "review-retry",
                fixture.root.path().join("next-route").to_str().unwrap()
            ])
            .status
            .success()
    );
}

#[test]
fn changed_producer_run_identity_cannot_resume_bound_session() {
    let fixture = Fixture::new(true, true, "rejected");
    write(
        &fixture.run.join("event-run.json"),
        &json!({"run_id":"replacement-run"}),
    );
    fixture.route();
    assert!(
        !fixture.root.path().join("calls").exists(),
        "{}",
        fixture.state()
    );
}

#[test]
fn partial_capture_cannot_deliver_an_unbound_verdict() {
    let fixture = Fixture::new(false, true, "rejected");
    let source = fixture
        .root
        .path()
        .join("review/reviewer/event-reports/assignment-0001.json");
    let journal: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    let bytes: Vec<u8> = serde_json::from_value(journal["review"]["bytes"].clone()).unwrap();
    let mut verdict: Value = serde_json::from_slice(&bytes).unwrap();
    verdict["generation"] = json!(2);
    verdict["candidate"] = json!("2".repeat(40));
    directory(&fixture.route.join("capture"));
    write(&fixture.route.join("capture/verdict.json"), &verdict);
    fs::remove_file(source).unwrap();
    fixture.route();
    let pending = Inbox::open(&fixture.inbox).unwrap().pending().unwrap();
    assert!(pending.is_empty(), "{pending:?}");
}

#[test]
fn surviving_correction_retains_workspace_lock_after_router_crash() {
    let fixture = Fixture::new_worker(
        true,
        false,
        "rejected",
        "printf '%s' \"$$\" >../spawned\nsleep 30",
    );
    let mut router = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .args(["route-reviews", fixture.route.to_str().unwrap(), "--once"])
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let marker = fixture.root.path().join("spawned");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        if std::time::Instant::now() >= deadline {
            router.kill().unwrap();
            router.wait().unwrap();
            panic!("correction did not start");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let pid: i32 = fs::read_to_string(marker).unwrap().parse().unwrap();
    assert!(pid > 1);
    router.kill().unwrap();
    router.wait().unwrap();
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    let lock_held =
        mush_supervisor::admission::execution_lock(&fixture.root.path().join("pool"), &fixture.cwd)
            .is_err();
    let run_lock = fs::OpenOptions::new()
        .append(true)
        .open(fixture.run.join("supervisor.lock"))
        .unwrap();
    let run_held = run_lock.try_lock().is_err();
    let session_held = mush_supervisor::admission::session_lock(
        &fixture.run.join("producer/mush-data/sessions"),
        "original",
        None,
    )
    .is_err();
    fixture.route();
    let state = fixture.state();
    let ledger_path = fixture.root.path().join("pool/claims.json");
    let mut ledger: Value = serde_json::from_slice(&fs::read(&ledger_path).unwrap()).unwrap();
    let claim = ledger["claims"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let release_with_receipt = fixture.command(&[
        "admission-release",
        fixture.root.path().join("pool").to_str().unwrap(),
        &claim,
        "--confirm-processes-exited",
    ]);
    ledger["claims"][&claim]["processes"] = json!({});
    ledger["claims"][&claim]["state"] = json!("reserved");
    write(&ledger_path, &ledger);
    let release = fixture.command(&[
        "admission-release",
        fixture.root.path().join("pool").to_str().unwrap(),
        &claim,
        "--confirm-processes-exited",
    ]);
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    assert!(alive);
    assert!(!release_with_receipt.status.success());
    assert!(
        run_held && session_held,
        "surviving correction lost run or session ownership"
    );
    assert_eq!(state["producer"], "needs_input_launch_unknown");
    assert!(
        lock_held && !release.status.success(),
        "live correction: workspace lock held={lock_held}, admission release={}",
        release.status
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let release = fixture.command(&[
            "admission-release",
            fixture.root.path().join("pool").to_str().unwrap(),
            &claim,
            "--confirm-processes-exited",
        ]);
        if release.status.success() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "cleanup did not become verifiable: {}",
            String::from_utf8_lossy(&release.stderr)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        mush_supervisor::admission::execution_lock(&fixture.root.path().join("pool"), &fixture.cwd)
            .is_ok()
    );
    let released: Value = serde_json::from_slice(&fs::read(ledger_path).unwrap()).unwrap();
    assert_eq!(released["claims"][&claim]["state"], "released");
}

#[test]
fn prepared_real_jj_workspace_resumes_without_revision_mutation() {
    let mut fixture = Fixture::new(true, false, "rejected");
    fs::remove_file(fixture.root.path().join("bin/jj")).unwrap();
    let output = Command::new("jj")
        .args(["git", "init", "--no-colocate"])
        .arg(&fixture.cwd)
        .env("HOME", fixture.root.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let revision = || {
        let output = Command::new("jj")
            .current_dir(&fixture.cwd)
            .args([
                "--ignore-working-copy",
                "--color=never",
                "--no-pager",
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "commit_id",
            ])
            .env("HOME", fixture.root.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let head = revision();
    assert_eq!(head.len(), 40);
    let mut config: Value = serde_json::from_slice(&fs::read(&fixture.config).unwrap()).unwrap();
    fixture.route = fixture.root.path().join("real-route");
    config["directory"] = json!(fixture.route);
    config["candidate"] = json!(head);
    config["producer"]["head"] = json!(head);
    config["task"] = json!("real-jj-task");
    config["authority_file"] = json!(fixture.root.path().join("real-authority.json"));
    write(
        &fixture.root.path().join("real-authority.json"),
        &json!({"assignment":"task-review","generation":1,"enabled":true}),
    );
    write(&fixture.config, &config);
    let source = fixture
        .root
        .path()
        .join("review/reviewer/event-reports/assignment-0001.json");
    let mut journal: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    let raw: Vec<u8> = serde_json::from_value(journal["review"]["bytes"].clone()).unwrap();
    let mut verdict: Value = serde_json::from_slice(&raw).unwrap();
    verdict["candidate"] = json!(head);
    verdict["task"] = json!("real-jj-task");
    let raw = serde_json::to_vec(&verdict).unwrap();
    journal["review"] = json!({"sha256":digest(&raw),"bytes":raw});
    write(&source, &journal);
    fixture.ok(&["register-review", fixture.config.to_str().unwrap()]);
    fixture.route();
    assert_eq!(fixture.state()["producer"], "attempt_finished");
    assert_eq!(revision(), head);
}

#[test]
fn parent_receives_updated_pending_action_status() {
    let fixture = Fixture::new(true, true, "rejected");
    let policy = fixture.root.path().join("pool/policy.json");
    let mut full: Value = serde_json::from_slice(&fs::read(&policy).unwrap()).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    fixture.route();
    assert_eq!(fixture.state()["producer"], "queued_capacity");
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    let events = inbox.pending().unwrap();
    assert!(
        events.iter().any(|event| event
            .review
            .as_ref()
            .is_some_and(|notice| notice.producer_action == "queued_capacity")),
        "{events:?}"
    );
}

#[test]
fn disable_is_durable_while_launch_transaction_is_owned() {
    let fixture = Fixture::new(true, true, "rejected");
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(fixture.route.join("transaction.lock"))
        .unwrap();
    lock.lock().unwrap();
    fixture.ok(&["review-disable", fixture.route.to_str().unwrap()]);
    assert!(fixture.route.join("DISABLED").exists());
    drop(lock);
    fixture.route();
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn late_run_identity_drift_is_rejected_at_final_launch() {
    let fixture = Fixture::new(true, false, "rejected");
    fs::write(fixture.root.path().join("bin/jj"),format!("#!/bin/sh\nset -eu\ncase \"$*\" in *'conflicts() & @'*) exit 0;; esac\nprintf '%s' '{{\"run_id\":\"replacement-run\"}}' >'{}'\nprintf '%s' '{}'\n",fixture.run.join("event-run.json").display(),"1".repeat(40))).unwrap();
    fixture.route();
    assert!(!fixture.root.path().join("calls").exists());
    assert_eq!(fixture.state()["launch_claimed"], true);
    assert!(
        fixture.state()["detail"]
            .as_str()
            .unwrap()
            .contains("run identity changed")
    );
}

#[test]
fn parent_status_transitions_are_bounded_durable_and_ack_independent() {
    let fixture = Fixture::new(true, true, "rejected");
    let policy = fixture.root.path().join("pool/policy.json");
    let original = fs::read(&policy).unwrap();
    let mut full: Value = serde_json::from_slice(&original).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    fixture.route();
    fixture.route();
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    let events = inbox.pending().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events
            .last()
            .unwrap()
            .review
            .as_ref()
            .unwrap()
            .producer_action,
        "queued_capacity"
    );
    for event in events {
        inbox.acknowledge(&event.event_id).unwrap();
    }
    fs::write(&policy, original).unwrap();
    fixture.route();
    fixture.route();
    let pending = inbox.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]
            .review
            .as_ref()
            .unwrap()
            .producer_action
            .starts_with("attempt_finished")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn unreceipted_or_truncated_capture_never_delivers_parent_details() {
    for scenario in [
        "no_receipt",
        "missing_terminal",
        "truncated_terminal",
        "stale_candidate",
    ] {
        let fixture = Fixture::new(false, true, "rejected");
        let source = fixture
            .root
            .path()
            .join("review/reviewer/event-reports/assignment-0001.json");
        let journal: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
        let bytes: Vec<u8> = serde_json::from_value(journal["review"]["bytes"].clone()).unwrap();
        directory(&fixture.route.join("capture"));
        fs::write(fixture.route.join("capture/verdict.json"), &bytes).unwrap();
        write(&fixture.route.join("capture/terminal.json"), &journal);
        if scenario != "no_receipt" {
            let mut state = fixture.state();
            state["capture"] = json!(digest(&bytes));
            write(&fixture.route.join("state.json"), &state);
            write(&fixture.route.join("history/0001.json"), &state);
        }
        match scenario {
            "no_receipt" => fs::write(&source, "malformed").unwrap(),
            "missing_terminal" => {
                fs::remove_file(fixture.route.join("capture/terminal.json")).unwrap();
                fs::remove_file(&source).unwrap();
            }
            "truncated_terminal" => {
                fs::write(fixture.route.join("capture/terminal.json"), "{").unwrap()
            }
            "stale_candidate" => {
                let mut wrong = journal;
                let mut verdict: Value = serde_json::from_slice(&bytes).unwrap();
                verdict["candidate"] = json!("4".repeat(40));
                let raw = serde_json::to_vec(&verdict).unwrap();
                wrong["review"] = json!({"bytes":raw,"sha256":digest(&raw)});
                write(&fixture.route.join("capture/terminal.json"), &wrong);
            }
            _ => unreachable!(),
        }
        fixture.route();
        assert!(
            Inbox::open(&fixture.inbox)
                .unwrap()
                .pending()
                .unwrap()
                .is_empty(),
            "{scenario}"
        );
    }
}

#[test]
fn disable_during_final_validation_prevents_spawn() {
    let fixture = Fixture::new(true, false, "rejected");
    let marker = fixture.root.path().join("validating");
    fs::write(fixture.root.path().join("bin/jj"),format!("#!/bin/sh\nset -eu\ncase \"$*\" in *'conflicts() & @'*) exit 0;; esac\nif test -f '{}'; then : >'{}'; sleep 1; fi\nprintf '%s' '{}'\n",fixture.run.join("review-reservation.json").display(),marker.display(),"1".repeat(40))).unwrap();
    let mut router = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .args(["route-reviews", fixture.route.to_str().unwrap(), "--once"])
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    fixture.ok(&["review-disable", fixture.route.to_str().unwrap()]);
    assert!(router.wait().unwrap().success());
    assert!(fixture.route.join("DISABLED").exists());
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn completed_action_reaches_parent_after_acknowledged_history_fills() {
    let fixture = Fixture::new(true, true, "rejected");
    let policy = fixture.root.path().join("pool/policy.json");
    let original = fs::read(&policy).unwrap();
    let mut full: Value = serde_json::from_slice(&original).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    fixture.route();
    for event in inbox.pending().unwrap() {
        inbox.acknowledge(&event.event_id).unwrap();
    }
    for _ in 0..31 {
        fs::write(fixture.cwd.join("manual"), "temporary operator edit").unwrap();
        fixture.route();
        assert_eq!(fixture.state()["producer"], "needs_input");
        for event in inbox.pending().unwrap() {
            inbox.acknowledge(&event.event_id).unwrap();
        }
        fs::remove_file(fixture.cwd.join("manual")).unwrap();
        fixture.route();
        assert_eq!(fixture.state()["producer"], "queued_capacity");
        for event in inbox.pending().unwrap() {
            inbox.acknowledge(&event.event_id).unwrap();
        }
    }
    assert!(inbox.pending().unwrap().is_empty());
    fs::write(&policy, original).unwrap();
    fixture.route();
    fixture.route();
    assert_eq!(fixture.state()["producer"], "attempt_finished");
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let pending = inbox.pending().unwrap();
    assert!(
        pending.iter().any(|event| event
            .review
            .as_ref()
            .is_some_and(|notice| notice.producer_action.starts_with("attempt_finished"))),
        "{}",
        fixture.state()
    );
}

#[test]
fn full_unacknowledged_history_retains_completion_until_space_is_acknowledged() {
    let fixture = Fixture::new(true, true, "rejected");
    let report = fixture.cwd.join("report.json");
    let original_report = fs::read(&report).unwrap();
    let policy = fixture.root.path().join("pool/policy.json");
    let original_policy = fs::read(&policy).unwrap();
    let mut full: Value = serde_json::from_slice(&original_policy).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    fixture.route();
    for index in 0..62 {
        if index % 2 == 0 {
            fs::write(&report, "{\"manual\":true}").unwrap();
        } else {
            fs::write(&report, &original_report).unwrap();
        }
        fixture.route();
    }
    let history_path = fixture.route.join("parent-events.json");
    let full_history = fs::read(&history_path).unwrap();
    assert_eq!(inbox.pending().unwrap().len(), 64);
    fs::write(&report, &original_report).unwrap();
    fs::write(&policy, &original_policy).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["producer"], "attempt_finished");
    assert_eq!(fixture.state()["parent"], "pending");
    assert_eq!(fs::read(&history_path).unwrap(), full_history);
    let pending = inbox.pending().unwrap();
    let oldest = pending.first().unwrap().clone();
    for event in pending.iter().skip(1).rev() {
        inbox.acknowledge(&event.event_id).unwrap();
    }
    let detached = fixture.root.path().join("detached-parent");
    fs::rename(&fixture.inbox, &detached).unwrap();
    fixture.route();
    assert_eq!(fs::read(&history_path).unwrap(), full_history);
    fs::rename(&detached, &fixture.inbox).unwrap();
    fixture.route();
    fixture.route();
    let retained: Vec<mush_supervisor::inbox::Event> =
        serde_json::from_slice(&fs::read(&history_path).unwrap()).unwrap();
    assert_eq!(retained.len(), 2);
    assert_eq!(retained[0], oldest);
    assert_eq!(retained[1].assignment_id, 65);
    assert!(
        retained[1]
            .review
            .as_ref()
            .unwrap()
            .producer_action
            .starts_with("attempt_finished")
    );
    assert_eq!(inbox.pending().unwrap(), retained);
    let queue_path = fixture.inbox.join("queue.json");
    let mut queue: Value = serde_json::from_slice(&fs::read(&queue_path).unwrap()).unwrap();
    queue["records"]
        .as_array_mut()
        .unwrap()
        .retain(|record| record["event"]["assignment_id"] != 65);
    write(&queue_path, &queue);
    fixture.route();
    assert_eq!(inbox.pending().unwrap(), retained);
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    inbox.acknowledge(&retained[1].event_id).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["parent"], "queued");
    inbox.acknowledge(&oldest.event_id).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["parent"], "acknowledged");
    assert!(inbox.pending().unwrap().is_empty());
    assert!(inbox.acknowledged(&pending[63].event_id).unwrap());
}

#[test]
fn acknowledged_compaction_keeps_sequence_across_multiple_restarts() {
    let fixture = Fixture::new(true, true, "rejected");
    let report = fixture.cwd.join("report.json");
    let original_report = fs::read(&report).unwrap();
    let policy = fixture.root.path().join("pool/policy.json");
    let original_policy = fs::read(&policy).unwrap();
    let mut full: Value = serde_json::from_slice(&original_policy).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    fixture.route();
    let mut highest = 0;
    for index in 0..128 {
        for event in inbox.pending().unwrap() {
            assert!(event.assignment_id > highest);
            highest = event.assignment_id;
            inbox.acknowledge(&event.event_id).unwrap();
        }
        if index % 2 == 0 {
            fs::write(&report, "{\"manual\":true}").unwrap();
        } else {
            fs::write(&report, &original_report).unwrap();
        }
        fixture.route();
        let history: Vec<mush_supervisor::inbox::Event> =
            serde_json::from_slice(&fs::read(fixture.route.join("parent-events.json")).unwrap())
                .unwrap();
        assert!(history.len() <= 64);
    }
    fs::write(&report, &original_report).unwrap();
    fs::write(&policy, &original_policy).unwrap();
    fixture.route();
    fixture.route();
    let pending = inbox.pending().unwrap();
    assert_eq!(pending.last().unwrap().assignment_id, 131);
    assert!(
        pending
            .last()
            .unwrap()
            .review
            .as_ref()
            .unwrap()
            .producer_action
            .starts_with("attempt_finished")
    );
    assert!(
        inbox
            .acknowledged("review-parser-task-1.reviewer.64")
            .unwrap()
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn another_tasks_verdict_cannot_launch_or_reach_parent() {
    let fixture = Fixture::new(true, true, "rejected");
    let source = fixture
        .root
        .path()
        .join("review/reviewer/event-reports/assignment-0001.json");
    let mut journal: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    let raw: Vec<u8> = serde_json::from_value(journal["review"]["bytes"].clone()).unwrap();
    let mut verdict: Value = serde_json::from_slice(&raw).unwrap();
    verdict["task"] = json!("unrelated-task");
    let raw = serde_json::to_vec(&verdict).unwrap();
    journal["review"] = json!({"sha256":digest(&raw),"bytes":raw});
    write(&source, &journal);
    fixture.route();
    fixture.route();
    assert_eq!(fixture.state()["producer"], "needs_input_review");
    assert!(!fixture.root.path().join("calls").exists());
    assert!(
        Inbox::open(&fixture.inbox)
            .unwrap()
            .pending()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn full_outbox_replays_after_parent_returns_with_completed_correction() {
    let fixture = Fixture::new(true, true, "rejected");
    let detached = fixture.root.path().join("detached-parent");
    fs::rename(&fixture.inbox, &detached).unwrap();
    let report = fixture.cwd.join("report.json");
    let original_report = fs::read(&report).unwrap();
    let policy = fixture.root.path().join("pool/policy.json");
    let original_policy = fs::read(&policy).unwrap();
    let mut full: Value = serde_json::from_slice(&original_policy).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    fixture.route();
    for index in 0..62 {
        if index % 2 == 0 {
            fs::write(&report, "{\"manual\":true}").unwrap();
        } else {
            fs::write(&report, &original_report).unwrap();
        }
        fixture.route();
    }
    let history_path = fixture.route.join("parent-events.json");
    let history_bytes = fs::read(&history_path).unwrap();
    let history: Vec<mush_supervisor::inbox::Event> =
        serde_json::from_slice(&history_bytes).unwrap();
    assert_eq!(history.len(), 64);
    fs::write(&policy, original_policy).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["producer"], "attempt_finished");
    assert_eq!(fs::read(&history_path).unwrap(), history_bytes);
    fs::rename(detached, &fixture.inbox).unwrap();
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    fixture.route();
    fixture.route();
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let pending = inbox.pending().unwrap();
    assert_eq!(pending.len(), history.len(), "{}", fixture.state());
    assert_eq!(pending, history);
    for event in history.iter().rev() {
        inbox.acknowledge(&event.event_id).unwrap();
    }
    fixture.route();
    let pending = inbox.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].assignment_id, 65);
    assert!(
        pending[0]
            .review
            .as_ref()
            .unwrap()
            .producer_action
            .starts_with("attempt_finished")
    );
}

#[test]
fn invalid_or_exhausted_parent_sequence_preserves_durable_outbox() {
    for scenario in ["exhausted", "missing", "oversized"] {
        let fixture = Fixture::new(false, true, "accepted");
        fixture.route();
        let history_path = fixture.route.join("parent-events.json");
        let mut history: Value = serde_json::from_slice(&fs::read(&history_path).unwrap()).unwrap();
        history[0]["review"]["producer_action"] = json!("previous");
        match scenario {
            "exhausted" => {
                history[0]["assignment_id"] = json!(u32::MAX);
                history[0]["round"] = json!(u32::MAX);
                history[0]["event_id"] =
                    json!(format!("review-parser-task-1.reviewer.{}", u32::MAX));
            }
            "missing" => {
                history[0].as_object_mut().unwrap().remove("assignment_id");
            }
            "oversized" => history = json!(vec![history[0].clone(); 65]),
            _ => unreachable!(),
        }
        write(&history_path, &history);
        let bytes = fs::read(&history_path).unwrap();
        let inbox = Inbox::open(&fixture.inbox).unwrap();
        let pending = inbox.pending().unwrap();
        fixture.route();
        fixture.route();
        assert_eq!(fixture.state()["parent"], "pending", "{scenario}");
        assert_eq!(fs::read(&history_path).unwrap(), bytes, "{scenario}");
        assert_eq!(inbox.pending().unwrap(), pending, "{scenario}");
        assert!(!fixture.root.path().join("calls").exists());
    }
}

#[test]
fn router_killed_after_outbox_write_replays_the_same_parent_event() {
    let fixture = Fixture::new(false, true, "accepted");
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(fixture.inbox.join("queue.lock"))
        .unwrap();
    lock.lock().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mush-supervisor"))
        .env("HOME", fixture.root.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                fixture.root.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["route-reviews", fixture.route.to_str().unwrap(), "--once"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let history_path = fixture.route.join("parent-events.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while !history_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let history = fs::read(&history_path);
    let pending = inbox.pending().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    drop(lock);
    let history = history.unwrap();
    assert!(pending.is_empty());
    fixture.route();
    fixture.route();
    assert_eq!(fs::read(&history_path).unwrap(), history);
    let pending = inbox.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].assignment_id, 1);
    assert_eq!(pending[0].event_id, "review-parser-task-1.reviewer.1");
    inbox.acknowledge(&pending[0].event_id).unwrap();
    fixture.route();
    assert!(inbox.pending().unwrap().is_empty());
    assert_eq!(inbox.events().unwrap(), pending);
    assert!(!fixture.root.path().join("calls").exists());
}

#[test]
fn retained_outbox_replays_when_latest_action_cannot_be_persisted() {
    let fixture = Fixture::new(true, true, "rejected");
    let detached = fixture.root.path().join("detached-parent");
    fs::rename(&fixture.inbox, &detached).unwrap();
    let policy = fixture.root.path().join("pool/policy.json");
    let original_policy = fs::read(&policy).unwrap();
    let mut full: Value = serde_json::from_slice(&original_policy).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    fixture.route();
    let history_path = fixture.route.join("parent-events.json");
    let history = fs::read(&history_path).unwrap();
    let retained: Vec<mush_supervisor::inbox::Event> = serde_json::from_slice(&history).unwrap();
    assert_eq!(retained.len(), 2);
    let obstruction = history_path.with_extension("pending-write");
    directory(&obstruction);
    fs::rename(&detached, &fixture.inbox).unwrap();
    fs::write(&policy, original_policy).unwrap();
    fixture.route();
    fixture.route();
    assert_eq!(fixture.state()["producer"], "attempt_finished");
    assert_eq!(fixture.state()["parent"], "pending");
    assert_eq!(fs::read(&history_path).unwrap(), history);
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    assert_eq!(inbox.pending().unwrap(), retained);
    for event in &retained {
        inbox.acknowledge(&event.event_id).unwrap();
    }
    fs::remove_dir(&obstruction).unwrap();
    fixture.route();
    fixture.route();
    let pending = inbox.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].assignment_id, 3);
    assert!(
        pending[0]
            .review
            .as_ref()
            .unwrap()
            .producer_action
            .starts_with("attempt_finished")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn failed_compaction_preserves_pending_events_and_rejects_rebound_parent() {
    let fixture = Fixture::new(true, true, "rejected");
    let report = fixture.cwd.join("report.json");
    let original_report = fs::read(&report).unwrap();
    let policy = fixture.root.path().join("pool/policy.json");
    let original_policy = fs::read(&policy).unwrap();
    let mut full: Value = serde_json::from_slice(&original_policy).unwrap();
    full["capacity"] = json!(2);
    write(&policy, &full);
    let inbox = Inbox::open(&fixture.inbox).unwrap();
    fixture.route();
    for index in 0..62 {
        if index % 2 == 0 {
            fs::write(&report, "{\"manual\":true}").unwrap();
        } else {
            fs::write(&report, &original_report).unwrap();
        }
        fixture.route();
    }
    let history_path = fixture.route.join("parent-events.json");
    let history = fs::read(&history_path).unwrap();
    let events = inbox.events().unwrap();
    assert_eq!(events.len(), 64);
    for event in events.iter().skip(1).rev() {
        inbox.acknowledge(&event.event_id).unwrap();
    }
    let queue = fixture.inbox.join("queue.json");
    let queue_bytes = fs::read(&queue).unwrap();
    let mut rebound: Value = serde_json::from_slice(&queue_bytes).unwrap();
    rebound["binding"]["session_id"] = json!("different-parent");
    write(&queue, &rebound);
    let rebound_bytes = fs::read(&queue).unwrap();
    fs::write(&policy, original_policy).unwrap();
    fixture.route();
    assert_eq!(fixture.state()["producer"], "attempt_finished");
    assert_eq!(fixture.state()["parent"], "pending");
    assert_eq!(fixture.state()["detail"], "parent binding changed");
    assert_eq!(fs::read(&queue).unwrap(), rebound_bytes);
    assert_eq!(fs::read(&history_path).unwrap(), history);
    fs::write(&queue, &queue_bytes).unwrap();
    let obstruction = history_path.with_extension("pending-write");
    directory(&obstruction);
    for _ in 0..3 {
        fixture.route();
        assert_eq!(fixture.state()["parent"], "pending");
        assert_eq!(fs::read(&history_path).unwrap(), history);
        assert_eq!(fs::read(&queue).unwrap(), queue_bytes);
        assert_eq!(inbox.pending().unwrap(), vec![events[0].clone()]);
    }
    fs::remove_dir(&obstruction).unwrap();
    fixture.route();
    fixture.route();
    let retained: Vec<mush_supervisor::inbox::Event> =
        serde_json::from_slice(&fs::read(&history_path).unwrap()).unwrap();
    assert_eq!(retained.len(), 2);
    assert_eq!(retained[0], events[0]);
    assert_eq!(retained[1].assignment_id, 65);
    assert!(
        retained[1]
            .review
            .as_ref()
            .unwrap()
            .producer_action
            .starts_with("attempt_finished")
    );
    assert_eq!(inbox.pending().unwrap(), retained);
    assert_eq!(inbox.events().unwrap().len(), 65);
    inbox.acknowledge(&retained[1].event_id).unwrap();
    fixture.route();
    assert_eq!(inbox.pending().unwrap(), vec![events[0].clone()]);
    inbox.acknowledge(&events[0].event_id).unwrap();
    fixture.route();
    fixture.route();
    assert_eq!(fixture.state()["parent"], "acknowledged");
    assert!(inbox.pending().unwrap().is_empty());
    assert_eq!(inbox.events().unwrap().len(), 65);
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
