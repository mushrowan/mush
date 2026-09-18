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
    assert!(client.tree().await.is_empty());
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
