//! UDS listener: accepts connections and handles IPC messages

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::card::AgentCard;

use super::{IpcMessage, IpcMessageKind};

/// handle for a running IPC listener
pub struct IpcListener {
    /// socket path (cleaned up on drop)
    path: PathBuf,
    /// background task handle
    _task: tokio::task::JoinHandle<()>,
}

impl IpcListener {
    /// start listening on the given socket path.
    /// responds to GetCard requests with the provided card.
    pub fn start(path: PathBuf, card: Arc<AgentCard>) -> std::io::Result<Self> {
        // ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // remove stale socket
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path)?;
        tracing::info!(path = %path.display(), "IPC listener started");

        let task_path = path.clone();
        let task = tokio::spawn(async move {
            accept_loop(listener, card, &task_path).await;
        });

        Ok(Self { path, _task: task })
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        tracing::debug!(path = %self.path.display(), "IPC socket removed");
    }
}

async fn accept_loop(listener: UnixListener, card: Arc<AgentCard>, path: &std::path::Path) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let card = card.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &card).await {
                        tracing::debug!("IPC connection error: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "IPC accept error: {e}");
                break;
            }
        }
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    card: &AgentCard,
) -> Result<(), Box<dyn std::error::Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let msg: IpcMessage = serde_json::from_str(&line)?;

        let kind = match msg.kind {
            IpcMessageKind::GetCard => IpcMessageKind::Card(card.clone()),
            _ => IpcMessageKind::Ack { message_id: msg.id.clone() },
        };
        let response = IpcMessage { id: msg.id, from: "mush".into(), kind };

        let mut json = serde_json::to_string(&response)?;
        json.push('\n');
        writer.write_all(json.as_bytes()).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistry;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn listener_responds_to_get_card() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");

        let card = Arc::new(AgentCard::build("test-model", &ToolRegistry::new()));
        let _listener = IpcListener::start(sock.clone(), card.clone()).unwrap();

        // connect and send a GetCard request (listener is already bound)
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
        // socket should be removed
        assert!(!sock.exists());
    }

    #[tokio::test]
    async fn listener_removes_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stale.sock");

        // create a stale file
        std::fs::write(&sock, "stale").unwrap();

        let card = Arc::new(AgentCard::build("test", &ToolRegistry::new()));
        let _listener = IpcListener::start(sock.clone(), card).unwrap();
        // should succeed despite stale file
    }
}
