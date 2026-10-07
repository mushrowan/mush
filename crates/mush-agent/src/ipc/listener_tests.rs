use super::control::{Control, ControlError, Event, Operation, Outcome, Reply, Request};
use super::listener::*;
use super::{IpcMessage, IpcMessageKind};
use crate::{card::AgentCard, tool::ToolRegistry};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[tokio::test]
async fn reviewer_rejects_parent_symlink_and_fifo_and_closes_partial_peers() {
    use std::os::unix::fs::{FileTypeExt, symlink};
    use std::time::Duration;
    let dir = crate::ipc::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let linked = dir.path().join("linked");
    symlink(&real, &linked).unwrap();
    let control = Control::new("session".into());
    assert!(
        IpcListener::start_control(linked.join("fixture.sock"), card(), control.clone()).is_err()
    );
    let fifo = real.join("fifo.sock");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert!(IpcListener::start_control(fifo.clone(), card(), control.clone()).is_err());
    assert!(
        std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo()
    );
    let socket = real.join("fixture.sock");
    let listener = IpcListener::start_control(socket.clone(), card(), control).unwrap();
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    assert_eq!(stream.peer_cred().unwrap().uid(), owner_uid());
    stream.write_all(b"{\"id\":").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(1), listener.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(!socket.exists());
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

fn card() -> Arc<AgentCard> {
    Arc::new(AgentCard::build("fixture", &ToolRegistry::new()))
}

fn runtime() -> tempfile::TempDir {
    let dir = crate::ipc::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn send(control: &Control, id: &str) -> IpcMessage {
    IpcMessage {
        id: id.into(),
        from: "fixture".into(),
        kind: IpcMessageKind::Control(Request {
            target: control.target(),
            operation: Operation::Send {
                event: Event::Message { text: id.into() },
            },
        }),
    }
}

async fn raw(path: &std::path::Path, bytes: &[u8]) -> IpcMessage {
    let mut stream = UnixStream::connect(path).await.unwrap();
    stream.write_all(bytes).await.unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

fn error(response: IpcMessage, expected: ControlError) {
    assert_eq!(
        response.kind,
        IpcMessageKind::ControlReply(Reply::Error { code: expected })
    );
}

#[tokio::test]
async fn simultaneous_clients_and_instances_are_isolated() {
    let dir = runtime();
    let a = Control::new("session".into());
    let b = Control::new("session".into());
    let la = IpcListener::start_control(dir.path().join("a.sock"), card(), a.clone()).unwrap();
    let lb = IpcListener::start_control(dir.path().join("b.sock"), card(), b.clone()).unwrap();
    let requests: Vec<_> = (0..8).map(|n| send(&a, &format!("a-{n}"))).collect();
    let responses =
        futures::future::join_all(requests.iter().map(|r| exchange(la.path(), r))).await;
    for response in responses {
        assert!(matches!(
            response.unwrap().kind,
            IpcMessageKind::ControlReply(Reply::Accepted {
                outcome: Outcome::Queued,
                ..
            })
        ));
    }
    assert!(b.take_next(&b.target()).is_none());
    error(
        exchange(lb.path(), &requests[0]).await.unwrap(),
        ControlError::Unavailable,
    );
    let mut ids = std::collections::HashSet::new();
    while let Some(delivery) = a.take_next(&a.target()) {
        assert_eq!(delivery.peer_uid, owner_uid());
        assert!(ids.insert(delivery.id));
        a.finish(Outcome::Completed);
    }
    assert_eq!(ids.len(), requests.len());
    assert!(matches!(
        exchange(la.path(), &requests[0]).await.unwrap().kind,
        IpcMessageKind::ControlReply(Reply::Accepted {
            duplicate: true,
            ..
        })
    ));
    la.shutdown().await.unwrap();
    lb.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_unknown_oversized_and_unbound_messages_get_errors() {
    let dir = runtime();
    let control = Control::new("session".into());
    let listener =
        IpcListener::start_control(dir.path().join("a.sock"), card(), control.clone()).unwrap();
    error(
        raw(listener.path(), b"{\n").await,
        ControlError::InvalidPayload,
    );
    let unknown = raw(
        listener.path(),
        b"{\"id\":\"unknown\",\"from\":\"fixture\",\"kind\":{\"type\":\"Execute\"}}\n",
    )
    .await;
    assert_eq!(unknown.id, "unknown");
    error(unknown, ControlError::UnknownRequest);
    error(raw(listener.path(), b"{\"id\":\"unbound\",\"from\":\"fixture\",\"kind\":{\"type\":\"UserMessage\",\"payload\":{\"text\":\"hello\"}}}\n").await, ControlError::InvalidPayload);
    error(
        raw(listener.path(), &vec![b'x'; MAX_REQUEST_BYTES + 1]).await,
        ControlError::TooLarge,
    );
    let mut request = send(&control, "oversized");
    request.kind = IpcMessageKind::Control(Request {
        target: control.target(),
        operation: Operation::Send {
            event: Event::Message {
                text: "x".repeat(super::control::MAX_TEXT + 1),
            },
        },
    });
    error(
        exchange(listener.path(), &request).await.unwrap(),
        ControlError::InvalidPayload,
    );
    request.kind = IpcMessageKind::UserMessage {
        text: "bound".into(),
        target: Some(control.target()),
    };
    assert!(matches!(
        exchange(listener.path(), &request).await.unwrap().kind,
        IpcMessageKind::ControlReply(Reply::Accepted {
            outcome: Outcome::Queued,
            ..
        })
    ));
    listener.shutdown().await.unwrap();
}

#[tokio::test]
async fn socket_queue_full_and_unavailable_are_explicit() {
    let dir = runtime();
    let control = Control::new("session".into());
    let listener =
        IpcListener::start_control(dir.path().join("a.sock"), card(), control.clone()).unwrap();
    for n in 0..super::control::MAX_QUEUE {
        assert!(matches!(
            exchange(listener.path(), &send(&control, &format!("id-{n}")))
                .await
                .unwrap()
                .kind,
            IpcMessageKind::ControlReply(Reply::Accepted { .. })
        ));
    }
    error(
        exchange(listener.path(), &send(&control, "full"))
            .await
            .unwrap(),
        ControlError::QueueFull,
    );
    listener.shutdown().await.unwrap();
    let listener = IpcListener::start(dir.path().join("b.sock"), card()).unwrap();
    error(
        exchange(listener.path(), &send(&control, "unavailable"))
            .await
            .unwrap(),
        ControlError::Unavailable,
    );
    listener.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn partial_request_timeout_does_not_block_other_clients() {
    let dir = runtime();
    let control = Control::new("session".into());
    let listener =
        IpcListener::start_control(dir.path().join("a.sock"), card(), control.clone()).unwrap();
    let mut slow = UnixStream::connect(listener.path()).await.unwrap();
    slow.write_all(b"{").await.unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        exchange(listener.path(), &send(&control, "fast"))
            .await
            .unwrap()
            .kind,
        IpcMessageKind::ControlReply(Reply::Accepted { .. })
    ));
    tokio::time::advance(REQUEST_TIMEOUT).await;
    let mut line = String::new();
    BufReader::new(slow).read_line(&mut line).await.unwrap();
    error(serde_json::from_str(&line).unwrap(), ControlError::Timeout);
    listener.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_limit_and_shutdown_close_all_clients() {
    let dir = runtime();
    let control = Control::new("session".into());
    let listener =
        IpcListener::start_control(dir.path().join("a.sock"), card(), control.clone()).unwrap();
    let mut clients = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        let client = UnixStream::connect(listener.path()).await.unwrap();
        clients.push(client);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }
    let overflow = UnixStream::connect(listener.path()).await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        REQUEST_TIMEOUT,
        BufReader::new(overflow).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    error(
        serde_json::from_str(&line).unwrap(),
        ControlError::ConnectionLimit,
    );
    let path = listener.path().to_owned();
    listener.shutdown().await.unwrap();
    assert!(!path.exists());
    for mut client in clients {
        assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    }
    let status = control.request(
        "status",
        "fixture",
        owner_uid(),
        Request {
            target: control.target(),
            operation: Operation::Status { id: None },
        },
    );
    assert!(matches!(
        status,
        Reply::Status(super::control::Status {
            available: false,
            ..
        })
    ));
}

#[tokio::test]
async fn socket_permissions_symlinks_stale_files_and_cleanup_ownership() {
    let dir = runtime();
    let legacy_dir = dir.path().join("legacy");
    std::fs::create_dir(&legacy_dir).unwrap();
    std::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let protected =
        IpcListener::start(legacy_dir.join("control").join("new.sock"), card()).unwrap();
    assert!(validate_socket(protected.path()).is_ok());
    protected.shutdown().await.unwrap();
    let path = dir.path().join("a.sock");
    let listener = IpcListener::start(path.clone(), card()).unwrap();
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert!(IpcListener::start(path.clone(), card()).is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "replacement").unwrap();
    listener.shutdown().await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
    assert!(validate_socket(&path).is_err());
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(IpcListener::start(link, card()).is_err());
    let ancestor_link = dir.path().join("ancestor");
    std::os::unix::fs::symlink(dir.path(), &ancestor_link).unwrap();
    assert!(IpcListener::start(ancestor_link.join("new.sock"), card()).is_err());
    let stale = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert!(IpcListener::start(stale.clone(), card()).is_err());
    assert!(stale.exists());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(IpcListener::start(dir.path().join("unsafe.sock"), card()).is_err());
}
