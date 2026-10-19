#![cfg(target_os = "linux")]
#![allow(clippy::panic)]

use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mush_ai::types::{Api, Model, Provider, TokenCount};
use serde_json::{Value, json};

fn command(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mush"));
    command
        .env_clear()
        .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("MUSH_CONFIG_DIR", dir.join("config/mush"))
        .env("MUSH_DATA_DIR", dir.join("data"))
        .env("TERM", "xterm-256color")
        .current_dir(dir)
        .stdin(Stdio::null());
    command
}

#[expect(clippy::unwrap_used)]
fn wait(child: &mut std::process::Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("output fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn reader(mut input: impl Read + Send + 'static, pty: bool) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 8192];
            match input.read(&mut buffer) {
                Ok(0) => return bytes,
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(error) if pty && error.raw_os_error() == Some(5) => return bytes,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("capture: {error}"),
            }
            assert!(bytes.len() < 1024 * 1024);
        }
    })
}

#[expect(clippy::unwrap_used)]
fn pty() -> (File, File) {
    use rustix::pty::{OpenptFlags, ioctl_tiocgptpeer, openpt, unlockpt};
    use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = openpt(flags).unwrap();
    unlockpt(&master).unwrap();
    let slave = ioctl_tiocgptpeer(&master, flags).unwrap();
    let mut termios = tcgetattr(&slave).unwrap();
    termios.make_raw();
    tcsetattr(&slave, OptionalActions::Now, &termios).unwrap();
    (master.into(), slave.into())
}

#[expect(clippy::unwrap_used)]
fn capture(
    mut command: Command,
    stdout_tty: bool,
    stderr_tty: bool,
) -> (ExitStatus, String, String) {
    let (out_master, out_slave) = if stdout_tty {
        let (m, s) = pty();
        (Some(m), Some(s))
    } else {
        (None, None)
    };
    let (err_master, err_slave) = if stderr_tty {
        let (m, s) = pty();
        (Some(m), Some(s))
    } else {
        (None, None)
    };
    command
        .stdout(out_slave.map_or_else(Stdio::piped, Stdio::from))
        .stderr(err_slave.map_or_else(Stdio::piped, Stdio::from));
    let mut child = command.spawn().unwrap();
    drop(command);
    let stdout = match out_master {
        Some(master) => reader(master, true),
        None => reader(child.stdout.take().unwrap(), false),
    };
    let stderr = match err_master {
        Some(master) => reader(master, true),
        None => reader(child.stderr.take().unwrap(), false),
    };
    let status = wait(&mut child);
    (
        status,
        String::from_utf8(stdout.join().unwrap()).unwrap(),
        String::from_utf8(stderr.join().unwrap()).unwrap(),
    )
}

fn event(kind: &str, data: Value) -> String {
    format!("event: {kind}\ndata: {data}\n\n")
}

fn text_block(index: usize, kind: &str, delta_kind: &str, text: &str) -> String {
    event(
        "response.output_item.added",
        json!({"output_index":index,"item":{"type":kind}}),
    ) + &event(delta_kind, json!({"output_index":index,"delta":text}))
        + &event("response.output_item.done", json!({"output_index":index}))
}

fn completed() -> String {
    event(
        "response.completed",
        json!({"response":{"status":"completed","usage":{"input_tokens":50,"output_tokens":4,"input_tokens_details":{"cached_tokens":10}}}}),
    )
}

#[expect(clippy::unwrap_used)]
fn request(socket: &mut TcpStream) -> Value {
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut bytes = Vec::new();
    let start = loop {
        if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0; 8192];
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
        let mut buffer = [0; 8192];
        let n = socket.read(&mut buffer).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buffer[..n]);
    }
    serde_json::from_slice(&bytes[start..start + length]).unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
    server: JoinHandle<Vec<Value>>,
    thinking: String,
    answer: String,
    tool_text: String,
}

#[expect(clippy::unwrap_used)]
impl Fixture {
    fn new(fail: bool, literal_escapes: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config/mush");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("config.toml"), "compaction_model = 'missing-fixture-model'\n[compaction]\nenabled = false\n[api_keys]\nfixture = 'fixture-key'\n[kernel]\nmanaged = false\n[lsp]\ndiagnostics = true\n[mcp.fixture]\ntype = 'local'\ncommand = ['__missing_output_fixture__']\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".mush/rules")).unwrap();
        std::fs::write(
            dir.path().join(".mush/rules/fixture.md"),
            "---\nglobs: [\"fixture\"]\n---\nfixture rule\n",
        )
        .unwrap();
        let payload = |text: &str| {
            if literal_escapes {
                format!("\x1b[35m{text}\x1b[0m")
            } else {
                text.to_owned()
            }
        };
        let thinking = payload("fixture thinking");
        let answer = payload("fixture answer");
        let tool_text = payload("fixture tool payload");
        std::fs::write(dir.path().join("fixture"), &tool_text).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let model = Model::builder()
            .id("gpt-output-fixture".into())
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
        let mut first = text_block(
            0,
            "reasoning",
            "response.reasoning_summary_text.delta",
            &thinking,
        );
        first += &text_block(1, "message", "response.output_text.delta", "before tools\n");
        first += &event(
            "response.output_item.added",
            json!({"output_index":2,"item":{"type":"function_call","call_id":"read-ok","name":"ipython","arguments":serde_json::to_string(&json!({"code":"from pathlib import Path\nprint(Path('fixture').read_text())"})).unwrap()}}),
        );
        first += &event("response.output_item.done", json!({"output_index":2}));
        first += &completed();
        let second = event(
            "response.output_item.added",
            json!({"output_index":0,"item":{"type":"function_call","call_id":"read-missing","name":"ipython","arguments":serde_json::to_string(&json!({"code":"print(Path('missing').read_text())"})).unwrap()}}),
        ) + &event("response.output_item.done", json!({"output_index":0}))
            + &completed();
        let third = text_block(0, "message", "response.output_text.delta", &answer)
            + &if fail {
                event(
                    "response.failed",
                    json!({"response":{"error":{"type":"fixture_error","code":"fixture_code","message":"fixture detail\nsecond detail"}}}),
                )
            } else {
                completed()
            };
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for body in [first, second, third] {
                let deadline = Instant::now() + Duration::from_secs(20);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                requests.push(request(&mut socket));
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        Self {
            dir,
            server,
            thinking,
            answer,
            tool_text,
        }
    }

    fn command(&self) -> Command {
        let mut command = command(self.dir.path());
        command
            .args([
                "--model",
                "gpt-output-fixture",
                "--thinking",
                "--debug-cache",
                "--max-turns",
                "3",
                "--status-file",
            ])
            .arg(self.dir.path().join("status.json"))
            .args(["-p", "fixture"]);
        command
    }

    fn finish(self, fail: bool) {
        let requests = self.server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1]["input"].as_array().unwrap().iter().any(|item| {
            item["type"] == "function_call_output"
                && item["output"]
                    .as_str()
                    .is_some_and(|s| s.contains(&self.tool_text))
        }));
        let status: Value =
            serde_json::from_slice(&std::fs::read(self.dir.path().join("status.json")).unwrap())
                .unwrap();
        assert_eq!(status["state"], if fail { "error" } else { "completed" });
        let store = mush_session::SessionStore::new(self.dir.path().join("data/sessions"));
        let session = store
            .load(&status["session_id"].as_str().unwrap().into())
            .unwrap();
        assert!(session.context().iter().any(|message| matches!(message, mush_ai::types::Message::ToolResult(result) if result.content.iter().any(|part| matches!(part, mush_ai::types::ToolResultContentPart::Text(text) if text.text.contains(&self.tool_text))))));
    }
}

fn styled(text: &str, code: u8, colour: bool) -> String {
    if colour {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

#[expect(clippy::unwrap_used)]
fn assert_messages(fixture: &Fixture, stdout: &str, stderr: &str, err_colour: bool, fail: bool) {
    if fail {
        assert_eq!(stdout, "");
    } else {
        assert_eq!(stdout, format!("{}\n", fixture.answer));
    }
    assert!(!stderr.contains(&fixture.thinking), "{stderr:?}");
    for (text, code) in [
        ("▶ ipython", 36),
        ("✓ ipython", 32),
        ("✗ ipython failed", 31),
        ("LSP diagnostics enabled", 2),
        ("found 1 rule files in .mush/rules/", 2),
        ("cache read detected: 10 tokens", 36),
    ] {
        assert!(
            stderr.contains(&styled(text, code, err_colour)),
            "{stderr:?}"
        );
    }
    for (prefix, code) in [
        ("gpt-output-fixture | in:40 out:4 cache:10 |", 2),
        ("session:", 2),
        (
            "warning: unknown compaction_model 'missing-fixture-model'",
            33,
        ),
    ] {
        let expected = if err_colour {
            format!("\x1b[{code}m{prefix}")
        } else {
            prefix.to_owned()
        };
        assert!(stderr.contains(&expected), "{stderr:?}");
    }
    assert!(!stderr.contains("mcp: fixture failed:"), "{stderr:?}");
    assert!(stderr.find("▶ ipython").unwrap() < stderr.find("✓ ipython").unwrap());
    assert!(stderr.find("✓ ipython").unwrap() < stderr.find("✗ ipython failed").unwrap());
    if fail {
        assert!(
            stderr.contains(&styled(
                "error: fixture_error: fixture_code: fixture detail\nsecond detail",
                31,
                err_colour
            )),
            "{stderr:?}"
        );
        assert!(stderr.find("✗ ipython failed").unwrap() < stderr.find("session:").unwrap());
        assert!(stderr.find("session:").unwrap() < stderr.find("error: fixture_error").unwrap());
        assert!(!stderr.contains("Location:"), "{stderr:?}");
    }
    assert_eq!(stderr.contains('\x1b'), err_colour, "{stderr:?}");
}

#[test]
fn provider_output_detects_each_stream() {
    for (out_tty, err_tty) in [(false, false), (true, false), (false, true), (true, true)] {
        for fail in [false, true] {
            let fixture = Fixture::new(fail, false);
            let (status, stdout, stderr) = capture(fixture.command(), out_tty, err_tty);
            assert_eq!(status.success(), !fail, "{stderr}");
            assert_messages(&fixture, &stdout, &stderr, err_tty, fail);
            fixture.finish(fail);
        }
    }
}

#[test]
fn colour_environment_matches_clap_policy() {
    for (env, enabled) in [
        (vec![("NO_COLOR", "1")], false),
        (vec![("NO_COLOR", "0")], false),
        (vec![("NO_COLOR", "")], true),
        (vec![("CLICOLOR", "0")], false),
        (vec![("TERM", "dumb")], false),
        (vec![("TERM", "dumb"), ("CLICOLOR", "1")], true),
        (vec![("CLICOLOR_FORCE", "1")], true),
        (vec![("CLICOLOR_FORCE", "0")], true),
        (vec![("CLICOLOR_FORCE", "")], true),
        (vec![("CLICOLOR", "0"), ("CLICOLOR_FORCE", "1")], true),
        (vec![("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")], false),
    ] {
        let fixture = Fixture::new(true, false);
        let mut command = fixture.command();
        command.envs(env.clone());
        let (status, stdout, stderr) = capture(command, true, true);
        assert!(!status.success());
        assert_messages(&fixture, &stdout, &stderr, enabled, true);
        for argument in ["--help", "--invalid-fixture-option"] {
            let mut command = self::command(fixture.dir.path());
            command.envs(env.clone()).arg(argument);
            let (_, stdout, stderr) = capture(command, true, true);
            assert_eq!(
                stdout.contains('\x1b') || stderr.contains('\x1b'),
                enabled,
                "{env:?} {stdout:?} {stderr:?}"
            );
        }
        fixture.finish(true);
    }
}

#[test]
fn forced_capture_and_no_color_preserve_payload_escapes() {
    for (env, enabled) in [
        (vec![], false),
        (vec![("CLICOLOR_FORCE", "1")], true),
        (vec![("CLICOLOR_FORCE", "0")], true),
        (vec![("CLICOLOR_FORCE", "")], false),
        (vec![("CLICOLOR", "1")], false),
        (vec![("CLICOLOR", "0"), ("CLICOLOR_FORCE", "1")], true),
        (vec![("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")], false),
        (vec![("CI", "true")], false),
    ] {
        let fixture = Fixture::new(false, true);
        let mut command = fixture.command();
        command.envs(env);
        let (status, stdout, stderr) = capture(command, false, false);
        assert!(status.success(), "{stderr}");
        assert_messages(&fixture, &stdout, &stderr, enabled, false);
        fixture.finish(false);
    }
}

#[test]
fn worker_file_capture_keeps_message_order() {
    let fixture = Fixture::new(true, false);
    let path = fixture.dir.path().join("round-001.log");
    let file = File::create(&path).unwrap();
    let mut child = fixture
        .command()
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    assert!(!wait(&mut child).success());
    let output = std::fs::read_to_string(path).unwrap();
    assert!(!output.contains('\x1b'), "{output:?}");
    let mut rest = output.as_str();
    for text in [
        "fixture thinking",
        "before tools",
        "fixture answer",
        "Error:",
    ] {
        assert!(!output.contains(text), "{text:?} in {output:?}");
    }
    for text in [
        "▶ ipython",
        "✓ ipython",
        "gpt-output-fixture |",
        "▶ ipython",
        "✗ ipython failed",
        "session:",
        "error: fixture_error: fixture_code: fixture detail\nsecond detail",
    ] {
        let index = rest
            .find(text)
            .unwrap_or_else(|| panic!("missing {text:?} in {rest:?}"));
        rest = &rest[index + text.len()..];
    }
    fixture.finish(true);
}

#[test]
fn commands_and_config_warnings_detect_each_stream() {
    for (out_tty, err_tty) in [(false, false), (true, false), (false, true), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config/mush");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("config.toml"), "model = [").unwrap();
        std::fs::write(config.join("settings.toml"), "scope = [").unwrap();
        for subcommand in ["status", "models"] {
            let mut command = command(dir.path());
            command.arg(subcommand);
            let (status, stdout, stderr) = capture(command, out_tty, err_tty);
            assert!(status.success());
            assert_eq!(stdout.contains('\x1b'), out_tty);
            assert_eq!(stderr.contains('\x1b'), err_tty);
            assert!(stderr.contains("warning: failed to parse config:"));
            assert!(stderr.contains("settings.toml:"));
            assert!(stderr.contains("TOML parse error"));
        }
        let store = mush_session::SessionStore::new(dir.path().join("data/sessions"));
        let session = mush_session::Session::new("fixture", "/fixture");
        store.save(&session).unwrap();
        let mut list = command(dir.path());
        list.args(["list", "--all"]);
        let (status, stdout, _) = capture(list, out_tty, err_tty);
        assert!(status.success());
        assert_eq!(stdout.contains('\x1b'), out_tty);
        let mut delete = command(dir.path());
        delete.args(["delete", session.meta.id.as_str()]);
        let (status, stdout, stderr) = capture(delete, out_tty, err_tty);
        assert!(status.success());
        assert!(stdout.is_empty());
        assert!(stderr.contains(&styled("✓ deleted session: (untitled)", 32, err_tty)));
        assert_eq!(stderr.contains('\x1b'), err_tty);
    }
}

#[test]
fn clap_help_and_errors_detect_their_destination() {
    let dir = tempfile::tempdir().unwrap();
    for (out_tty, err_tty) in [(false, false), (true, false), (false, true), (true, true)] {
        for (argument, success) in [("--help", true), ("--invalid-fixture-option", false)] {
            let mut command = command(dir.path());
            command.arg(argument);
            let (status, stdout, stderr) = capture(command, out_tty, err_tty);
            assert_eq!(status.success(), success);
            assert_eq!(stdout.contains('\x1b'), success && out_tty);
            assert_eq!(stderr.contains('\x1b'), !success && err_tty);
            if success {
                assert!(stderr.is_empty());
            } else {
                assert!(stdout.is_empty());
            }
        }
    }
}

#[test]
fn setup_errors_and_backtraces_follow_stderr() {
    let dir = tempfile::tempdir().unwrap();
    for (out_tty, err_tty, no_color) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, true),
    ] {
        let mut command = command(dir.path());
        command.env("RUST_LIB_BACKTRACE", "1").args([
            "--model",
            "missing-output-fixture",
            "-p",
            "fixture",
        ]);
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let (status, stdout, stderr) = capture(command, out_tty, err_tty);
        assert!(!status.success());
        assert!(stdout.is_empty());
        assert!(
            stderr.contains("unknown model: missing-output-fixture\n\n      available models:"),
            "{stderr:?}"
        );
        assert!(stderr.contains("Location:"));
        assert!(stderr.contains("BACKTRACE"));
        assert_eq!(stderr.contains('\x1b'), err_tty && !no_color, "{stderr:?}");
    }
}

#[cfg(feature = "profiling")]
#[test]
fn trace_announcement_follows_stderr() {
    for (out_tty, err_tty, no_color) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut command = command(dir.path());
        command.args(["--trace", "models"]);
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let (status, _, stderr) = capture(command, out_tty, err_tty);
        assert!(status.success());
        assert!(stderr.contains("profiling: trace will be written to "));
        assert_eq!(stderr.contains('\x1b'), err_tty && !no_color);
        if err_tty && !no_color {
            assert!(stderr.starts_with("\x1b[35mprofiling:"));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_stdout_is_identical_under_colour_policies() {
    use mush_agent::ipc::{IpcListener, control::Control};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let control = Control::new("fixture-session".into());
    let target = control.target();
    let listener = IpcListener::start_control(
        dir.path().join("control.sock"),
        Arc::new(mush_agent::AgentCard::build(
            "fixture",
            &mush_agent::tool::ToolRegistry::new(),
        )),
        control,
    )
    .unwrap();
    let mut baseline = None;
    for (out_tty, err_tty, env) in [
        (false, false, vec![]),
        (true, false, vec![]),
        (false, true, vec![]),
        (true, true, vec![]),
        (false, false, vec![("CLICOLOR_FORCE", "1")]),
        (true, true, vec![("NO_COLOR", "1")]),
    ] {
        let mut command = command(dir.path());
        command
            .envs(env.clone())
            .args(["control", "--socket"])
            .arg(listener.path())
            .args([
                "--instance",
                &target.instance,
                "--session",
                &target.session,
                "--activation",
                &target.activation,
                "--request-id",
                "fixture-request",
                "cancel",
                "missing",
            ]);
        let (status, stdout, stderr) = capture(command, out_tty, err_tty);
        assert!(!status.success());
        assert!(!stdout.contains('\x1b'));
        let value: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(value["kind"]["payload"]["code"], "unknown_request");
        assert_eq!(
            stderr.contains('\x1b'),
            (err_tty || env.contains(&("CLICOLOR_FORCE", "1")))
                && !env.contains(&("NO_COLOR", "1"))
        );
        if let Some(baseline) = &baseline {
            assert_eq!(&stdout, baseline);
        } else {
            baseline = Some(stdout);
        }
    }
    listener.shutdown().await.unwrap();
}
