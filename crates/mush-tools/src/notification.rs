use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("could not launch notification helper: {0}")]
    Launch(std::io::Error),
    #[error("could not wait for notification helper: {0}")]
    Wait(std::io::Error),
    #[error("notification helper exited with {0}")]
    Exit(std::process::ExitStatus),
    #[error("notification helper timed out")]
    Timeout,
    #[error("could not stop notification helper: {0}")]
    Stop(std::io::Error),
}

pub fn command(summary: &str, body: &str) -> Command {
    command_with_sound(summary, body, None)
}

pub fn command_with_sound(summary: &str, body: &str, sound: Option<&str>) -> Command {
    let mut command = Command::new("notify-send");
    command.args(["--app-name=mush", "--urgency=normal"]);
    if let Some(sound) = sound {
        command.arg(format!("--hint=string:sound-name:{sound}"));
    }
    command.args(["--", summary, body]);
    command
}

pub async fn deliver(mut command: Command, timeout: Duration) -> Result<(), DeliveryError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(DeliveryError::Launch)?;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(DeliveryError::Exit(status)),
        Ok(Err(error)) => Err(DeliveryError::Wait(error)),
        Err(_) => {
            child.kill().await.map_err(DeliveryError::Stop)?;
            Err(DeliveryError::Timeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_follows_argument_separator() {
        let command = command("--wait", "--action=secret");
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(
            args,
            [
                "--app-name=mush",
                "--urgency=normal",
                "--",
                "--wait",
                "--action=secret"
            ]
        );
    }

    #[tokio::test]
    async fn missing_helper_reports_launch_error() {
        let directory = tempfile::tempdir().unwrap();
        let command = Command::new(directory.path().join("missing"));
        assert!(matches!(
            deliver(command, DELIVERY_TIMEOUT).await,
            Err(DeliveryError::Launch(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_exit_status_is_checked() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        let result = deliver(command, DELIVERY_TIMEOUT).await;
        assert!(matches!(result, Err(DeliveryError::Exit(status)) if status.code() == Some(7)));
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        assert!(deliver(command, DELIVERY_TIMEOUT).await.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_helper_is_reaped() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo $$ > \"$1\"; exec sleep 30", "helper"]);
        command.arg(&pid_file);
        let result = deliver(command, Duration::from_millis(200)).await;
        assert!(matches!(result, Err(DeliveryError::Timeout)));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
        #[cfg(not(target_os = "linux"))]
        assert!(!pid.trim().is_empty());
    }
}
