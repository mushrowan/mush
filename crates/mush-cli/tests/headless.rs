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

#[test]
fn resume_appends_to_saved_context_without_rewriting_the_prefix() {
    use mush_ai::types::*;
    let dir = tempfile::tempdir().unwrap();
    let store = mush_session::SessionStore::new(dir.path().join("data/sessions"));
    let mut session = mush_session::Session::new(
        "nonexistent-headless-test-model",
        &dir.path().display().to_string(),
    );
    session.push_message(Message::User(
        UserMessage::builder()
            .content(UserContent::Text("original task".into()))
            .timestamp_ms(Timestamp::zero())
            .prompt_id(PromptId::new())
            .build(),
    ));
    session.push_message(Message::Assistant(
        AssistantMessage::builder()
            .model("nonexistent-headless-test-model".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .content(vec![
                AssistantContentPart::Thinking(ThinkingContent::Thinking {
                    thinking: "fixture reasoning".into(),
                    signature: Some("fixture signature".into()),
                }),
                AssistantContentPart::ToolCall(ToolCall {
                    id: "saved-call".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"fixture"}),
                }),
            ])
            .build(),
    ));
    session.push_message(Message::ToolResult(ToolResultMessage {
        tool_call_id: "saved-call".into(),
        tool_name: "read".into(),
        timestamp_ms: Timestamp::zero(),
        outcome: ToolOutcome::Success,
        content: vec![ToolResultContentPart::Text(TextContent {
            text: "full output ".repeat(1000),
        })],
    }));
    store.save(&session).unwrap();
    let before = serde_json::to_value(session.context()).unwrap();
    let mut child = command(dir.path())
        .args(["--status-file"])
        .arg(dir.path().join("status.json"))
        .args(["-c", session.meta.id.as_str(), "-p", "new follow-up"])
        .spawn()
        .unwrap();
    assert!(!wait(&mut child).unwrap().success());
    let resumed = store.load(&session.meta.id).unwrap();
    let context = serde_json::to_value(resumed.context()).unwrap();
    let before = before.as_array().unwrap();
    assert_eq!(&context.as_array().unwrap()[..before.len()], before);
    assert_eq!(context.as_array().unwrap().len(), before.len() + 1);
    assert_eq!(resumed.meta.id, session.meta.id);
}

#[test]
fn restarted_headless_requests_keep_cache_key_tools_and_history_prefix() {
    restarted_requests_keep_prefix(false);
}

#[test]
fn orchestrator_event_preserves_cross_process_provider_prefix() {
    restarted_requests_keep_prefix(true);
}

#[expect(clippy::unwrap_used)]
fn restarted_requests_keep_prefix(with_event: bool) {
    use mush_ai::types::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config/mush");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        "auto_compact = false\n[api_keys]\nfixture = 'fixture-key'\n",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let model = Model::builder()
        .id("gpt-fixture".into())
        .name("fixture".into())
        .provider(Provider::Custom("fixture".into()))
        .api(Api::OpenaiResponses)
        .base_url(format!("http://{}", listener.local_addr().unwrap()).into())
        .context_window(TokenCount::new(1_000_000))
        .max_output_tokens(TokenCount::new(64))
        .build();
    std::fs::write(
        config.join("models.json"),
        serde_json::to_vec(&[model]).unwrap(),
    )
    .unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut requests = Vec::new();
        while requests.len() < 2 {
            let (mut socket, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("fixture accept: {error}"),
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let start = loop {
                if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    break index + 4;
                }
                let mut buffer = [0u8; 8192];
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0 && bytes.len() < 1024 * 1024);
                bytes.extend_from_slice(&buffer[..n]);
            };
            let headers = String::from_utf8_lossy(&bytes[..start]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(length < 1024 * 1024);
            while bytes.len() < start + length {
                let mut buffer = [0u8; 8192];
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            requests.push(
                serde_json::from_slice::<serde_json::Value>(&bytes[start..start + length]).unwrap(),
            );
            let body = concat!(
                "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n\n",
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"done\"}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}\n\n",
            );
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    let store = mush_session::SessionStore::new(dir.path().join("data/sessions"));
    let mut session = mush_session::Session::new("gpt-fixture", &dir.path().display().to_string());
    session.push_message(Message::User(
        UserMessage::builder()
            .content(UserContent::Text("first task".into()))
            .timestamp_ms(Timestamp::zero())
            .build(),
    ));
    session.push_message(Message::Assistant(
        AssistantMessage::builder()
            .model("gpt-fixture".into())
            .provider(Provider::Custom("fixture".into()))
            .api(Api::OpenaiResponses)
            .timestamp_ms(Timestamp::zero())
            .content(vec![AssistantContentPart::ToolCall(ToolCall {
                id: "saved".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"fixture"}),
            })])
            .build(),
    ));
    session.push_message(Message::ToolResult(ToolResultMessage {
        tool_call_id: "saved".into(),
        tool_name: "read".into(),
        timestamp_ms: Timestamp::zero(),
        outcome: ToolOutcome::Success,
        content: vec![ToolResultContentPart::Text(TextContent {
            text: "preserved tool output ".repeat(400),
        })],
    }));
    store.save(&session).unwrap();
    for (index, text) in ["next task", "follow-up task"].into_iter().enumerate() {
        let mut child = command(dir.path())
            .args([
                "--max-turns",
                "1",
                "-c",
                session.meta.id.as_str(),
                "-p",
                text,
            ])
            .spawn()
            .unwrap();
        assert!(wait(&mut child).unwrap().success());
        if with_event && index == 0 {
            use mush_session::inbox::{Binding, Consumer, Event, Inbox, TerminalReason, append};
            let path = dir.path().join("orchestrator-inbox");
            let inbox = Inbox::register(
                &path,
                Binding {
                    session_id: session.meta.id.to_string(),
                    cwd: dir.path().to_owned(),
                    workers: vec![dir.path().to_owned()],
                },
            )
            .unwrap();
            let event = Event {
                event_id: "run.worker.1".into(),
                run_id: "run".into(),
                worker_id: "worker".into(),
                assignment_id: 1,
                round: 2,
                session_id: Some("worker-session".into()),
                reason: TerminalReason::Completed,
                report_path: dir.path().join("private-report.json"),
            };
            inbox.enqueue(event.clone(), dir.path()).unwrap();
            let consumer = Consumer::open(&path, session.meta.id.clone(), dir.path()).unwrap();
            let mut saved = store.load(&session.meta.id).unwrap();
            assert!(append(&mut saved.conversation, &event).unwrap());
            store.save(&saved).unwrap();
            assert!(consumer.acknowledge_saved(&event.event_id, &store).unwrap());
            assert!(inbox.pending().unwrap().is_empty());
        }
    }
    let requests = server.join().unwrap();
    let first = &requests[0];
    let next = &requests[1];
    for key in [
        "model",
        "tools",
        "instructions",
        "prompt_cache_key",
        "reasoning",
    ] {
        assert_eq!(first[key], next[key], "{key}");
    }
    assert_eq!(first["prompt_cache_key"], session.meta.id.as_str());
    assert!(!first["tools"].as_array().unwrap().is_empty());
    let prefix = first["input"].as_array().unwrap();
    assert_eq!(&next["input"].as_array().unwrap()[..prefix.len()], prefix);
    assert_eq!(
        next["input"].as_array().unwrap().len(),
        prefix.len() + 2 + usize::from(with_event)
    );
    if with_event {
        let input = serde_json::to_string(&next["input"]).unwrap();
        assert_eq!(input.matches("run.worker.1").count(), 1);
        assert!(input.contains("untrusted worker event"));
    }
}
