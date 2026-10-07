#![expect(clippy::unwrap_used)]
#![allow(clippy::panic)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

fn serve_one_reply(listener: TcpListener) -> std::thread::JoinHandle<serde_json::Value> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(25);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no request arrived");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture accept: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let start = loop {
            if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                break index + 4;
            }
            let mut buffer = [0u8; 8192];
            let n = socket.read(&mut buffer).unwrap();
            assert!(n > 0);
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
        while bytes.len() < start + length {
            let mut buffer = [0u8; 8192];
            let n = socket.read(&mut buffer).unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
        }
        let body = concat!(
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"done\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}\n\n",
        );
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        serde_json::from_slice(&bytes[start..start + length]).unwrap()
    })
}

#[test]
fn json_mode_streams_events_as_json_lines_and_nothing_else_on_stdout() {
    let dir = crate::tempdir().unwrap();
    let config = dir.path().join("config/mush");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        "rlm = false\n[compaction]\nenabled = false\n[api_keys]\nfixture = 'fixture-key'\n",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let model = mush_ai::types::Model::builder()
        .id("gpt-fixture".into())
        .name("fixture".into())
        .provider(mush_ai::types::Provider::Custom("fixture".into()))
        .api(mush_ai::types::Api::OpenaiResponses)
        .base_url(format!("http://{}", listener.local_addr().unwrap()).into())
        .context_window(mush_ai::types::TokenCount::new(1_000_000))
        .max_output_tokens(mush_ai::types::TokenCount::new(64))
        .build();
    std::fs::write(
        config.join("models.json"),
        serde_json::to_vec(&[model]).unwrap(),
    )
    .unwrap();
    let server = serve_one_reply(listener);

    let output = Command::new(env!("CARGO_BIN_EXE_mush"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", dir.path())
        .env("MUSH_DATA_DIR", dir.path().join("data"))
        .env("MUSH_CONFIG_DIR", &config)
        .current_dir(dir.path())
        .args([
            "--no-tools",
            "--no-session",
            "--no-skills",
            "--no-context-files",
            "--model",
            "gpt-fixture",
            "--mode",
            "json",
            "-p",
            "hello",
        ])
        .output()
        .unwrap();
    let _ = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    assert_eq!(lines[0]["type"], "session");
    assert!(lines[0]["id"].is_string(), "{}", lines[0]);
    assert!(lines[0]["timestamp"].is_string(), "{}", lines[0]);
    assert!(lines[0]["cwd"].is_string(), "{}", lines[0]);
    assert!(lines[0].get("model").is_none(), "{}", lines[0]);
    let mut types: Vec<&str> = lines[1..]
        .iter()
        .filter_map(|line| line["type"].as_str())
        .collect();
    types.dedup();
    assert_eq!(
        types,
        [
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ],
        "{stdout}"
    );
    assert_eq!(lines[3]["message"]["role"], "user");
    assert_eq!(lines[3]["message"]["content"], "hello");
    let update = lines
        .iter()
        .find(|line| line["assistantMessageEvent"]["type"] == "text_delta")
        .unwrap_or_else(|| panic!("no text delta in {stdout}"));
    assert_eq!(update["assistantMessageEvent"]["delta"], "done");
    assert_eq!(update["message"]["role"], "assistant");
    let end = &lines[lines.len() - 1];
    let messages = end["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "{end}");
    let answer = &messages[1];
    assert_eq!(answer["role"], "assistant");
    assert_eq!(answer["stopReason"], "stop");
    assert_eq!(
        answer["content"][0],
        serde_json::json!({"type": "text", "text": "done"})
    );
    assert_eq!(answer["usage"]["output"], 1);
    assert_eq!(answer["api"], "openai-responses");
    assert_eq!(answer["provider"], "fixture");
}

#[test]
fn json_mode_needs_a_prompt() {
    let output = Command::new(env!("CARGO_BIN_EXE_mush"))
        .env_clear()
        .args(["--mode", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--prompt"), "{stderr}");
}

#[test]
fn a_resumed_print_run_keeps_the_sessions_thinking_level() {
    let dir = crate::tempdir().unwrap();
    let config = dir.path().join("config/mush");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        "rlm = false\nthinking = \"high\"\n[compaction]\nenabled = false\n[api_keys]\nfixture = 'fixture-key'\n",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let model = mush_ai::types::Model::builder()
        .id("gpt-fixture".into())
        .name("fixture".into())
        .provider(mush_ai::types::Provider::Custom("fixture".into()))
        .api(mush_ai::types::Api::OpenaiResponses)
        .base_url(format!("http://{}", listener.local_addr().unwrap()).into())
        .context_window(mush_ai::types::TokenCount::new(1_000_000))
        .max_output_tokens(mush_ai::types::TokenCount::new(64))
        .reasoning(true)
        .build();
    std::fs::write(
        config.join("models.json"),
        serde_json::to_vec(&[model]).unwrap(),
    )
    .unwrap();
    let store = mush_session::SessionStore::new(dir.path().join("data/sessions"));
    let mut session = mush_session::Session::new("gpt-fixture", &dir.path().display().to_string());
    session
        .conversation
        .record_thinking_level(&mush_ai::types::ThinkingLevel::Low);
    session.push_message(mush_ai::types::Message::User(
        mush_ai::types::UserMessage::builder()
            .content(mush_ai::types::UserContent::Text("earlier".into()))
            .timestamp_ms(mush_ai::types::Timestamp::zero())
            .build(),
    ));
    store.save(&session).unwrap();
    let server = serve_one_reply(listener);

    let output = Command::new(env!("CARGO_BIN_EXE_mush"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", dir.path())
        .env("MUSH_DATA_DIR", dir.path().join("data"))
        .env("MUSH_CONFIG_DIR", &config)
        .current_dir(dir.path())
        .args([
            "--no-tools",
            "--no-skills",
            "--no-context-files",
            "--resume",
            session.meta.id.as_str(),
            "-p",
            "again",
        ])
        .output()
        .unwrap();
    let request = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(request["reasoning"]["effort"], "low", "{request}");
    let resumed = store.load(&session.meta.id).unwrap();
    assert_eq!(
        resumed.conversation.thinking_level(),
        Some(mush_ai::types::ThinkingLevel::Low)
    );
}
