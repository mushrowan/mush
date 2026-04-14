//! delegate_task tool: spawn a sub-agent in a new pane
//!
//! the calling agent forks a new pane with a task prompt.
//! the sub-agent runs independently and sends results back
//! via the messaging system.

use std::sync::{Arc, Mutex};

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use mush_ai::types::PaneId;
use serde::Deserialize;

/// pending delegation: pane fork + prompt injection
pub struct PendingDelegation {
    pub task: String,
    pub from: PaneId,
    pub task_id: String,
    /// model override for the sub-agent (tier name or model id)
    pub model: Option<String>,
}

/// shared queue of pending delegations (tool writes, TUI loop reads)
pub type DelegationQueue = Arc<Mutex<Vec<PendingDelegation>>>;

/// create a new empty delegation queue
pub fn new_queue() -> DelegationQueue {
    Arc::new(Mutex::new(Vec::new()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    /// the task to delegate (becomes the sub-agent's prompt)
    task: String,
    /// optional task id for tracking (auto-generated if not provided)
    task_id: Option<String>,
    /// optional model tier name (e.g. "fast") or model id for the sub-agent
    model: Option<String>,
}

/// tool that spawns a sub-agent pane to work on a task
pub struct DelegateTaskTool {
    pub sender_id: PaneId,
    pub queue: DelegationQueue,
}

#[async_trait::async_trait]
impl AgentTool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn label(&self) -> &str {
        "Delegate Task"
    }

    fn description(&self) -> &str {
        "spawn a new agent pane to work on a sub-task independently. \
         the sub-agent gets its own conversation and tools. when it finishes, \
         its result is sent back to you as a message. use this for parallelisable \
         work like reviewing a file while you edit another, running tests while \
         you continue coding, or any task that can proceed independently. \
         optionally specify a model tier (e.g. 'fast', 'strong') or model id \
         for the sub-agent"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "the task description (becomes the sub-agent's prompt)"
                },
                "task_id": {
                    "type": "string",
                    "description": "optional identifier for tracking this delegation"
                },
                "model": {
                    "type": "string",
                    "description": "model tier (e.g. 'fast', 'strong') or model id for the sub-agent"
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let params: DelegateArgs = match parse_tool_args(args) {
            Ok(p) => p,
            Err(e) => return e,
        };

        let task_id = params
            .task_id
            .unwrap_or_else(|| format!("del-{}", rand_id()));

        let model_note = params
            .model
            .as_deref()
            .map(|m| format!(", model: {m}"))
            .unwrap_or_default();

        let delegation = PendingDelegation {
            task: params.task.clone(),
            from: self.sender_id,
            task_id: task_id.clone(),
            model: params.model,
        };

        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(delegation);

        ToolResult::text(format!(
            "delegated task to a new pane (task_id: {task_id}{model_note}). \
             the sub-agent will work on it independently and send results back \
             as a message when done. continue with your own work."
        ))
    }
}

fn rand_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{:x}", t & 0xFFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_starts_empty() {
        let q = new_queue();
        assert!(q.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[tokio::test]
    async fn delegate_tool_queues_task() {
        let q = new_queue();
        let tool = DelegateTaskTool {
            sender_id: PaneId::new(1),
            queue: q.clone(),
        };

        let args = serde_json::json!({
            "task": "review src/main.rs for security issues",
            "task_id": "review-1"
        });
        let result = tool.execute(args).await;
        assert!(result.outcome.is_success());

        let pending = q.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].task, "review src/main.rs for security issues");
        assert_eq!(pending[0].task_id, "review-1");
        assert_eq!(pending[0].from, PaneId::new(1));
    }

    #[tokio::test]
    async fn delegate_tool_auto_generates_task_id() {
        let q = new_queue();
        let tool = DelegateTaskTool {
            sender_id: PaneId::new(1),
            queue: q.clone(),
        };

        let args = serde_json::json!({ "task": "run tests" });
        let result = tool.execute(args).await;
        assert!(result.outcome.is_success());

        let pending = q.lock().unwrap_or_else(|e| e.into_inner());
        assert!(pending[0].task_id.starts_with("del-"));
    }

    #[tokio::test]
    async fn delegate_tool_with_model() {
        let q = new_queue();
        let tool = DelegateTaskTool {
            sender_id: PaneId::new(1),
            queue: q.clone(),
        };

        let args = serde_json::json!({
            "task": "review code",
            "model": "fast"
        });
        let result = tool.execute(args).await;
        assert!(result.outcome.is_success());

        let pending = q.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(pending[0].model.as_deref(), Some("fast"));
    }

    #[tokio::test]
    async fn delegate_tool_rejects_bad_args() {
        let q = new_queue();
        let tool = DelegateTaskTool {
            sender_id: PaneId::new(1),
            queue: q.clone(),
        };

        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.outcome.is_error());
        assert!(q.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }
}
