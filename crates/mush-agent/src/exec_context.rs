//! ambient identifier for the currently-executing tool call.
//!
//! set by the agent loop (and the batch tool for its sub-calls) around
//! each tool `execute`, so streaming sinks (e.g. bash live output) can
//! attribute their output to the right tool-call panel even when several
//! tools run concurrently. without this a shared sink would route every
//! line to whichever panel happened to be last.

tokio::task_local! {
    /// id of the tool execution currently on this task. for top-level
    /// calls this is the tool-call id; the batch tool re-scopes it to a
    /// synthetic `{batch_id}#{index}` per sub-call.
    pub static TOOL_EXEC_ID: String;
}

/// build the synthetic exec id for sub-call `index` of a batch whose own
/// exec id is `batch_id`. kept in one place so the batch tool and the tui
/// agree on the scheme.
#[must_use]
pub fn batch_sub_exec_id(batch_id: &str, index: usize) -> String {
    format!("{batch_id}#{index}")
}

/// inverse of [`batch_sub_exec_id`]: if `exec_id` is a sub-call of the batch
/// `batch_id`, return its index, else `None`.
#[must_use]
pub fn batch_sub_index(exec_id: &str, batch_id: &str) -> Option<usize> {
    exec_id
        .strip_prefix(batch_id)?
        .strip_prefix('#')?
        .parse()
        .ok()
}

/// read the current exec id, or an empty string when unset (e.g. outside
/// the agent loop, in tests, or a tool invoked directly).
#[must_use]
pub fn current_exec_id() -> String {
    TOOL_EXEC_ID.try_with(String::clone).unwrap_or_default()
}
