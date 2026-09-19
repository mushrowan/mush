use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use mush_runtime::{
    AgentConnection, Applied, ClientCommand, ClientEvent, ConnectionError, ConnectionState,
    InProcessConnection,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::protocol::{ClientId, CommandId, Cursor, Envelope, Frame, Reply, Request, encode};
use crate::supervisor::WorkerDescriptor;

const TICK: std::time::Duration = std::time::Duration::from_millis(50);
const CLIENT_FRAMES: usize = 512;
const REPLAY_HISTORY: usize = 2048;
const JOURNAL: usize = 256;

pub struct Descriptor {
    path: std::path::PathBuf,
    written: WorkerDescriptor,
}

impl Descriptor {
    #[must_use]
    pub fn new(
        path: std::path::PathBuf,
        socket: std::path::PathBuf,
        cwd: std::path::PathBuf,
    ) -> Self {
        let pid = std::process::id();
        let started_at = now_secs();
        Self {
            path,
            written: WorkerDescriptor {
                session_id: String::new(),
                model_id: String::new(),
                build: crate::build::build_id(),
                pid,
                start_id: mush_tools::orphans::process_start_id(pid),
                cwd,
                socket,
                started_at,
                title: None,
                turn_active: false,
                messages: 0,
                updated_at: started_at,
                clients: 0,
                failed: None,
            },
        }
    }

    fn refresh(&mut self, state: &ConnectionState, clients: usize) {
        let next = WorkerDescriptor {
            session_id: state.session_id.to_string(),
            model_id: state.model_id.clone(),
            title: state.title.clone(),
            turn_active: state.turn_active,
            messages: state.context_len,
            clients,
            ..self.written.clone()
        };
        if next == self.written && self.path.exists() {
            return;
        }
        self.written = WorkerDescriptor {
            updated_at: now_secs(),
            ..next
        };
        if let Ok(bytes) = serde_json::to_vec(&self.written) {
            let _ = mush_ai::private_io::write_private(&self.path, bytes);
        }
    }

    fn remove(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default)]
struct TurnLog {
    events: Vec<ClientEvent>,
}

impl TurnLog {
    fn record(&mut self, event: &ClientEvent) {
        match event {
            ClientEvent::TurnStarted => {
                self.events.clear();
                self.events.push(event.clone());
            }
            ClientEvent::TurnEnded { .. } => self.events.clear(),
            ClientEvent::Agent {
                applied: Applied::Appended | Applied::Compacted { .. },
                ..
            }
            | ClientEvent::Compacted(_) => self.events.truncate(1),
            _ if !self.events.is_empty() => self.events.push(event.clone()),
            _ => {}
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

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

type ClientKey = u64;

enum Incoming {
    Line { key: ClientKey, line: String },
    Gone { key: ClientKey },
}

struct Attachment {
    frames: mpsc::Sender<Frame>,
    identity: Option<ClientId>,
}

struct Hub {
    generation: String,
    sequence: u64,
    history: VecDeque<(u64, ClientEvent)>,
    clients: HashMap<ClientKey, Attachment>,
    next_key: ClientKey,
    last_state: ConnectionState,
    incoming: mpsc::Sender<Incoming>,
}

impl Hub {
    fn new(incoming: mpsc::Sender<Incoming>, state: ConnectionState) -> Self {
        Self {
            generation: mush_ai::types::SessionId::new().to_string(),
            sequence: 0,
            history: VecDeque::new(),
            clients: HashMap::new(),
            next_key: 0,
            last_state: state,
            incoming,
        }
    }

    fn cursor(&self) -> Cursor {
        Cursor {
            generation: self.generation.clone(),
            sequence: self.sequence,
        }
    }

    fn accept(&mut self, stream: UnixStream, state: ConnectionState) {
        let key = self.next_key;
        self.next_key += 1;
        let (reader, mut writer) = stream.into_split();
        let (frames, mut outgoing) = mpsc::channel(CLIENT_FRAMES);
        let gone = self.incoming.clone();
        tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                let Ok(bytes) = encode(&frame) else { continue };
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = gone.send(Incoming::Gone { key }).await;
        });
        let lines = self.incoming.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if lines.send(Incoming::Line { key, line }).await.is_err() {
                    return;
                }
            }
            let _ = lines.send(Incoming::Gone { key }).await;
        });
        self.clients.insert(
            key,
            Attachment {
                frames,
                identity: None,
            },
        );
        self.send(
            key,
            Frame::Hello {
                state,
                build: crate::build::build_id(),
                cursor: self.cursor(),
            },
        );
    }

    fn send(&mut self, key: ClientKey, frame: Frame) {
        let Some(client) = self.clients.get(&key) else {
            return;
        };
        if client.frames.try_send(frame).is_err() {
            tracing::warn!(key, "dropping a client that stopped reading");
            self.clients.remove(&key);
        }
    }

    fn broadcast(&mut self, frame: &Frame) {
        let keys: Vec<ClientKey> = self.clients.keys().copied().collect();
        for key in keys {
            self.send(key, frame.clone());
        }
    }

    fn broadcast_except(&mut self, skip: ClientKey, frame: &Frame) {
        let keys: Vec<ClientKey> = self
            .clients
            .keys()
            .copied()
            .filter(|key| *key != skip)
            .collect();
        for key in keys {
            self.send(key, frame.clone());
        }
    }

    fn record(&mut self, event: ClientEvent) -> u64 {
        self.sequence += 1;
        self.history.push_back((self.sequence, event));
        while self.history.len() > REPLAY_HISTORY {
            self.history.pop_front();
        }
        self.sequence
    }

    fn state_delta(&mut self, connection: &InProcessConnection) -> Option<ConnectionState> {
        let state = connection.state();
        if state == self.last_state {
            return None;
        }
        self.last_state = state.clone();
        Some(state)
    }

    fn replay(&mut self, key: ClientKey, after: u64) -> bool {
        let oldest = self
            .history
            .front()
            .map_or(self.sequence + 1, |(at, _)| *at);
        if after > self.sequence || (after + 1) < oldest {
            return false;
        }
        let missed: Vec<(u64, ClientEvent)> = self
            .history
            .iter()
            .filter(|(at, _)| *at > after)
            .cloned()
            .collect();
        for (sequence, event) in missed {
            self.send(
                key,
                Frame::Event {
                    event,
                    state: None,
                    sequence,
                },
            );
        }
        true
    }

    fn replay_turn(&mut self, key: ClientKey, log: &TurnLog) {
        for event in &log.events {
            self.send(
                key,
                Frame::Event {
                    event: event.clone(),
                    state: None,
                    sequence: self.sequence,
                },
            );
        }
    }
}

pub async fn serve(
    mut connection: InProcessConnection,
    listener: UnixListener,
    mut descriptor: Option<Descriptor>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let (incoming, mut received) = mpsc::channel(CLIENT_FRAMES);
    let mut worker = Worker {
        log: TurnLog::default(),
        hub: Hub::new(incoming, connection.state()),
        journal: Journal::default(),
        shutdown,
        shutting_down: false,
    };
    let served = worker
        .serve_until_closed(
            &mut connection,
            &listener,
            &mut received,
            descriptor.as_mut(),
        )
        .await;
    if let Some(descriptor) = &descriptor {
        descriptor.remove();
    }
    served
}

type Outcome = Result<Vec<ClientEvent>, ConnectionError>;

#[derive(Default)]
struct Journal {
    entries: HashMap<(ClientId, CommandId), Outcome>,
    order: VecDeque<(ClientId, CommandId)>,
}

impl Journal {
    fn stored(&self, key: &(ClientId, CommandId)) -> Option<Outcome> {
        self.entries.get(key).cloned()
    }

    fn record(&mut self, key: (ClientId, CommandId), result: Outcome) {
        self.order.push_back(key.clone());
        self.entries.insert(key, result);
        while self.order.len() > JOURNAL {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn ack(&mut self, key: &(ClientId, CommandId)) {
        self.entries.remove(key);
        self.order.retain(|held| held != key);
    }
}

struct Worker {
    log: TurnLog,
    hub: Hub,
    journal: Journal,
    shutdown: CancellationToken,
    shutting_down: bool,
}

impl Worker {
    async fn shut_down(&mut self, connection: &mut InProcessConnection) {
        self.shutting_down = true;
        let _ = connection.send(ClientCommand::Shutdown).await;
    }

    async fn serve_until_closed(
        &mut self,
        connection: &mut InProcessConnection,
        listener: &UnixListener,
        received: &mut mpsc::Receiver<Incoming>,
        mut descriptor: Option<&mut Descriptor>,
    ) -> io::Result<()> {
        loop {
            connection.start_pending().await;
            if let Some(descriptor) = descriptor.as_deref_mut() {
                descriptor.refresh(&connection.state(), self.hub.clients.len());
            }
            tokio::select! {
                accepted = listener.accept() => {
                    self.hub.accept(accepted?.0, connection.state());
                }
                Some(incoming) = received.recv() => match incoming {
                    Incoming::Gone { key } => { self.hub.clients.remove(&key); }
                    Incoming::Line { key, line } => self.serve_line(connection, key, &line).await,
                },
                event = connection.next_event() => {
                    let Some(event) = event else {
                        self.hub.broadcast(&Frame::Closed);
                        return Ok(());
                    };
                    self.log.record(&event);
                    let sequence = self.hub.record(event.clone());
                    let state = self.hub.state_delta(connection);
                    self.hub.broadcast(&Frame::Event { event, state, sequence });
                }
                () = self.shutdown.cancelled(), if !self.shutting_down => {
                    self.shut_down(connection).await;
                }
                () = tokio::time::sleep(TICK) => {}
            }
        }
    }

    async fn serve_line(
        &mut self,
        connection: &mut InProcessConnection,
        key: ClientKey,
        line: &str,
    ) {
        let request = match crate::protocol::decode::<Request>(line) {
            Ok(request) => request,
            Err(error) => {
                let id = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| value.get("id")?.as_u64());
                self.hub.send(
                    key,
                    Frame::Response {
                        id: id.unwrap_or(0),
                        result: Err(ConnectionError::Transport(format!(
                            "malformed request: {error}"
                        ))),
                        state: None,
                    },
                );
                return;
            }
        };
        let id = request.id();
        let result = match request {
            Request::Attach { client, from, .. } => {
                Ok(self.attach(connection, key, client, from).await)
            }
            Request::Command {
                command, envelope, ..
            } => self.dispatch(connection, command, envelope).await,
            other => handle(connection, other).await,
        };
        let state = self.hub.state_delta(connection);
        if let Ok(Reply::Events { events }) = &result {
            for event in events {
                self.log.record(event);
                let sequence = self.hub.record(event.clone());
                self.hub.broadcast_except(
                    key,
                    &Frame::Event {
                        event: event.clone(),
                        state: state.clone(),
                        sequence,
                    },
                );
            }
        }
        self.hub.send(key, Frame::Response { id, result, state });
    }

    async fn dispatch(
        &mut self,
        connection: &mut InProcessConnection,
        command: ClientCommand,
        envelope: Envelope,
    ) -> Result<Reply, ConnectionError> {
        if let Some(ack) = envelope.ack {
            self.journal.ack(&(envelope.client.clone(), ack));
        }
        let key = (envelope.client, envelope.command);
        if let Some(stored) = self.journal.stored(&key) {
            return stored.map(|events| Reply::Events { events });
        }
        let result = connection.send(command).await;
        self.journal.record(key, result.clone());
        result.map(|events| Reply::Events { events })
    }

    async fn attach(
        &mut self,
        connection: &mut InProcessConnection,
        key: ClientKey,
        client: ClientId,
        from: Option<Cursor>,
    ) -> Reply {
        if let Some(attachment) = self.hub.clients.get_mut(&key) {
            attachment.identity = Some(client);
        }
        let caught_up = match from {
            Some(cursor) if cursor.generation == self.hub.generation => {
                self.hub.replay(key, cursor.sequence)
            }
            Some(_) => false,
            None => {
                self.hub.replay_turn(key, &self.log);
                true
            }
        };
        if !caught_up {
            let event = ClientEvent::Session(mush_runtime::SessionEvent::TranscriptReplaced {
                reason: mush_runtime::Replaced::Resynced,
                messages: connection.transcript().await,
                editor_text: None,
            });
            let sequence = self.hub.sequence;
            self.hub.send(
                key,
                Frame::Event {
                    event,
                    state: None,
                    sequence,
                },
            );
        }
        Reply::Attached {
            cursor: self.hub.cursor(),
            snapshot: !caught_up,
        }
    }
}

async fn handle(
    connection: &mut InProcessConnection,
    request: Request,
) -> Result<Reply, ConnectionError> {
    match request {
        Request::Attach { .. } => Err(ConnectionError::Transport("already attached".into())),
        Request::Command { .. } => Err(ConnectionError::Transport("unjournalled command".into())),
        Request::Transcript { .. } => Ok(Reply::Transcript {
            messages: connection.transcript().await,
        }),
        Request::History { .. } => Ok(Reply::History {
            messages: connection.history().await,
        }),
        Request::TreeSummary { .. } => Ok(Reply::TreeSummary {
            rows: connection.tree_summary().await,
        }),
        Request::NavigationImpact { entry_id, .. } => Ok(Reply::NavigationImpact {
            abandons: connection.navigation_impact(&entry_id).await,
        }),
        Request::SystemPrompt { .. } => Ok(Reply::SystemPrompt {
            prompt: connection.system_prompt().await,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ConnectionState {
        ConnectionState {
            session_id: mush_ai::types::SessionId::new(),
            title: None,
            model_id: "m".into(),
            thinking_level: mush_ai::types::ThinkingLevel::Off,
            context_tokens: mush_ai::types::TokenCount::ZERO,
            context_len: 0,
            tree_len: 0,
            rlm_depth: 0,
            rlm_max_depth: 2,
            turn_active: false,
            turn_model: None,
            active_turns: 0,
            compacting: false,
            pending_prompt: false,
            controlled: false,
            last_request: None,
        }
    }

    fn hub_with_one_slot() -> (
        Hub,
        ClientKey,
        mpsc::Receiver<Frame>,
        mpsc::Receiver<Incoming>,
    ) {
        let (incoming, received) = mpsc::channel(8);
        let mut hub = Hub::new(incoming, state());
        let (frames, outgoing) = mpsc::channel(1);
        let key = hub.next_key;
        hub.next_key += 1;
        hub.clients.insert(
            key,
            Attachment {
                frames,
                identity: None,
            },
        );
        (hub, key, outgoing, received)
    }

    #[tokio::test]
    async fn a_client_that_fills_its_buffer_is_dropped() {
        let (mut hub, key, _outgoing, _received) = hub_with_one_slot();
        hub.broadcast(&Frame::Closed);
        assert_eq!(hub.clients.len(), 1);
        hub.broadcast(&Frame::Closed);
        assert!(hub.clients.is_empty(), "the stalled client should be gone");
        hub.send(key, Frame::Closed);
    }

    #[tokio::test]
    async fn replay_refuses_a_cursor_older_than_the_ring() {
        let (mut hub, key, _outgoing, _received) = hub_with_one_slot();
        for _ in 0..3 {
            hub.record(ClientEvent::TurnStarted);
        }
        hub.history.pop_front();
        assert!(!hub.replay(key, 0));
        assert!(hub.replay(key, 3));
    }
}
