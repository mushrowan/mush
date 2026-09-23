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
    apply_control_environment_with_path(command, std::env::var_os("PATH").unwrap_or_default());
}

pub fn apply_control_environment_with_path(
    command: &mut tokio::process::Command,
    path: std::ffi::OsString,
) {
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
    if let Some(path) = path_with_own_bin(&path) {
        command.env("PATH", path);
    }
}

fn path_with_own_bin(current: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let bin = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let rest: Vec<_> = std::env::split_paths(current)
        .filter(|dir| *dir != bin)
        .collect();
    std::env::join_paths(std::iter::once(bin).chain(rest)).ok()
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

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_running_binary_leads_the_path_of_a_child() {
        let mut command = tokio::process::Command::new("sh");
        super::apply_control_environment(&mut command);
        let output = command
            .args(["-c", "printf '%s' \"$PATH\""])
            .output()
            .await
            .unwrap();
        let path = String::from_utf8(output.stdout).unwrap();
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(std::env::split_paths(&path).next(), Some(bin));
    }
}
