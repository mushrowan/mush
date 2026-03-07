//! inter-pane messaging for multi-agent coordination
//!
//! provides a message bus for agents to communicate with siblings,
//! plus a `send_message` tool that agents can invoke.
//!
//! messages use typed envelopes with intent + optional task_id so agents
//! can categorise and route communication effectively

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use mush_agent::tool::{AgentTool, ToolResult};
use mush_ai::types::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::pane::PaneId;

/// why this message is being sent
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageIntent {
    /// sharing information or findings
    #[default]
    Info,
    /// requesting help or input from a sibling
    Request,
    /// responding to a previous request
    Response,
    /// coordinating work allocation
    Coordinate,
    /// reporting that a task is done
    Complete,
    /// reporting an error or blocker
    Error,
}

impl MessageIntent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Request => "request",
            Self::Response => "response",
            Self::Coordinate => "coordinate",
            Self::Complete => "complete",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for MessageIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// typed message envelope for inter-pane communication
#[derive(Debug, Clone)]
pub struct InterPaneMessage {
    pub from: PaneId,
    /// explicit recipient (None = broadcast)
    pub to: Option<PaneId>,
    pub intent: MessageIntent,
    pub content: String,
    /// optional task identifier for grouping related messages
    pub task_id: Option<String>,
    pub timestamp: Timestamp,
}

/// routes messages between panes via per-pane channels
#[derive(Clone)]
pub struct MessageBus {
    senders: Arc<Mutex<HashMap<PaneId, mpsc::UnboundedSender<InterPaneMessage>>>>,
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageBus {
    pub fn new() -> Self {
        Self {
            senders: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// register a pane, returns its inbox receiver
    pub fn register(&self, pane_id: PaneId) -> mpsc::UnboundedReceiver<InterPaneMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().unwrap().insert(pane_id, tx);
        rx
    }

    /// remove a pane's channel (on close)
    pub fn unregister(&self, pane_id: PaneId) {
        self.senders.lock().unwrap().remove(&pane_id);
    }

    /// send a message to a specific pane
    pub fn send(&self, to: PaneId, msg: InterPaneMessage) -> Result<(), String> {
        let senders = self.senders.lock().unwrap();
        match senders.get(&to) {
            Some(tx) => tx.send(msg).map_err(|_| "pane channel closed".into()),
            None => Err(format!("pane {} not found", to)),
        }
    }

    /// send a message to all panes except the sender
    pub fn broadcast(&self, from: PaneId, content: String) -> usize {
        let senders = self.senders.lock().unwrap();
        let timestamp = Timestamp::now();
        let mut sent = 0;
        for (&id, tx) in senders.iter() {
            if id != from {
                let msg = InterPaneMessage {
                    from,
                    to: Some(id),
                    intent: MessageIntent::Info,
                    content: content.clone(),
                    task_id: None,
                    timestamp,
                };
                if tx.send(msg).is_ok() {
                    sent += 1;
                }
            }
        }
        sent
    }

    /// list registered pane ids
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.senders.lock().unwrap().keys().copied().collect()
    }
}

/// tool that lets an agent send messages to sibling panes
pub struct SendMessageTool {
    pub sender_id: PaneId,
    pub bus: MessageBus,
}

impl AgentTool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn label(&self) -> &str {
        "Send Message"
    }

    fn description(&self) -> &str {
        "send a structured message to a sibling agent pane. use this to share \
         findings, coordinate work, request help, or report completion. \
         set intent to categorise the message (info/request/response/coordinate/complete/error)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "recipient_pane": {
                    "type": "integer",
                    "description": "pane number to send to (1-indexed)"
                },
                "message": {
                    "type": "string",
                    "description": "message to send"
                },
                "intent": {
                    "type": "string",
                    "enum": ["info", "request", "response", "coordinate", "complete", "error"],
                    "description": "purpose of this message (default: info)"
                },
                "task_id": {
                    "type": "string",
                    "description": "optional task identifier to group related messages"
                }
            },
            "required": ["recipient_pane", "message"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let recipient = match args.get("recipient_pane").and_then(|v| v.as_u64()) {
                Some(n) => n as u32,
                None => return ToolResult::error("recipient_pane is required (integer)"),
            };
            let message = match args.get("message").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return ToolResult::error("message is required (string)"),
            };

            let intent: MessageIntent = args
                .get("intent")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s.into())).ok())
                .unwrap_or_default();

            let task_id = args
                .get("task_id")
                .and_then(|v| v.as_str())
                .map(String::from);

            let target = PaneId::new(recipient);
            if target == self.sender_id {
                return ToolResult::error("cannot send a message to yourself");
            }

            let msg = InterPaneMessage {
                from: self.sender_id,
                to: Some(target),
                intent,
                content: message.to_string(),
                task_id: task_id.clone(),
                timestamp: Timestamp::now(),
            };

            match self.bus.send(target, msg) {
                Ok(()) => {
                    let mut reply = format!("sent {intent} to pane {recipient}");
                    if let Some(tid) = &task_id {
                        reply.push_str(&format!(" (task: {tid})"));
                    }
                    ToolResult::text(reply)
                }
                Err(e) => ToolResult::error(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_bus_register_and_send() {
        let bus = MessageBus::new();
        let mut rx = bus.register(PaneId::new(1));
        bus.register(PaneId::new(2));

        let msg = InterPaneMessage {
            from: PaneId::new(2),
            to: Some(PaneId::new(1)),
            intent: MessageIntent::Info,
            content: "hello from pane 2".into(),
            task_id: None,
            timestamp: Timestamp::now(),
        };
        bus.send(PaneId::new(1), msg).unwrap();

        let received = rx.try_recv().unwrap();
        assert_eq!(received.content, "hello from pane 2");
        assert_eq!(received.from, PaneId::new(2));
        assert_eq!(received.intent, MessageIntent::Info);
    }

    #[test]
    fn message_bus_send_to_nonexistent_pane() {
        let bus = MessageBus::new();
        let msg = InterPaneMessage {
            from: PaneId::new(1),
            to: Some(PaneId::new(99)),
            intent: MessageIntent::Info,
            content: "hello".into(),
            task_id: None,
            timestamp: Timestamp::now(),
        };
        assert!(bus.send(PaneId::new(99), msg).is_err());
    }

    #[test]
    fn message_bus_broadcast() {
        let bus = MessageBus::new();
        let mut rx1 = bus.register(PaneId::new(1));
        let mut rx2 = bus.register(PaneId::new(2));
        bus.register(PaneId::new(3)); // sender

        let sent = bus.broadcast(PaneId::new(3), "broadcast msg".into());
        assert_eq!(sent, 2);

        assert_eq!(rx1.try_recv().unwrap().content, "broadcast msg");
        assert_eq!(rx2.try_recv().unwrap().content, "broadcast msg");
    }

    #[test]
    fn message_bus_broadcast_excludes_sender() {
        let bus = MessageBus::new();
        let mut rx1 = bus.register(PaneId::new(1));

        let sent = bus.broadcast(PaneId::new(1), "self msg".into());
        assert_eq!(sent, 0);
        assert!(rx1.try_recv().is_err());
    }

    #[test]
    fn message_bus_unregister() {
        let bus = MessageBus::new();
        let _rx = bus.register(PaneId::new(1));
        assert_eq!(bus.pane_ids().len(), 1);

        bus.unregister(PaneId::new(1));
        assert!(bus.pane_ids().is_empty());
    }

    #[tokio::test]
    async fn send_message_tool_works() {
        let bus = MessageBus::new();
        bus.register(PaneId::new(1));
        let mut rx2 = bus.register(PaneId::new(2));

        let tool = SendMessageTool {
            sender_id: PaneId::new(1),
            bus: bus.clone(),
        };

        let result = tool
            .execute(serde_json::json!({
                "recipient_pane": 2,
                "message": "check main.rs"
            }))
            .await;
        assert!(result.outcome.is_success());

        let received = rx2.try_recv().unwrap();
        assert_eq!(received.content, "check main.rs");
        assert_eq!(received.from, PaneId::new(1));
        assert_eq!(received.intent, MessageIntent::Info); // default
        assert!(received.task_id.is_none());
    }

    #[tokio::test]
    async fn send_message_tool_with_intent_and_task() {
        let bus = MessageBus::new();
        bus.register(PaneId::new(1));
        let mut rx2 = bus.register(PaneId::new(2));

        let tool = SendMessageTool {
            sender_id: PaneId::new(1),
            bus: bus.clone(),
        };

        let result = tool
            .execute(serde_json::json!({
                "recipient_pane": 2,
                "message": "can you review the error handling?",
                "intent": "request",
                "task_id": "refactor-errors"
            }))
            .await;
        assert!(result.outcome.is_success());

        let received = rx2.try_recv().unwrap();
        assert_eq!(received.intent, MessageIntent::Request);
        assert_eq!(received.task_id.as_deref(), Some("refactor-errors"));
    }

    #[tokio::test]
    async fn send_message_tool_rejects_self() {
        let bus = MessageBus::new();
        bus.register(PaneId::new(1));

        let tool = SendMessageTool {
            sender_id: PaneId::new(1),
            bus: bus.clone(),
        };

        let result = tool
            .execute(serde_json::json!({
                "recipient_pane": 1,
                "message": "self"
            }))
            .await;
        assert!(result.outcome.is_error());
    }

    #[tokio::test]
    async fn send_message_tool_invalid_args() {
        let bus = MessageBus::new();

        let tool = SendMessageTool {
            sender_id: PaneId::new(1),
            bus: bus.clone(),
        };

        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.outcome.is_error());
    }
}
