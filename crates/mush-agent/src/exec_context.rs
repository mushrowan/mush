tokio::task_local! {
    pub static TOOL_EXEC_ID: String;
    pub static CONTROL_ENDPOINT: Option<ControlEndpoint>;
}

#[derive(Debug, Clone)]
pub struct ControlEndpoint {
    pub socket: std::path::PathBuf,
    pub target: crate::ipc::control::Target,
}

pub fn control_endpoint() -> Option<ControlEndpoint> {
    CONTROL_ENDPOINT.try_with(Clone::clone).ok().flatten()
}

pub fn apply_control_environment(command: &mut tokio::process::Command) {
    for key in [
        "MUSH_CONTROL_SOCKET",
        "MUSH_CONTROL_INSTANCE",
        "MUSH_CONTROL_SESSION",
        "MUSH_CONTROL_ACTIVATION",
    ] {
        command.env_remove(key);
    }
    if let Some(endpoint) = control_endpoint() {
        command
            .env("MUSH_CONTROL_SOCKET", endpoint.socket)
            .env("MUSH_CONTROL_INSTANCE", endpoint.target.instance)
            .env("MUSH_CONTROL_SESSION", endpoint.target.session)
            .env("MUSH_CONTROL_ACTIVATION", endpoint.target.activation);
    }
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
