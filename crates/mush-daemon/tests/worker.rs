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
    let task = tokio::spawn(serve(connection, listener, None));
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
