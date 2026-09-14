use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;

use crate::card::AgentCard;

use super::control::{Control, ControlError, Reply};
use super::{IpcMessage, IpcMessageKind};

pub const MAX_REQUEST_BYTES: usize = 16_384;
pub const MAX_RESPONSE_BYTES: usize = 131_072;
pub const MAX_CONNECTIONS: usize = 16;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = self.path.symlink_metadata()
            && meta.file_type().is_socket()
            && meta.uid() == owner_uid()
            && meta.dev() == self.device
            && meta.ino() == self.inode
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct IpcListener {
    socket: SocketGuard,
    task: Option<tokio::task::JoinHandle<()>>,
    control: Option<Control>,
    cancel: tokio_util::sync::CancellationToken,
}

impl IpcListener {
    pub fn start(path: PathBuf, card: Arc<AgentCard>) -> std::io::Result<Self> {
        Self::start_inner(path, card, None)
    }

    pub fn start_control(
        path: PathBuf,
        card: Arc<AgentCard>,
        control: Control,
    ) -> io::Result<Self> {
        Self::start_inner(path, card, Some(control))
    }

    fn start_inner(
        path: PathBuf,
        card: Arc<AgentCard>,
        control: Option<Control>,
    ) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("socket has no parent"))?;
        ensure_directory(parent)?;
        validate_parent(parent)?;
        if path.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "socket path already exists",
            ));
        }
        let listener = UnixListener::bind(&path)?;
        let meta = path.symlink_metadata()?;
        let socket = SocketGuard {
            path,
            device: meta.dev(),
            inode: meta.ino(),
        };
        std::fs::set_permissions(&socket.path, std::fs::Permissions::from_mode(0o600))?;
        let shared = control.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            accept_loop(listener, card, shared, shutdown).await;
        });
        Ok(Self {
            socket,
            task: Some(task),
            control,
            cancel,
        })
    }

    pub fn path(&self) -> &Path {
        &self.socket.path
    }

    pub async fn shutdown(mut self) -> Result<(), tokio::task::JoinError> {
        if let Some(control) = &self.control {
            control.shutdown();
        }
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.await?;
        }
        Ok(())
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        if let Some(control) = &self.control {
            control.shutdown();
        }
        self.cancel.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub fn owner_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn authorise_peer(stream: &UnixStream, owner: u32) -> io::Result<()> {
    if stream.peer_cred()?.uid() != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            ControlError::Unauthorised,
        ));
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    if path.symlink_metadata().is_ok() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid runtime directory"))?;
    ensure_directory(parent)?;
    validate_ancestors(parent)?;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        result => result,
    }
}

fn validate_ancestors(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(io::Error::other("socket path must be absolute"));
    }
    for ancestor in path.ancestors() {
        let meta = ancestor.symlink_metadata()?;
        let trusted_owner = meta.uid() == 0 || meta.uid() == owner_uid();
        let protected = meta.mode() & 0o022 == 0 || (meta.uid() == 0 && meta.mode() & 0o1000 != 0);
        if !meta.is_dir() || !trusted_owner || !protected {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe socket ancestor",
            ));
        }
    }
    Ok(())
}

fn validate_parent(path: &Path) -> io::Result<()> {
    validate_ancestors(path)?;
    let meta = path.symlink_metadata()?;
    if meta.uid() != owner_uid() || meta.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory must be owned by this user with mode 0700",
        ));
    }
    Ok(())
}

pub fn validate_socket(path: &Path) -> io::Result<()> {
    validate_parent(
        path.parent()
            .ok_or_else(|| io::Error::other("socket has no parent"))?,
    )?;
    let meta = path.symlink_metadata()?;
    if !meta.file_type().is_socket() || meta.uid() != owner_uid() || meta.mode() & 0o777 != 0o600 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe control socket",
        ));
    }
    Ok(())
}

async fn accept_loop(
    listener: UnixListener,
    card: Arc<AgentCard>,
    control: Option<Control>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut clients = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => { if let Some(control) = &control { control.tick(); } }
            _ = clients.join_next(), if !clients.is_empty() => {}
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                if authorise_peer(&stream, owner_uid()).is_err() {
                    reject(stream, ControlError::Unauthorised);
                    continue;
                }
                if clients.len() >= MAX_CONNECTIONS {
                    reject(stream, ControlError::ConnectionLimit);
                    continue;
                }
                let card = Arc::clone(&card);
                let control = control.clone();
                clients.spawn(async move {
                    if let Err(error) = handle_connection(stream, &card, control.as_ref()).await {
                        tracing::debug!(%error, "ipc connection closed");
                    }
                });
            }
        }
    }
    clients.shutdown().await;
    if let Some(control) = control {
        control.shutdown();
    }
}

fn error_response(id: String, code: ControlError) -> IpcMessage {
    IpcMessage {
        id,
        from: "mush".into(),
        kind: IpcMessageKind::ControlReply(Reply::Error { code }),
    }
}

fn reject(stream: UnixStream, code: ControlError) {
    if let Ok(mut bytes) = serde_json::to_vec(&error_response(String::new(), code)) {
        bytes.push(b'\n');
        if let Ok(mut stream) = stream.into_std() {
            let _ = std::io::Write::write(&mut stream, &bytes);
        }
    }
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, ControlError> {
    let mut bytes = Vec::new();
    let read = tokio::time::timeout(
        REQUEST_TIMEOUT,
        reader
            .take((limit + 1) as u64)
            .read_until(b'\n', &mut bytes),
    )
    .await;
    match read {
        Err(_) => Err(ControlError::Timeout),
        Ok(Err(_)) => Err(ControlError::InvalidPayload),
        Ok(Ok(0)) => Ok(None),
        Ok(Ok(_)) if bytes.len() > limit => Err(ControlError::TooLarge),
        Ok(Ok(_)) if bytes.last() != Some(&b'\n') => Err(ControlError::InvalidPayload),
        Ok(Ok(_)) => Ok(Some(bytes)),
    }
}

async fn handle_connection(
    stream: UnixStream,
    card: &AgentCard,
    control: Option<&Control>,
) -> io::Result<()> {
    let uid = stream.peer_cred()?.uid();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    for _ in 0..32 {
        let (response, close) = match read_frame(&mut reader, MAX_REQUEST_BYTES).await {
            Ok(None) => break,
            Err(code) => (error_response(String::new(), code), true),
            Ok(Some(bytes)) => (dispatch(&bytes, card, control, uid), false),
        };
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        tokio::time::timeout(REQUEST_TIMEOUT, writer.write_all(&bytes)).await??;
        if close {
            break;
        }
    }
    Ok(())
}

fn dispatch(bytes: &[u8], card: &AgentCard, control: Option<&Control>, uid: u32) -> IpcMessage {
    let msg: IpcMessage = match serde_json::from_slice(bytes) {
        Ok(message) => message,
        Err(_) => {
            let value = serde_json::from_slice::<serde_json::Value>(bytes).ok();
            let id = value
                .as_ref()
                .and_then(|v| v["id"].as_str())
                .filter(|s| super::control::valid_id(s))
                .unwrap_or_default()
                .to_owned();
            let unknown = value.as_ref().is_some_and(|v| {
                v["kind"]["type"].as_str().is_some_and(|kind| {
                    !matches!(
                        kind,
                        "GetCard"
                            | "Card"
                            | "UserMessage"
                            | "Ack"
                            | "Error"
                            | "GetTarget"
                            | "Target"
                            | "Control"
                            | "ControlReply"
                    )
                }) || v["kind"]["payload"]["operation"]["op"]
                    .as_str()
                    .is_some_and(|op| !matches!(op, "send" | "schedule" | "cancel" | "status"))
            });
            return error_response(
                id,
                if unknown {
                    ControlError::UnknownRequest
                } else {
                    ControlError::InvalidPayload
                },
            );
        }
    };
    if !super::control::valid_id(&msg.id) || !super::control::valid_id(&msg.from) {
        return error_response(String::new(), ControlError::InvalidPayload);
    }
    let kind = match msg.kind {
        IpcMessageKind::GetCard => IpcMessageKind::Card(card.clone()),
        IpcMessageKind::GetTarget => match control {
            Some(control) => IpcMessageKind::Target(control.target()),
            None => return error_response(msg.id, ControlError::Unavailable),
        },
        IpcMessageKind::Control(request) => match control {
            Some(control) => {
                IpcMessageKind::ControlReply(control.request(&msg.id, &msg.from, uid, request))
            }
            None => return error_response(msg.id, ControlError::Unavailable),
        },
        IpcMessageKind::UserMessage { text, target } => match (control, target) {
            (Some(control), Some(target)) => IpcMessageKind::ControlReply(control.request(
                &msg.id,
                &msg.from,
                uid,
                super::control::Request {
                    target,
                    operation: super::control::Operation::Send {
                        event: super::control::Event::Message { text },
                    },
                },
            )),
            (Some(_), None) => return error_response(msg.id, ControlError::InvalidPayload),
            (None, _) => return error_response(msg.id, ControlError::Unavailable),
        },
        IpcMessageKind::Card(_)
        | IpcMessageKind::Ack { .. }
        | IpcMessageKind::Error { .. }
        | IpcMessageKind::Target(_)
        | IpcMessageKind::ControlReply(_) => {
            return error_response(msg.id, ControlError::UnknownRequest);
        }
    };
    IpcMessage {
        id: msg.id,
        from: "mush".into(),
        kind,
    }
}

pub async fn exchange(path: &Path, request: &IpcMessage) -> io::Result<IpcMessage> {
    validate_socket(path)?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(io::Error::other(ControlError::TooLarge));
    }
    let mut stream = tokio::time::timeout(REQUEST_TIMEOUT, UnixStream::connect(path)).await??;
    authorise_peer(&stream, owner_uid())?;
    tokio::time::timeout(REQUEST_TIMEOUT, stream.write_all(&bytes)).await??;
    let response = read_frame(&mut BufReader::new(stream), MAX_RESPONSE_BYTES)
        .await
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("control endpoint closed"))?;
    let response: IpcMessage = serde_json::from_slice(&response)?;
    if response.id != request.id && !response.id.is_empty() {
        return Err(io::Error::other("response id mismatch"));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistry;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn peer_credentials_reject_a_different_owner() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        assert!(authorise_peer(&stream, owner_uid()).is_ok());
        assert_eq!(
            authorise_peer(&stream, owner_uid().wrapping_add(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn listener_responds_to_get_card() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");

        let card = Arc::new(AgentCard::build("test-model", &ToolRegistry::new()));
        let _listener = IpcListener::start(sock.clone(), card.clone()).unwrap();

        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = stream.into_split();

        let request = IpcMessage {
            id: "req-1".into(),
            from: "test-client".into(),
            kind: IpcMessageKind::GetCard,
        };
        let mut json = serde_json::to_string(&request).unwrap();
        json.push('\n');
        writer.write_all(json.as_bytes()).await.unwrap();

        let mut lines = BufReader::new(reader).lines();
        let response_line = lines.next_line().await.unwrap().unwrap();
        let response: IpcMessage = serde_json::from_str(&response_line).unwrap();

        assert_eq!(response.id, "req-1");
        assert_eq!(response.from, "mush");
        match response.kind {
            IpcMessageKind::Card(c) => {
                assert_eq!(c.model, "test-model");
            }
            other => panic!("expected Card, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn listener_cleans_up_socket_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("cleanup.sock");

        let card = Arc::new(AgentCard::build("test", &ToolRegistry::new()));
        let listener = IpcListener::start(sock.clone(), card).unwrap();
        assert!(sock.exists());

        drop(listener);
        assert!(!sock.exists());
    }

    #[tokio::test]
    async fn listener_preserves_existing_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");

        std::fs::write(&sock, "stale").unwrap();

        let card = Arc::new(AgentCard::build("test", &ToolRegistry::new()));
        assert!(IpcListener::start(sock.clone(), card).is_err());
        assert_eq!(std::fs::read_to_string(sock).unwrap(), "stale");
    }
}
