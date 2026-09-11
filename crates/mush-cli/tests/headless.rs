use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mush"));
    command
        .current_dir(dir)
        .env("MUSH_DATA_DIR", dir.join("data"))
        .env("MUSH_CONFIG_DIR", dir.join("config/mush"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("HOME", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait(child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            panic!("headless process waited for stdin or tty");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn signals_checkpoint_and_cancel_during_startup() {
    for signal in ["-TERM", "-INT"] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config/mush");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.toml"),
            r#"
[mcp.wait]
type = "local"
command = ["sh", "-c", "touch ready; exec sleep 8"]
"#,
        )
        .unwrap();
        let path = dir.path().join("status.json");
        let mut child = command(dir.path())
            .args(["--model", "claude-opus-4-7", "--status-file"])
            .arg(&path)
            .args(["-p", "hello"])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.path().join("ready").exists() {
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("startup did not reach local fake mcp server");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Command::new("kill")
                .args([signal, &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        assert!(!wait(&mut child).unwrap().success());
        let status: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(status["state"], "cancelled");
        let store = mush_session::SessionStore::new(dir.path().join("data/sessions"));
        let id = status["session_id"].as_str().unwrap();
        assert_eq!(store.load(&id.into()).unwrap().context().len(), 1);
    }
}

#[test]
fn explicit_prompt_checkpoints_before_setup_error_with_stdin_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.json");
    let mut child = command(dir.path())
        .args([
            "--model",
            "nonexistent-headless-test-model",
            "--status-file",
        ])
        .arg(&path)
        .args(["-p", "hello"])
        .spawn()
        .unwrap();
    assert!(!wait(&mut child).unwrap().success());
    let status: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(status["state"], "error");
    assert_eq!(status["error_classification"], "unknown_model");
    assert_eq!(status["model"], "nonexistent-headless-test-model");
    let id = status["session_id"].as_str().unwrap();
    let store = mush_session::SessionStore::new(dir.path().join("data/sessions"));
    let session = store.load(&id.into()).unwrap();
    assert_eq!(session.context().len(), 1);
    assert!(
        matches!(&session.context()[0], mush_ai::types::Message::User(user) if user.text() == "hello")
    );

    let mut child = command(dir.path())
        .args(["--status-file"])
        .arg(&path)
        .args(["-c", id, "-p", "again"])
        .spawn()
        .unwrap();
    assert!(!wait(&mut child).unwrap().success());
    let resumed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(resumed["session_id"], id);
    assert_eq!(resumed["error_classification"], "unknown_model");
    assert_eq!(store.load(&id.into()).unwrap().context().len(), 2);
}

#[test]
fn status_file_resume_does_not_read_stdin_or_append_empty_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let store = mush_session::SessionStore::new(dir.path().join("data/sessions"));
    let session = mush_session::Session::new("nonexistent-headless-test-model", "/tmp");
    store.save(&session).unwrap();
    let path = dir.path().join("status.json");
    let mut child = command(dir.path())
        .arg("--status-file")
        .arg(&path)
        .args(["-c", session.meta.id.as_str()])
        .spawn()
        .unwrap();
    assert!(!wait(&mut child).unwrap().success());
    assert!(store.load(&session.meta.id).unwrap().context().is_empty());
    let status: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(status["error_classification"], "unknown_model");
}

#[test]
fn subcommands_do_not_read_stdin() {
    let dir = tempfile::tempdir().unwrap();
    for subcommand in ["models", "sessions"] {
        let mut child = command(dir.path()).arg(subcommand).spawn().unwrap();
        assert!(wait(&mut child).unwrap().success());
    }
}

#[test]
fn no_session_still_reports_setup_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.json");
    let mut child = command(dir.path())
        .args([
            "--no-session",
            "--model",
            "nonexistent-headless-test-model",
            "--status-file",
        ])
        .arg(&path)
        .args(["-p", "hello"])
        .spawn()
        .unwrap();
    assert!(!wait(&mut child).unwrap().success());
    let status: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(status["state"], "error");
    assert!(!dir.path().join("data/sessions").exists());
}
