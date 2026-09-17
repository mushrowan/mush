use std::collections::VecDeque;
use std::io;
use std::path::Path;

use mush_ai::types::Message;
use mush_runtime::{AgentConnection, ClientCommand, ClientEvent, ConnectionError, ConnectionState};
use mush_session::tree::SessionTree;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;

use crate::protocol::{CodecError, Frame, Reply, Request, RequestId, decode, encode};

const FRAME_BUFFER: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("the worker closed before saying hello")]
    NoHello,
    #[error("the worker's first frame was not a hello")]
    BadHello,
    #[error(transparent)]
    Codec(CodecError),
}

pub struct DaemonConnection {
    writer: OwnedWriteHalf,
    frames: mpsc::Receiver<Result<Frame, CodecError>>,
    reader: tokio::task::JoinHandle<()>,
    buffered: VecDeque<ClientEvent>,
    state: ConnectionState,
    worker_build: String,
    next_id: RequestId,
    closed: bool,
}

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl DaemonConnection {
    pub async fn connect(path: &Path) -> Result<Self, ConnectError> {
        let stream = UnixStream::connect(path).await?;
        Self::from_stream(stream).await
    }

    pub async fn from_stream(stream: UnixStream) -> Result<Self, ConnectError> {
        let (reader, writer) = stream.into_split();
        let (tx, mut frames) = mpsc::channel(FRAME_BUFFER);
        let reader = tokio::spawn(read_frames(reader, tx));
        let (state, worker_build) = match frames.recv().await {
            Some(Ok(Frame::Hello { state, build })) => (state, build),
            Some(Ok(_)) => return Err(ConnectError::BadHello),
            Some(Err(error)) => return Err(ConnectError::Codec(error)),
            None => return Err(ConnectError::NoHello),
        };
        Ok(Self {
            writer,
            frames,
            reader,
            buffered: VecDeque::new(),
            state,
            worker_build,
            next_id: 1,
            closed: false,
        })
    }

    #[must_use]
    pub fn worker_build(&self) -> &str {
        &self.worker_build
    }

    fn absorb_state(&mut self, state: Option<ConnectionState>) {
        if let Some(state) = state {
            self.state = state;
        }
    }

    fn absorb(&mut self, frame: Frame) -> Option<(RequestId, Result<Reply, ConnectionError>)> {
        match frame {
            Frame::Event { event, state } => {
                self.absorb_state(state);
                self.buffered.push_back(event);
                None
            }
            Frame::Response { id, result, state } => {
                self.absorb_state(state);
                Some((id, result))
            }
            Frame::Hello { state, .. } => {
                self.state = state;
                None
            }
            Frame::Closed => {
                self.closed = true;
                None
            }
        }
    }

    async fn request(&mut self, request: Request) -> Result<Reply, ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        let id = request.id();
        let bytes =
            encode(&request).map_err(|error| ConnectionError::Transport(error.to_string()))?;
        self.writer
            .write_all(&bytes)
            .await
            .map_err(|error| ConnectionError::Transport(error.to_string()))?;
        loop {
            let frame = match self.frames.recv().await {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => return Err(ConnectionError::Transport(error.to_string())),
                None => {
                    self.closed = true;
                    return Err(ConnectionError::Closed);
                }
            };
            match self.absorb(frame) {
                Some((reply_id, result)) if reply_id == id => return result,
                Some((reply_id, _)) => {
                    tracing::warn!(reply_id, expected = id, "dropping a stray response");
                }
                None if self.closed => return Err(ConnectionError::Closed),
                None => {}
            }
        }
    }

    fn next_request_id(&mut self) -> RequestId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

async fn read_frames(
    reader: tokio::net::unix::OwnedReadHalf,
    tx: mpsc::Sender<Result<Frame, CodecError>>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if tx.send(decode::<Frame>(&line)).await.is_err() {
            return;
        }
    }
}

#[async_trait::async_trait]
impl AgentConnection for DaemonConnection {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    async fn send(&mut self, command: ClientCommand) -> Result<Vec<ClientEvent>, ConnectionError> {
        let id = self.next_request_id();
        match self.request(Request::Command { id, command }).await? {
            Reply::Events { events } => Ok(events),
            other => Err(ConnectionError::Transport(format!(
                "unexpected reply to a command: {other:?}"
            ))),
        }
    }

    async fn next_event(&mut self) -> Option<ClientEvent> {
        loop {
            if let Some(event) = self.buffered.pop_front() {
                return Some(event);
            }
            if self.closed {
                return None;
            }
            let frame = match self.frames.recv().await {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => {
                    tracing::warn!(%error, "dropping a malformed frame");
                    continue;
                }
                None => {
                    self.closed = true;
                    return None;
                }
            };
            if let Some((id, _)) = self.absorb(frame) {
                tracing::warn!(id, "dropping a response nobody waited for");
            }
        }
    }

    fn try_next_event(&mut self) -> Option<ClientEvent> {
        loop {
            if let Some(event) = self.buffered.pop_front() {
                return Some(event);
            }
            if self.closed {
                return None;
            }
            let frame = match self.frames.try_recv().ok()? {
                Ok(frame) => frame,
                Err(error) => {
                    tracing::warn!(%error, "dropping a malformed frame");
                    continue;
                }
            };
            if let Some((id, _)) = self.absorb(frame) {
                tracing::warn!(id, "dropping a response nobody waited for");
            }
        }
    }

    fn closed(&self) -> bool {
        self.closed
    }

    fn state(&self) -> ConnectionState {
        self.state.clone()
    }

    async fn transcript(&mut self) -> Vec<Message> {
        let id = self.next_request_id();
        match self.request(Request::Transcript { id }).await {
            Ok(Reply::Transcript { messages }) => messages,
            _ => Vec::new(),
        }
    }

    async fn history(&mut self) -> Vec<Message> {
        let id = self.next_request_id();
        match self.request(Request::History { id }).await {
            Ok(Reply::History { messages }) => messages,
            _ => Vec::new(),
        }
    }

    async fn tree(&mut self) -> SessionTree {
        let id = self.next_request_id();
        match self.request(Request::Tree { id }).await {
            Ok(Reply::Tree { tree }) => tree,
            _ => SessionTree::default(),
        }
    }
}
