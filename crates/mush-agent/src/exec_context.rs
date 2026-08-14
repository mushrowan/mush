tokio::task_local! {
    pub static TOOL_EXEC_ID: String;
}

#[must_use]
pub fn batch_sub_exec_id(batch_id: &str, index: usize) -> String {
    format!("{batch_id}#{index}")
}

#[must_use]
pub fn batch_sub_index(exec_id: &str, batch_id: &str) -> Option<usize> {
    exec_id
        .strip_prefix(batch_id)?
        .strip_prefix('#')?
        .parse()
        .ok()
}

#[must_use]
pub fn current_exec_id() -> String {
    TOOL_EXEC_ID.try_with(String::clone).unwrap_or_default()
}
