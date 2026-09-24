#![expect(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use mush_ai::types::Message;
use mush_daemon::{DaemonConnection, bind, serve};
use mush_runtime::{AgentConnection, ClientCommand, ClientEvent, ConnectionError, SessionCommand};

struct Worker {
    _dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

fn spawn_worker() -> Worker {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let listener = bind(&socket).unwrap();
    let (connection, _requests) = mush_runtime::testing::echo_connection();
    let task = tokio::spawn(serve(connection, listener, None, Default::default()));
    Worker {
        _dir: dir,
        socket,
        task,
    }
}

async fn drain_turn(connection: &mut DaemonConnection) -> Vec<ClientEvent> {
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), connection.next_event())
            .await
            .expect("an event before the timeout")
            .expect("the connection stays open");
        let done = matches!(event, ClientEvent::TurnEnded { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

fn prompt(text: &str) -> ClientCommand {
    ClientCommand::Prompt {
        text: text.into(),
        images: Vec::new(),
    }
}

#[tokio::test]
async fn a_prompt_over_the_socket_runs_a_turn() {
    let worker = spawn_worker();
    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();
    assert!(!client.state().turn_active);

    let started = client.send(prompt("hello")).await.unwrap();
    assert!(matches!(started[..], [ClientEvent::TurnStarted]));
    assert!(client.state().turn_active);

    let events = drain_turn(&mut client).await;
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Agent {
            event: mush_agent::AgentEvent::MessageEnd { .. },
            ..
        }
    )));
    assert!(!client.state().turn_active);
    assert_eq!(client.state().context_len, 2);

    let transcript = client.transcript().await;
    assert!(matches!(&transcript[0], Message::User(user) if user.text() == "hello"));
    assert!(matches!(&transcript[1], Message::Assistant(_)));
}

#[tokio::test]
async fn a_second_client_sees_the_state_the_first_left() {
    let worker = spawn_worker();
    {
        let mut first = DaemonConnection::connect(&worker.socket).await.unwrap();
        first.send(prompt("hello")).await.unwrap();
        drain_turn(&mut first).await;
    }

    let mut second = DaemonConnection::connect(&worker.socket).await.unwrap();
    assert_eq!(second.state().context_len, 2);
    assert_eq!(second.history().await.len(), 2);
    assert!(!worker.task.is_finished());
}

#[tokio::test]
async fn a_turn_keeps_running_while_nobody_is_attached() {
    let worker = spawn_worker();
    {
        let mut first = DaemonConnection::connect(&worker.socket).await.unwrap();
        first.send(prompt("hello")).await.unwrap();
    }

    let mut second = DaemonConnection::connect(&worker.socket).await.unwrap();
    if second.state().turn_active {
        drain_turn(&mut second).await;
    }
    assert_eq!(second.transcript().await.len(), 2);
}

#[tokio::test]
async fn a_session_command_answers_with_its_events() {
    let worker = spawn_worker();
    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();
    client.send(prompt("hello")).await.unwrap();
    drain_turn(&mut client).await;
    let before = client.state().session_id;

    let events = client
        .send(ClientCommand::Session(SessionCommand::New))
        .await
        .unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Session(mush_runtime::SessionEvent::TranscriptReplaced { messages, .. })
            if messages.is_empty()
    )));
    assert_ne!(client.state().session_id, before);
    assert_eq!(client.state().context_len, 0);
    assert!(client.tree_summary().await.is_empty());
}

#[tokio::test]
async fn a_refused_command_comes_back_as_the_same_error() {
    let worker = spawn_worker();
    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();

    let error = client
        .send(ClientCommand::Compact {
            focus: Default::default(),
            auto: false,
        })
        .await
        .unwrap_err();

    assert_eq!(error, ConnectionError::TooShortToCompact);
}

#[tokio::test]
async fn shutdown_closes_the_client_and_ends_the_worker() {
    let worker = spawn_worker();
    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();

    client.send(ClientCommand::Shutdown).await.unwrap();

    assert!(
        tokio::time::timeout(Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        client.send(prompt("late")).await.unwrap_err(),
        ConnectionError::Closed
    );
    tokio::time::timeout(Duration::from_secs(5), worker.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_client_attaching_mid_turn_replays_the_turn_so_far() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let listener = bind(&socket).unwrap();
    let (connection, release) = mush_runtime::testing::stalled_connection();
    let _task = tokio::spawn(serve(connection, listener, None, Default::default()));

    let mut first = DaemonConnection::connect(&socket).await.unwrap();
    first.send(prompt("hello")).await.unwrap();
    let mut seen_delta = false;
    while !seen_delta {
        let event = tokio::time::timeout(Duration::from_secs(5), first.next_event())
            .await
            .unwrap()
            .unwrap();
        seen_delta = matches!(
            event,
            ClientEvent::Agent {
                event: mush_agent::AgentEvent::StreamEvent { .. },
                ..
            }
        );
    }
    drop(first);

    let mut second = DaemonConnection::connect(&socket).await.unwrap();
    assert!(second.state().turn_active);
    let mut replayed = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(500), second.next_event()).await
    {
        replayed.push(event);
    }
    assert!(
        matches!(replayed[0], ClientEvent::TurnStarted),
        "{replayed:?}"
    );
    assert!(
        replayed.iter().any(|event| matches!(
            event,
            ClientEvent::Agent {
                event: mush_agent::AgentEvent::StreamEvent {
                    event: mush_ai::stream::StreamEvent::TextDelta { delta, .. },
                },
                ..
            } if delta == "partial"
        )),
        "{replayed:?}"
    );

    release.notify_one();
    let events = drain_turn(&mut second).await;
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Agent {
            event: mush_agent::AgentEvent::MessageEnd { .. },
            ..
        }
    )));

    drop(second);

    let mut third = DaemonConnection::connect(&socket).await.unwrap();
    assert!(!third.state().turn_active);
    assert_eq!(third.history().await.len(), 2);
}

#[tokio::test]
async fn a_malformed_request_is_answered_with_an_error() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let worker = spawn_worker();
    let stream = tokio::net::UnixStream::connect(&worker.socket)
        .await
        .unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let hello = lines.next_line().await.unwrap().unwrap();
    assert!(hello.contains("\"hello\""));

    writer.write_all(b"{\"type\":\"tree\"}\n").await.unwrap();

    let reply = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(frame["type"], "response");
    assert_eq!(frame["id"], 0);
    assert!(
        frame["result"]["Err"]["data"]
            .as_str()
            .unwrap()
            .starts_with("transport: malformed request")
            || frame["result"]["Err"]["data"]
                .as_str()
                .unwrap()
                .starts_with("malformed request"),
        "{reply}"
    );
}

#[tokio::test]
async fn a_steer_over_the_socket_during_a_tool_call_is_injected_and_announced() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let listener = bind(&socket).unwrap();
    let (connection, requests, gate) = mush_runtime::testing::tool_then_text_connection();
    let _task = tokio::spawn(serve(connection, listener, None, Default::default()));
    let mut client = DaemonConnection::connect(&socket).await.unwrap();

    client.send(prompt("start")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(client.state().turn_active);
    client
        .send(ClientCommand::Steer {
            text: "actually, stop and do y".into(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    gate.notify_one();

    let events = drain_turn(&mut client).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            ClientEvent::Agent {
                event: mush_agent::AgentEvent::SteeringInjected { .. },
                ..
            }
        )),
        "{events:?}"
    );
    let asked = requests.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert!(
        matches!(asked[1].messages.last(), Some(Message::User(u)) if u.text() == "actually, stop and do y")
    );
}

#[tokio::test]
async fn a_steer_and_its_retraction_reach_the_other_client_only() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let listener = bind(&socket).unwrap();
    let (connection, _requests, gate) = mush_runtime::testing::tool_then_text_connection();
    let _task = tokio::spawn(serve(connection, listener, None, Default::default()));
    let mut first = DaemonConnection::connect(&socket).await.unwrap();
    let mut second = DaemonConnection::connect(&socket).await.unwrap();

    first.send(prompt("start")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    for command in [
        ClientCommand::Steer {
            text: "do y".into(),
            images: Vec::new(),
        },
        ClientCommand::RetractSteer {
            text: "do y".into(),
        },
    ] {
        first.send(command).await.unwrap();
    }
    gate.notify_one();

    let seen = drain_turn(&mut second).await;
    let inputs: Vec<_> = seen
        .iter()
        .filter(|event| {
            matches!(
                event,
                ClientEvent::UserInput { .. } | ClientEvent::SteerRetracted { .. }
            )
        })
        .collect();
    assert!(
        matches!(
            &inputs[..],
            [
                ClientEvent::UserInput { text: start, queued: false, .. },
                ClientEvent::UserInput { text: steer, queued: true, .. },
                ClientEvent::SteerRetracted { text: retracted },
            ] if start == "start" && steer == "do y" && retracted == "do y"
        ),
        "{inputs:?}"
    );
    let own = drain_turn(&mut first).await;
    assert!(
        !own.iter().any(|event| matches!(
            event,
            ClientEvent::UserInput { .. } | ClientEvent::SteerRetracted { .. }
        )),
        "{own:?}"
    );
}

#[tokio::test]
async fn a_prompt_after_aborting_a_hung_tool_over_the_socket_starts_a_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let listener = bind(&socket).unwrap();
    let (connection, requests) = mush_runtime::testing::hanging_tool_connection();
    let _task = tokio::spawn(serve(connection, listener, None, Default::default()));
    let mut client = DaemonConnection::connect(&socket).await.unwrap();

    client.send(prompt("start")).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), client.next_event())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event,
            ClientEvent::Agent {
                event: mush_agent::AgentEvent::ToolExecStart { .. },
                ..
            }
        ) {
            break;
        }
    }

    let aborted = client.send(ClientCommand::Abort).await.unwrap();
    assert!(
        aborted
            .iter()
            .any(|event| matches!(event, ClientEvent::TurnEnded { cancelled: true })),
        "{aborted:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    while client.try_next_event().is_some() {}
    let state_after_abort = client.state();

    let started = client.send(prompt("again")).await.unwrap();
    let mut events = started;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !events
        .iter()
        .any(|event| matches!(event, ClientEvent::TurnStarted))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no new turn after abort; state after abort {state_after_abort:?}, now {:?}, events {events:?}",
            client.state()
        );
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(100), client.next_event()).await
        {
            events.push(event);
        }
    }
    drain_turn(&mut client).await;
    let asked = requests.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert!(matches!(asked[1].messages.last(), Some(Message::User(u)) if u.text() == "again"));
}

#[tokio::test]
async fn the_descriptor_follows_the_title_and_the_message_count() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let path = dir.path().join("worker.json");
    let listener = bind(&socket).unwrap();
    let (connection, _requests) = mush_runtime::testing::echo_connection();
    let descriptor =
        mush_daemon::Descriptor::new(path.clone(), socket.clone(), dir.path().to_path_buf());
    let task = tokio::spawn(serve(
        connection,
        listener,
        Some(descriptor),
        Default::default(),
    ));

    let mut client = DaemonConnection::connect(&socket).await.unwrap();
    client
        .send(ClientCommand::Session(SessionCommand::Name {
            title: Some("scout".into()),
        }))
        .await
        .unwrap();
    client.send(prompt("hello")).await.unwrap();
    drain_turn(&mut client).await;

    let written = read_descriptor(&path, |written| written.messages == 2).await;
    assert_eq!(written.title.as_deref(), Some("scout"));
    assert!(!written.turn_active);
    assert!(written.updated_at >= written.started_at);
    assert!(!task.is_finished());
}

async fn read_descriptor(
    path: &std::path::Path,
    ready: impl Fn(&mush_daemon::WorkerDescriptor) -> bool,
) -> mush_daemon::WorkerDescriptor {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(written) = serde_json::from_slice::<mush_daemon::WorkerDescriptor>(&bytes)
            && ready(&written)
        {
            return written;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the descriptor never caught up"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_worker_that_never_says_hello_times_the_client_out() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("silent.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let _accepting = tokio::spawn(async move {
        let held = listener.accept().await;
        std::future::pending::<()>().await;
        drop(held);
    });

    let Err(error) = DaemonConnection::connect_within(&socket, Duration::from_millis(200)).await
    else {
        panic!("the client got in")
    };
    assert!(
        matches!(error, mush_daemon::ConnectError::TimedOut(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn two_clients_share_a_worker_and_both_see_the_turn() {
    let worker = spawn_worker();
    let mut first = DaemonConnection::connect(&worker.socket).await.unwrap();
    let mut second = DaemonConnection::connect(&worker.socket).await.unwrap();

    let started = first.send(prompt("hello")).await.unwrap();
    assert!(matches!(started[..], [ClientEvent::TurnStarted]));

    let seen = drain_turn(&mut second).await;
    assert!(matches!(
        &seen[..2],
        [
            ClientEvent::UserInput { text, queued: false, .. },
            ClientEvent::TurnStarted,
        ] if text == "hello"
    ));
    assert!(seen.iter().any(|event| matches!(
        event,
        ClientEvent::Agent {
            event: mush_agent::AgentEvent::MessageEnd { .. },
            ..
        }
    )));
    drain_turn(&mut first).await;
    assert_eq!(first.state().context_len, 2);
    assert_eq!(second.state().context_len, 2);
}

#[tokio::test]
async fn a_command_is_answered_to_its_sender_once() {
    let worker = spawn_worker();
    let mut first = DaemonConnection::connect(&worker.socket).await.unwrap();
    let mut second = DaemonConnection::connect(&worker.socket).await.unwrap();

    let events = first
        .send(ClientCommand::Session(SessionCommand::Name {
            title: Some("scout".into()),
        }))
        .await
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        ClientEvent::Session(mush_runtime::SessionEvent::Title(Some(title))) if title == "scout"
    )));

    let echoed = tokio::time::timeout(Duration::from_secs(5), second.next_event())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        echoed,
        ClientEvent::Session(mush_runtime::SessionEvent::Title(Some(title))) if title == "scout"
    ));
    assert!(first.try_next_event().is_none());
}

struct RawClient {
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl RawClient {
    #[expect(clippy::panic, reason = "the fixture needs the hello frame")]
    async fn connect(socket: &std::path::Path) -> (Self, mush_daemon::Cursor) {
        use tokio::io::AsyncBufReadExt;
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut client = Self {
            lines: tokio::io::BufReader::new(reader).lines(),
            writer,
        };
        let mush_daemon::Frame::Hello { cursor, .. } = client.read().await else {
            panic!("expected a hello")
        };
        (client, cursor)
    }

    async fn read(&mut self) -> mush_daemon::Frame {
        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("a frame before the timeout")
            .unwrap()
            .expect("the worker keeps the link open");
        serde_json::from_str(&line).unwrap()
    }

    async fn command(&mut self, id: u64, text: &str, command_id: &str) {
        self.versioned(id, text, command_id, 1).await;
    }

    async fn versioned(&mut self, id: u64, text: &str, command_id: &str, version: u32) {
        use tokio::io::AsyncWriteExt;
        let request = serde_json::json!({
            "type": "command",
            "id": id,
            "command": {"type": "prompt", "data": {"text": text, "images": []}},
            "envelope": {
                "version": version,
                "client": "raw",
                "command": command_id,
            },
        });
        let line = format!("{request}\n");
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn acked(&mut self, id: u64, text: &str, command_id: &str, ack: &str) {
        use tokio::io::AsyncWriteExt;
        let request = serde_json::json!({
            "type": "command",
            "id": id,
            "command": {"type": "prompt", "data": {"text": text, "images": []}},
            "envelope": {
                "version": 1,
                "client": "raw",
                "command": command_id,
                "ack": ack,
            },
        });
        let line = format!("{request}\n");
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn attach(&mut self, from: Option<mush_daemon::Cursor>) {
        use tokio::io::AsyncWriteExt;
        let request = serde_json::json!({
            "type": "attach",
            "id": 1,
            "client": "raw",
            "from": from,
        });
        let line = format!("{request}\n");
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }
}

#[tokio::test]
async fn a_cursor_replay_delivers_only_what_was_missed() {
    let worker = spawn_worker();
    let mut first = DaemonConnection::connect(&worker.socket).await.unwrap();
    first.send(prompt("hello")).await.unwrap();
    drain_turn(&mut first).await;
    let cursor = first.cursor().clone();
    assert!(cursor.sequence > 2, "{cursor:?}");

    let (mut raw, hello) = RawClient::connect(&worker.socket).await;
    assert_eq!(hello, cursor);
    raw.attach(Some(mush_daemon::Cursor {
        generation: cursor.generation.clone(),
        sequence: cursor.sequence - 2,
    }))
    .await;

    let mut replayed = Vec::new();
    loop {
        match raw.read().await {
            mush_daemon::Frame::Event { sequence, .. } => replayed.push(sequence),
            mush_daemon::Frame::Response { result, .. } => {
                assert!(matches!(
                    result,
                    Ok(mush_daemon::Reply::Attached {
                        snapshot: false,
                        ..
                    })
                ));
                break;
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert_eq!(replayed, vec![cursor.sequence - 1, cursor.sequence]);
}

#[tokio::test]
async fn a_cursor_from_another_worker_gets_a_fresh_snapshot() {
    let worker = spawn_worker();
    let mut first = DaemonConnection::connect(&worker.socket).await.unwrap();
    first.send(prompt("hello")).await.unwrap();
    drain_turn(&mut first).await;

    let (mut raw, _) = RawClient::connect(&worker.socket).await;
    raw.attach(Some(mush_daemon::Cursor {
        generation: "a worker that is gone".into(),
        sequence: 3,
    }))
    .await;

    let mush_daemon::Frame::Event { event, .. } = raw.read().await else {
        panic!("expected a snapshot")
    };
    let ClientEvent::Session(mush_runtime::SessionEvent::TranscriptReplaced {
        reason,
        messages,
        ..
    }) = event
    else {
        panic!("expected a transcript")
    };
    assert_eq!(reason, mush_runtime::Replaced::Resynced);
    assert_eq!(messages.len(), 2);
    let mush_daemon::Frame::Response { result, .. } = raw.read().await else {
        panic!("expected the attach response")
    };
    assert!(matches!(
        result,
        Ok(mush_daemon::Reply::Attached { snapshot: true, .. })
    ));
}

#[tokio::test]
async fn a_client_that_goes_away_is_forgotten_and_the_worker_lives_on() {
    let worker = spawn_worker();
    let stalled = tokio::net::UnixStream::connect(&worker.socket)
        .await
        .unwrap();
    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();
    client.send(prompt("hello")).await.unwrap();
    drain_turn(&mut client).await;
    drop(stalled);

    client.send(prompt("again")).await.unwrap();
    drain_turn(&mut client).await;
    assert_eq!(client.state().context_len, 4);
    assert!(!worker.task.is_finished());
}

#[tokio::test]
async fn a_client_reattaches_when_the_worker_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let listener = bind(&socket).unwrap();
    let (connection, _requests) = mush_runtime::testing::echo_connection();
    let first = tokio::spawn(serve(connection, listener, None, Default::default()));

    let mut client = DaemonConnection::connect(&socket).await.unwrap();
    client.send(prompt("hello")).await.unwrap();
    drain_turn(&mut client).await;
    assert_eq!(client.state().context_len, 2);

    first.abort();
    let _ = first.await;
    std::fs::remove_file(&socket).unwrap();
    let listener = bind(&socket).unwrap();
    let (replacement, _also) = mush_runtime::testing::echo_connection();
    let second = tokio::spawn(serve(replacement, listener, None, Default::default()));

    let event = tokio::time::timeout(Duration::from_secs(10), client.next_event())
        .await
        .expect("the client reattaches")
        .expect("the connection survives");
    assert!(
        matches!(
            event,
            ClientEvent::Session(mush_runtime::SessionEvent::TranscriptReplaced {
                reason: mush_runtime::Replaced::Resynced,
                ..
            })
        ),
        "{event:?}"
    );
    assert!(!client.closed());
    client.send(prompt("again")).await.unwrap();
    drain_turn(&mut client).await;
    assert!(!second.is_finished());
}

#[tokio::test]
async fn the_journal_answers_a_repeated_command_without_running_it_again() {
    let worker = spawn_worker();
    let (mut raw, _) = RawClient::connect(&worker.socket).await;
    raw.attach(None).await;
    let mush_daemon::Frame::Response { .. } = raw.read().await else {
        panic!("expected the attach response")
    };

    for id in [1, 2] {
        raw.command(id, "hello", "cmd-1").await;
        loop {
            match raw.read().await {
                mush_daemon::Frame::Response { result, .. } => {
                    let Ok(mush_daemon::Reply::Events { events }) = result else {
                        panic!("expected events")
                    };
                    assert!(matches!(events[..], [ClientEvent::TurnStarted]));
                    break;
                }
                mush_daemon::Frame::Event { .. } => {}
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let transcript = client.transcript().await;
        if transcript.len() >= 2 {
            assert_eq!(transcript.len(), 2, "the repeat ran a second turn");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the turn never ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(client.transcript().await.len(), 2);
}

#[tokio::test]
async fn an_acked_command_leaves_the_journal() {
    let worker = spawn_worker();
    let (mut raw, _) = RawClient::connect(&worker.socket).await;
    raw.attach(None).await;
    let _ = raw.read().await;

    raw.command(1, "hello", "cmd-1").await;
    wait_for_response(&mut raw).await;
    raw.acked(2, "hello", "cmd-2", "cmd-1").await;
    wait_for_response(&mut raw).await;
    raw.command(3, "hello", "cmd-1").await;
    wait_for_response(&mut raw).await;

    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if client.transcript().await.len() >= 6 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the acked command should run again: {:?}",
            client.transcript().await.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_empty_worker_ends_when_its_last_attached_client_leaves() {
    let worker = spawn_worker();
    let probe = tokio::net::UnixStream::connect(&worker.socket)
        .await
        .unwrap();
    drop(probe);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !worker.task.is_finished(),
        "a client that never attached is no detach"
    );

    let client = DaemonConnection::connect(&worker.socket).await.unwrap();
    drop(client);
    tokio::time::timeout(Duration::from_secs(5), worker.task)
        .await
        .expect("the worker ends")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_worker_with_a_conversation_outlives_its_last_client() {
    let worker = spawn_worker();
    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();
    client.send(prompt("hello")).await.unwrap();
    drain_turn(&mut client).await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!worker.task.is_finished());
}

#[tokio::test]
async fn a_command_from_another_envelope_version_is_refused() {
    let worker = spawn_worker();
    let (mut raw, _) = RawClient::connect(&worker.socket).await;
    raw.attach(None).await;
    let _ = raw.read().await;

    raw.versioned(1, "hello", "cmd-1", 2).await;
    let result = loop {
        if let mush_daemon::Frame::Response { result, .. } = raw.read().await {
            break result;
        }
    };
    let Err(ConnectionError::Refused(error)) = result else {
        panic!("expected a refusal, got {result:?}")
    };
    assert!(error.contains("version 2"), "{error}");

    let mut client = DaemonConnection::connect(&worker.socket).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(client.transcript().await.is_empty());
}

async fn wait_for_response(raw: &mut RawClient) {
    loop {
        if let mush_daemon::Frame::Response { .. } = raw.read().await {
            return;
        }
    }
}
