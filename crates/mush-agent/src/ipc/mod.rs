pub mod listener;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::card::AgentCard;

pub use listener::IpcListener;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcMessage {
    pub id: String,
    pub from: String,
    pub kind: IpcMessageKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload")]
pub enum IpcMessageKind {
    GetCard,
    Card(AgentCard),
    UserMessage { text: String },
    Ack { message_id: String },
    Error { message: String },
}

pub fn socket_path(session_id: &str) -> PathBuf {
    mush_runtime_dir().join(format!("{session_id}.sock"))
}

pub fn discover_sockets() -> Vec<PathBuf> {
    let dir = mush_runtime_dir();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "sock"))
        .collect()
}

fn mush_runtime_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
        format!("/tmp/mush-{user}")
    });
    PathBuf::from(base).join("mush")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_uses_session_id() {
        let path = socket_path("abc123");
        assert!(path.to_str().unwrap().contains("abc123.sock"));
        assert!(path.to_str().unwrap().contains("mush"));
    }

    #[test]
    fn runtime_dir_ends_with_mush() {
        let dir = mush_runtime_dir();
        assert_eq!(dir.file_name().unwrap(), "mush");
    }

    #[test]
    fn ipc_message_roundtrips() {
        let msg = IpcMessage {
            id: "msg-1".into(),
            from: "agent-a".into(),
            kind: IpcMessageKind::GetCard,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: IpcMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn ipc_card_response_roundtrips() {
        let card = AgentCard::build("test", &crate::tool::ToolRegistry::new());
        let msg = IpcMessage {
            id: "msg-2".into(),
            from: "agent-b".into(),
            kind: IpcMessageKind::Card(card.clone()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: IpcMessage = serde_json::from_str(&json).unwrap();
        if let IpcMessageKind::Card(c) = &parsed.kind {
            assert_eq!(c, &card);
        } else {
            panic!("expected Card variant");
        }
    }

    #[test]
    fn ipc_error_roundtrips() {
        let msg = IpcMessage {
            id: "msg-3".into(),
            from: "agent-c".into(),
            kind: IpcMessageKind::Error {
                message: "not found".into(),
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: IpcMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }
}
