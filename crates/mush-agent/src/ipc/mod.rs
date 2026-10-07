pub mod control;
#[cfg(test)]
mod control_tests;
pub mod listener;
#[cfg(test)]
mod listener_tests;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::card::AgentCard;

pub use listener::IpcListener;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IpcMessage {
    pub id: String,
    pub from: String,
    pub kind: IpcMessageKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", deny_unknown_fields)]
pub enum IpcMessageKind {
    GetCard,
    Card(AgentCard),
    UserMessage {
        text: String,
        #[serde(default)]
        target: Option<control::Target>,
    },
    Ack {
        message_id: String,
    },
    Error {
        message: String,
    },
    GetTarget,
    Target(control::Target),
    Control(control::Request),
    ControlReply(control::Reply),
}

pub fn socket_path(session_id: &str) -> PathBuf {
    runtime_dir().join(format!("{session_id}.sock"))
}

pub fn control_socket_path(instance: &str) -> PathBuf {
    runtime_dir()
        .join("control")
        .join(format!("{instance}.sock"))
}

pub fn runtime_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        let user = rustix::process::geteuid().as_raw();
        format!("/tmp/mush-{user}")
    });
    resolve_existing_prefix(&PathBuf::from(base)).join("mush")
}

fn resolve_existing_prefix(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(real) = std::fs::canonicalize(ancestor) {
            let rest = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
            return real.join(rest);
        }
    }
    path.to_path_buf()
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
    fn existing_prefix_resolves_symlinks_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let resolved = resolve_existing_prefix(&link.join("missing/child"));

        assert_eq!(
            resolved,
            std::fs::canonicalize(&real).unwrap().join("missing/child")
        );
    }

    #[test]
    fn runtime_dir_ends_with_mush() {
        let dir = runtime_dir();
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
