use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

const CHECK: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(10);

#[must_use]
pub fn launch_lease_path(socket: &Path) -> PathBuf {
    socket.with_extension("launch")
}

fn answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

pub async fn watch(
    socket: PathBuf,
    program: PathBuf,
    log_dir: PathBuf,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(CHECK) => {}
        }
        if answers(&socket) {
            continue;
        }
        match elect(&socket, &program, &log_dir).await {
            Ok(true) => tracing::info!(socket = %socket.display(), "started a replacement daemon"),
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "could not start a replacement daemon"),
        }
    }
}

pub async fn elect(socket: &Path, program: &Path, log_dir: &Path) -> io::Result<bool> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lease = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(launch_lease_path(socket))?;
    match lease.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    if answers(socket) {
        return Ok(false);
    }
    let _ = std::fs::remove_file(socket);
    std::fs::create_dir_all(log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))?;
    std::process::Command::new(program)
        .arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .process_group(0)
        .spawn()?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while !answers(socket) {
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the replacement daemon never answered",
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(true)
}
