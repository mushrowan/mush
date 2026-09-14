use mush_agent::ipc::{
    IpcListener,
    control::{Control, Event, Outcome},
};
use std::sync::Arc;

async fn cli(
    dir: &std::path::Path,
    socket: &std::path::Path,
    target: &mush_agent::ipc::control::Target,
    args: &[&str],
) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_mush"))
        .env_clear()
        .env("HOME", dir)
        .env("MUSH_DATA_DIR", dir.join("mush-data"))
        .env("MUSH_CONTROL_SOCKET", socket)
        .env("MUSH_CONTROL_INSTANCE", &target.instance)
        .env("MUSH_CONTROL_SESSION", &target.session)
        .env("MUSH_CONTROL_ACTIVATION", &target.activation)
        .arg("control")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap_or_else(|error| panic!("{error}"))
}

fn value(output: std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}"))
}

#[tokio::test]
async fn cli_send_schedule_cancel_status_and_explicit_errors() {
    let dir = tempfile::tempdir().unwrap();
    let control = Control::new("session".into());
    let listener = IpcListener::start_control(
        dir.path().join("control.sock"),
        Arc::new(mush_agent::AgentCard::build(
            "fixture",
            &mush_agent::tool::ToolRegistry::new(),
        )),
        control.clone(),
    )
    .unwrap();
    let target = control.target();
    let run = |args| cli(dir.path(), listener.path(), &target, args);
    let sent = value(
        run(&[
            "--request-id",
            "sent",
            "send",
            "worker complete",
            "--worker",
            "fixture",
        ])
        .await,
    );
    assert_eq!(sent["kind"]["payload"]["outcome"], "queued");
    let delivery = control.take_next(&target).unwrap();
    assert!(matches!(delivery.event, Event::WorkerCompletion { .. }));
    control.finish(Outcome::Completed);
    let timer = value(
        run(&[
            "--request-id",
            "timer",
            "schedule",
            "check worker",
            "--delay-ms",
            "60000",
        ])
        .await,
    );
    assert_eq!(timer["kind"]["payload"]["id"], "timer");
    assert_eq!(timer["kind"]["payload"]["outcome"], "scheduled");
    let status = value(run(&["status", "timer"]).await);
    assert_eq!(
        status["kind"]["payload"]["records"][0]["outcome"],
        "scheduled"
    );
    let cancel = value(run(&["cancel", "timer"]).await);
    assert_eq!(cancel["kind"]["payload"]["outcome"], "cancelled");
    let duplicate = value(
        run(&[
            "--request-id",
            "sent",
            "send",
            "worker complete",
            "--worker",
            "fixture",
        ])
        .await,
    );
    assert_eq!(duplicate["kind"]["payload"]["duplicate"], true);
    let error = run(&["cancel", "absent"]).await;
    assert!(!error.status.success());
    let error: serde_json::Value = serde_json::from_slice(&error.stdout).unwrap();
    assert_eq!(error["kind"]["payload"]["code"], "unknown_request");
    control.activate("other".into());
    let error = run(&["send", "old target"]).await;
    assert!(!error.status.success());
    let error: serde_json::Value = serde_json::from_slice(&error.stdout).unwrap();
    assert_eq!(error["kind"]["payload"]["code"], "unavailable");
    listener.shutdown().await.unwrap();
    assert!(!dir.path().join("mush-data").exists());
}
