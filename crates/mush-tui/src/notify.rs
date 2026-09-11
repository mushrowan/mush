use mush_tools::notification::{DELIVERY_TIMEOUT, command_with_sound, deliver};

static DELIVERY_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Complete,
    Attention,
    Error,
}

impl Sound {
    fn name(self) -> &'static str {
        match self {
            Self::Complete => "message",
            Self::Attention => "dialog-information",
            Self::Error => "dialog-warning",
        }
    }
}

pub fn send(summary: &str, body: &str) {
    send_with_sound(summary, body, None);
}

pub fn send_with_sound(summary: &str, body: &str, sound: Option<Sound>) {
    let command = command_with_sound(summary, body, sound.map(Sound::name));
    spawn_delivery(command, DELIVERY_TIMEOUT, &DELIVERY_SLOTS);
}

fn spawn_delivery(
    command: tokio::process::Command,
    timeout: std::time::Duration,
    slots: &'static tokio::sync::Semaphore,
) -> Option<tokio::task::JoinHandle<Result<(), mush_tools::notification::DeliveryError>>> {
    let Ok(permit) = slots.try_acquire() else {
        tracing::warn!("desktop notification delivery limit reached");
        return None;
    };
    Some(tokio::spawn(async move {
        let _permit = permit;
        let result = deliver(command, timeout).await;
        if let Err(ref error) = result {
            tracing::warn!(%error, "desktop notification failed");
        }
        result
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_tools::notification::DeliveryError;

    #[tokio::test]
    async fn delivery_launch_failure_is_observable() {
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let directory = tempfile::tempdir().unwrap();
        let command = tokio::process::Command::new(directory.path().join("missing"));
        let task = spawn_delivery(command, DELIVERY_TIMEOUT, &SLOTS).unwrap();
        assert!(matches!(task.await.unwrap(), Err(DeliveryError::Launch(_))));
        assert_eq!(SLOTS.available_permits(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delivery_is_nonblocking_bounded_and_times_out() {
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        let task = spawn_delivery(command, std::time::Duration::from_millis(100), &SLOTS).unwrap();
        assert!(!task.is_finished());
        assert!(
            spawn_delivery(
                tokio::process::Command::new("unused"),
                DELIVERY_TIMEOUT,
                &SLOTS
            )
            .is_none()
        );
        assert!(matches!(task.await.unwrap(), Err(DeliveryError::Timeout)));
        assert_eq!(SLOTS.available_permits(), 1);
    }

    #[test]
    fn desktop_owns_the_only_sound() {
        let command = command_with_sound(
            "awaiting input",
            "task stopped and needs your input",
            Some(Sound::Complete.name()),
        );
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(
            args,
            [
                "--app-name=mush",
                "--urgency=normal",
                "--hint=string:sound-name:message",
                "--",
                "awaiting input",
                "task stopped and needs your input"
            ]
        );
    }
}
