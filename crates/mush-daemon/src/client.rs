use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::time::Duration;

use mush_ai::types::Message;
use mush_runtime::{AgentConnection, ClientCommand, ClientEvent, ConnectionError, ConnectionState};
use mush_session::tree::TreeSummaryRow;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;

use crate::protocol::{
    ClientId, CodecError, CommandId, Cursor, ENVELOPE_VERSION, Envelope, Frame, Reply, Request,
    RequestId, decode, encode,
};

const FRAME_BUFFER: usize = 256;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const PROVIDER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
pub const RECONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const RECONNECT_BACKOFF_CAP: Duration = Duration::from_secs(2);

fn timeout_for(command: &ClientCommand) -> Duration {
    match command {
        ClientCommand::Prompt { .. }
        | ClientCommand::QueuePrompt { .. }
        | ClientCommand::AbortAndSendQueued
        | ClientCommand::Compact { .. }
        | ClientCommand::CompactionRetry
        | ClientCommand::CompactionFallback
        | ClientCommand::SummariseAndNavigate { .. }
        | ClientCommand::Refine(_)
        | ClientCommand::SideQuestion(_)
        | ClientCommand::Session(_) => PROVIDER_REQUEST_TIMEOUT,
        _ => REQUEST_TIMEOUT,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("the worker closed before saying hello")]
    NoHello,
    #[error("the worker did not answer within {0:?}")]
    TimedOut(Duration),
    #[error("the worker refused the attach: {0}")]
    Attach(String),
    #[error("the worker's first frame was not a hello")]
    BadHello,
    #[error(transparent)]
    Codec(CodecError),
}

pub struct DaemonConnection {
    path: std::path::PathBuf,
    client: ClientId,
    cursor: Cursor,
    link: Link,
    buffered: VecDeque<ClientEvent>,
    state: ConnectionState,
    worker_build: String,
    next_id: RequestId,
    next_command: u64,
    acked: Option<CommandId>,
    closed: bool,
}

struct Link {
    writer: OwnedWriteHalf,
    frames: mpsc::Receiver<Result<Frame, CodecError>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl DaemonConnection {
    pub async fn connect(path: &Path) -> Result<Self, ConnectError> {
        Self::connect_within(path, CONNECT_TIMEOUT).await
    }

    pub async fn connect_within(path: &Path, timeout: Duration) -> Result<Self, ConnectError> {
        let (link, state, worker_build, cursor) = dial(path, timeout).await?;
        let mut connection = Self {
            path: path.to_path_buf(),
            client: mush_ai::types::SessionId::new().to_string(),
            cursor,
            link,
            buffered: VecDeque::new(),
            state,
            worker_build,
            next_id: 1,
            next_command: 1,
            acked: None,
            closed: false,
        };
        tokio::time::timeout(timeout, connection.attach(None))
            .await
            .map_err(|_| ConnectError::TimedOut(timeout))?
            .map_err(|error| ConnectError::Attach(error.to_string()))?;
        Ok(connection)
    }

    async fn attach(&mut self, from: Option<Cursor>) -> Result<bool, ConnectionError> {
        let id = self.next_request_id();
        let request = Request::Attach {
            id,
            client: self.client.clone(),
            from,
        };
        match self.exchange(request).await? {
            Reply::Attached { cursor, snapshot } => {
                self.cursor = cursor;
                Ok(snapshot)
            }
            other => Err(ConnectionError::Transport(format!(
                "unexpected reply to attach: {other:?}"
            ))),
        }
    }

    async fn reconnect(&mut self) -> bool {
        if self.closed {
            return false;
        }
        let deadline = tokio::time::Instant::now() + RECONNECT_TIMEOUT;
        let mut backoff = Duration::from_millis(50);
        loop {
            if let Ok((link, state, worker_build, _)) = dial(&self.path, CONNECT_TIMEOUT).await {
                self.link = link;
                self.state = state;
                self.worker_build = worker_build;
                let from = Some(self.cursor.clone());
                if self.attach(from).await.is_ok() {
                    tracing::info!(cursor = self.cursor.sequence, "reattached to the worker");
                    return true;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                self.closed = true;
                return false;
            }
            tokio::time::sleep_until((tokio::time::Instant::now() + backoff).min(deadline)).await;
            backoff = (backoff * 2).min(RECONNECT_BACKOFF_CAP);
        }
    }

    #[must_use]
    pub fn worker_build(&self) -> &str {
        &self.worker_build
    }

    #[must_use]
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }

    fn absorb_state(&mut self, state: Option<ConnectionState>) {
        if let Some(state) = state {
            self.state = state;
        }
    }

    fn absorb(&mut self, frame: Frame) -> Option<(RequestId, Result<Reply, ConnectionError>)> {
        match frame {
            Frame::Event {
                event,
                state,
                sequence,
            } => {
                self.absorb_state(state);
                self.cursor.sequence = self.cursor.sequence.max(sequence);
                self.buffered.push_back(event);
                None
            }
            Frame::Response { id, result, state } => {
                self.absorb_state(state);
                Some((id, result))
            }
            Frame::Hello { state, cursor, .. } => {
                self.state = state;
                self.cursor = cursor;
                None
            }
            Frame::Closed => {
                self.closed = true;
                None
            }
        }
    }

    async fn request(&mut self, request: Request) -> Result<Reply, ConnectionError> {
        let timeout = match &request {
            Request::Command { command, .. } => timeout_for(command),
            _ => REQUEST_TIMEOUT,
        };
        match tokio::time::timeout(timeout, self.request_forever(request)).await {
            Ok(result) => result,
            Err(_) => Err(ConnectionError::TimedOut),
        }
    }

    async fn request_forever(&mut self, request: Request) -> Result<Reply, ConnectionError> {
        match self.exchange(request).await {
            Err(ConnectionError::Closed) if !self.closed => {
                if self.reconnect().await {
                    Err(ConnectionError::Reconnected)
                } else {
                    Err(ConnectionError::Closed)
                }
            }
            other => other,
        }
    }

    async fn exchange(&mut self, request: Request) -> Result<Reply, ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        let id = request.id();
        let bytes =
            encode(&request).map_err(|error| ConnectionError::Transport(error.to_string()))?;
        self.link
            .writer
            .write_all(&bytes)
            .await
            .map_err(|error| ConnectionError::Transport(error.to_string()))?;
        loop {
            let frame = match self.link.frames.recv().await {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => return Err(ConnectionError::Transport(error.to_string())),
                None => return Err(ConnectionError::Closed),
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

    fn envelope(&mut self, command: CommandId) -> Envelope {
        Envelope {
            version: ENVELOPE_VERSION,
            client: self.client.clone(),
            command,
            ack: self.acked.take(),
        }
    }

    fn next_command_id(&mut self) -> CommandId {
        let id = self.next_command;
        self.next_command += 1;
        format!("{}-{id}", self.client)
    }

    fn next_request_id(&mut self) -> RequestId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

async fn dial(
    path: &Path,
    timeout: Duration,
) -> Result<(Link, ConnectionState, String, Cursor), ConnectError> {
    let stream = tokio::time::timeout(timeout, UnixStream::connect(path))
        .await
        .map_err(|_| ConnectError::TimedOut(timeout))??;
    let (reader, writer) = stream.into_split();
    let (tx, frames) = mpsc::channel(FRAME_BUFFER);
    let mut link = Link {
        writer,
        frames,
        reader: tokio::spawn(read_frames(reader, tx)),
    };
    let hello = tokio::time::timeout(timeout, link.frames.recv())
        .await
        .map_err(|_| ConnectError::TimedOut(timeout))?;
    let (state, build, cursor) = match hello {
        Some(Ok(Frame::Hello {
            state,
            build,
            cursor,
        })) => (state, build, cursor),
        Some(Ok(_)) => return Err(ConnectError::BadHello),
        Some(Err(error)) => return Err(ConnectError::Codec(error)),
        None => return Err(ConnectError::NoHello),
    };
    Ok((link, state, build, cursor))
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
        let command_id = self.next_command_id();
        let envelope = self.envelope(command_id.clone());
        let generation = self.cursor.generation.clone();
        let request = Request::Command {
            id,
            command: command.clone(),
            envelope,
        };
        let mut reply = self.request(request).await;
        if matches!(reply, Err(ConnectionError::Reconnected))
            && self.cursor.generation == generation
        {
            let id = self.next_request_id();
            let envelope = self.envelope(command_id.clone());
            reply = self
                .request(Request::Command {
                    id,
                    command,
                    envelope,
                })
                .await;
        }
        self.acked = Some(command_id);
        match reply? {
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
            let frame = match self.link.frames.recv().await {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => {
                    tracing::warn!(%error, "dropping a malformed frame");
                    continue;
                }
                None => {
                    if self.reconnect().await {
                        continue;
                    }
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
            let frame = match self.link.frames.try_recv().ok()? {
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
        match self.request(Request::History { id, limit: None }).await {
            Ok(Reply::History { messages, .. }) => messages,
            _ => Vec::new(),
        }
    }

    async fn recent_history(&mut self, limit: usize) -> mush_session::tree::RecentHistory {
        let id = self.next_request_id();
        let request = Request::History {
            id,
            limit: Some(limit),
        };
        match self.request(request).await {
            Ok(Reply::History {
                messages,
                recent: Some(recent),
            }) => mush_session::tree::RecentHistory {
                messages,
                omitted: recent.omitted,
                usage: recent.usage,
                cost: recent.cost,
            },
            Ok(Reply::History {
                messages,
                recent: None,
            }) => {
                let (usage, cost) =
                    mush_ai::models::conversation_spend(messages.iter().filter_map(|message| {
                        match message {
                            Message::Assistant(assistant) => Some(assistant),
                            _ => None,
                        }
                    }));
                mush_session::tree::RecentHistory {
                    messages,
                    omitted: 0,
                    usage,
                    cost,
                }
            }
            _ => mush_session::tree::RecentHistory::default(),
        }
    }

    async fn tree_summary(&mut self) -> Vec<TreeSummaryRow> {
        let id = self.next_request_id();
        match self.request(Request::TreeSummary { id }).await {
            Ok(Reply::TreeSummary { rows }) => rows,
            _ => Vec::new(),
        }
    }

    async fn navigation_impact(&mut self, entry_id: &mush_session::tree::EntryId) -> usize {
        let id = self.next_request_id();
        let request = Request::NavigationImpact {
            id,
            entry_id: entry_id.clone(),
        };
        match self.request(request).await {
            Ok(Reply::NavigationImpact { abandons }) => abandons,
            _ => 0,
        }
    }

    async fn system_prompt(&mut self) -> Option<String> {
        let id = self.next_request_id();
        match self.request(Request::SystemPrompt { id }).await {
            Ok(Reply::SystemPrompt { prompt }) => prompt,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_connect_that_times_out_releases_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hung.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await.unwrap().0 });

        let error = DaemonConnection::connect_within(&path, Duration::from_millis(100))
            .await
            .err()
            .unwrap();
        assert!(matches!(error, ConnectError::TimedOut(_)), "{error}");

        let mut server = accepted.await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let wrote = server.write_all(b"{}\n").await;
        assert!(wrote.is_err(), "the client still holds the read half");
    }
}
