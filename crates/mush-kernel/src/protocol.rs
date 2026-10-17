use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Execute {
        id: String,
        code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    Interrupt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    HostReply {
        id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Snapshot {
        id: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_variable_bytes: Option<u64>,
    },
    Restore {
        id: String,
        path: String,
    },
    ListNames {
        id: String,
    },
    Wake,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready {
        protocol: u32,
        python: String,
    },
    Stdout {
        id: Option<String>,
        text: String,
    },
    Stderr {
        id: Option<String>,
        text: String,
    },
    Result {
        id: String,
        text: String,
    },
    Display {
        id: Option<String>,
        data: serde_json::Map<String, Value>,
    },
    HostRequest {
        id: String,
        cell: Option<String>,
        kind: String,
        payload: Value,
    },
    Error {
        id: Option<String>,
        ename: String,
        evalue: String,
        #[serde(default)]
        traceback: Vec<String>,
    },
    Done {
        id: String,
        ok: bool,
        #[serde(flatten)]
        extra: serde_json::Map<String, Value>,
    },
    HostCancel {
        id: String,
    },
    Wait {
        id: Option<String>,
        on: Option<String>,
    },
}

impl Event {
    #[must_use]
    pub fn cell_id(&self) -> Option<&str> {
        match self {
            Self::Ready { .. } | Self::HostCancel { .. } => None,
            Self::Stdout { id, .. }
            | Self::Stderr { id, .. }
            | Self::Error { id, .. }
            | Self::Display { id, .. }
            | Self::Wait { id, .. } => id.as_deref(),
            Self::Result { id, .. } | Self::Done { id, .. } => Some(id),
            Self::HostRequest { cell, .. } => cell.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialise_with_a_type_tag() {
        let json = serde_json::to_string(&Request::Execute {
            id: "c1".into(),
            code: "x = 1".into(),
            origin: None,
        })
        .unwrap();
        assert_eq!(json, r#"{"type":"execute","id":"c1","code":"x = 1"}"#);
        let json = serde_json::to_string(&Request::Execute {
            id: "c1".into(),
            code: "x = 1".into(),
            origin: Some("call_7".into()),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"execute","id":"c1","code":"x = 1","origin":"call_7"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Interrupt { id: None }).unwrap(),
            r#"{"type":"interrupt"}"#
        );
    }

    #[test]
    fn a_host_reply_omits_the_field_it_does_not_carry() {
        let json = serde_json::to_string(&Request::HostReply {
            id: "r".into(),
            ok: false,
            value: None,
            error: Some("nope".into()),
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"host_reply","id":"r","ok":false,"error":"nope"}"#
        );
    }

    #[test]
    fn done_keeps_its_extras() {
        let event: Event =
            serde_json::from_str(r#"{"type":"done","id":"n","ok":true,"names":["x"]}"#).unwrap();
        match event {
            Event::Done { id, ok, extra } => {
                assert_eq!(id, "n");
                assert!(ok);
                assert_eq!(extra["names"], serde_json::json!(["x"]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn raw_fd_output_has_no_cell() {
        let event: Event =
            serde_json::from_str(r#"{"type":"stdout","id":null,"text":"raw\n"}"#).unwrap();
        assert_eq!(event.cell_id(), None);
    }
}
