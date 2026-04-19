//! delegate_task tool: spawn a sub-agent in a new pane
//!
//! the calling agent forks a new pane with a task prompt.
//! the sub-agent runs independently and sends results back
//! via the messaging system.
//!
//! ## status
//!
//! this tool is currently NOT registered with the agent (see
//! `runner/streams.rs` `build_extra_tools`). the module and queue
//! live on so that re-enabling is a one-line change once the known
//! reliability issues are addressed.
//!
//! ## audit findings (2026-04, kept here as the single source of truth
//! for anyone re-enabling delegation)
//!
//! 1. `complete_delegation` in `runner/streams.rs` appends a User
//!    message straight onto the parent's conversation. on its own that
//!    is borrow-safe because every mutation is driven from the main
//!    TUI loop, but it injects turns out-of-band w.r.t. steering queue
//!    ordering. if delegation completes mid-stream on the parent, the
//!    injected turn lands after the in-flight assistant message in the
//!    history but isn't visible to the API call until the next turn.
//!    prefer `parent.steering_queue.push(...)` when the parent has an
//!    active stream so ordering matches user-submitted steering.
//!
//! 2. `DelegateTaskTool::execute` doesn't cap queue length. a runaway
//!    agent could push thousands of pending delegations in one turn
//!    before the TUI loop drains them. add a hard cap (e.g. 8) and
//!    return an error when exceeded.
//!
//! 3. `rand_id()` uses the low 32 bits of wall-clock millis. two
//!    delegations created in the same millisecond collide. swap for
//!    an atomic counter plus an incarnation prefix.
//!
//! 4. sub-agent panes inherit the parent's cwd but not its per-pane
//!    VCS isolation. a jj or worktree parent spawning a delegation
//!    pane will write to the isolation root, not the branch. decide
//!    whether delegation should branch the isolation too.
//!
//! 5. no overall timeout. `DELEGATION_MAX_TURNS` bounds agent turns
//!    inside the pane but a sub-agent stuck between turns (api error
//!    loop, missing confirmation) can sit forever. wrap the whole
//!    delegation in a wall-clock deadline.
//!
//! 6. silent drop on pane-manager push failure. `process_delegations`
//!    `add_pane` can't fail today but the flow has no channel to
//!    report "couldn't start" back to the parent. wire an error path.
//!
//! re-enable checklist:
//!   - pick a fix for (1) first, it's the only race-shaped concern
//!   - add (2) as a guardrail before any live test
//!   - (3)-(6) can land gradually once the tool proves useful

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

/// hard cap on pending delegations. a misbehaving agent could flood the
/// queue with thousands of tasks before the TUI loop drains it, so bound
/// queue length and return an error when exceeded
const MAX_PENDING_DELEGATIONS: usize = 8;

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

        let task_id = params.task_id.unwrap_or_else(next_task_id);

        let model_note = params
            .model
            .as_deref()
            .map(|m| format!(", model: {m}"))
            .unwrap_or_default();

        let delegation = PendingDelegation {
            task: params.task,
            from: self.sender_id,
            task_id: task_id.clone(),
            model: params.model,
        };

        {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= MAX_PENDING_DELEGATIONS {
                return ToolResult::error(format!(
                    "delegation queue full ({MAX_PENDING_DELEGATIONS} pending). \
                     wait for a sub-agent to finish before spawning more"
                ));
            }
            q.push(delegation);
        }

        ToolResult::text(format!(
            "delegated task to a new pane (task_id: {task_id}{model_note}). \
             the sub-agent will work on it independently and send results back \
             as a message when done. continue with your own work."
        ))
    }
}

/// monotonically increasing task id: `del-<counter>-<millis-suffix>`.
/// the counter prevents two calls in the same millisecond from colliding,
/// the millis suffix keeps ids roughly time-ordered across restarts.
fn next_task_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("del-{n}-{:x}", t & 0xFFFF)
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

    #[tokio::test]
    async fn delegate_tool_rejects_when_queue_full() {
        // guardrail: a misbehaving agent that keeps submitting delegations
        // faster than the TUI can drain them should get a clear error
        // once the cap is hit, not flood the queue unboundedly
        let q = new_queue();
        let tool = DelegateTaskTool {
            sender_id: PaneId::new(1),
            queue: q.clone(),
        };

        for i in 0..MAX_PENDING_DELEGATIONS {
            let result = tool
                .execute(serde_json::json!({ "task": format!("task {i}") }))
                .await;
            assert!(result.outcome.is_success(), "task {i} should enqueue");
        }

        let result = tool
            .execute(serde_json::json!({ "task": "one too many" }))
            .await;
        assert!(
            result.outcome.is_error(),
            "submission past the cap should error out"
        );
        assert_eq!(
            q.lock().unwrap_or_else(|e| e.into_inner()).len(),
            MAX_PENDING_DELEGATIONS,
            "queue length must not exceed the cap"
        );
    }

    #[test]
    fn next_task_id_is_unique_across_same_millisecond() {
        // counter-based ids prevent collisions when two delegations land
        // inside the same wall-clock millisecond. the old rand_id() used
        // only millis and would collide
        let ids: std::collections::HashSet<String> = (0..100).map(|_| next_task_id()).collect();
        assert_eq!(
            ids.len(),
            100,
            "100 rapid calls must produce 100 unique ids"
        );
    }
}
