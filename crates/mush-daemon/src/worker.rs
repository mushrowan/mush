use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use mush_runtime::{AgentConnection, ConnectionError, ConnectionState, InProcessConnection};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

use crate::protocol::{Frame, Reply, Request, encode};

pub fn bind(path: &Path) -> io::Result<UnixListener> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("socket path has no parent"))?;
    if !parent.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    if path.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", path.display()),
        ));
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve(mut connection: InProcessConnection, listener: UnixListener) -> io::Result<()> {
    loop {
        connection.start_pending().await;
        let stream = tokio::select! {
            accepted = listener.accept() => accepted?.0,
            event = connection.next_event() => {
                if event.is_none() {
                    return Ok(());
                }
                continue;
            }
        };
        if let Attached::Closed = attach(&mut connection, stream).await {
            return Ok(());
        }
    }
}

enum Attached {
    Detached,
    Closed,
}

struct Client {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    last_state: ConnectionState,
}

impl Client {
    async fn write(&mut self, frame: &Frame) -> io::Result<()> {
        let bytes = encode(frame).map_err(io::Error::other)?;
        self.writer.write_all(&bytes).await
    }

    fn state_delta(&mut self, connection: &InProcessConnection) -> Option<ConnectionState> {
        let state = connection.state();
        if state == self.last_state {
            return None;
        }
        self.last_state = state.clone();
        Some(state)
    }
}

async fn attach(connection: &mut InProcessConnection, stream: UnixStream) -> Attached {
    let (reader, writer) = stream.into_split();
    let mut client = Client {
        lines: BufReader::new(reader).lines(),
        writer,
        last_state: connection.state(),
    };
    let hello = Frame::Hello {
        state: client.last_state.clone(),
    };
    if client.write(&hello).await.is_err() {
        return Attached::Detached;
    }
    loop {
        connection.start_pending().await;
        tokio::select! {
            line = client.lines.next_line() => {
                let Ok(Some(line)) = line else {
                    return Attached::Detached;
                };
                let request = match crate::protocol::decode::<Request>(&line) {
                    Ok(request) => request,
                    Err(error) => {
                        tracing::warn!(%error, "dropping a malformed request");
                        continue;
                    }
                };
                let id = request.id();
                let result = handle(connection, request).await;
                let state = client.state_delta(connection);
                let frame = Frame::Response { id, result, state };
                if client.write(&frame).await.is_err() {
                    return Attached::Detached;
                }
            }
            event = connection.next_event() => {
                let Some(event) = event else {
                    let _ = client.write(&Frame::Closed).await;
                    return Attached::Closed;
                };
                let state = client.state_delta(connection);
                let frame = Frame::Event { event, state };
                if client.write(&frame).await.is_err() {
                    return Attached::Detached;
                }
            }
        }
    }
}

async fn handle(
    connection: &mut InProcessConnection,
    request: Request,
) -> Result<Reply, ConnectionError> {
    match request {
        Request::Command { command, .. } => connection
            .send(command)
            .await
            .map(|events| Reply::Events { events }),
        Request::Transcript { .. } => Ok(Reply::Transcript {
            messages: connection.transcript().await,
        }),
        Request::History { .. } => Ok(Reply::History {
            messages: connection.history().await,
        }),
        Request::Tree { .. } => Ok(Reply::Tree {
            tree: connection.tree().await,
        }),
    }
}
