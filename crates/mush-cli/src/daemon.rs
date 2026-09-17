use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use mush_daemon::{OpenTarget, SupervisorReply, SupervisorRequest, WorkerDescriptor};

use crate::output::Style;

const START_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn ensure_running() -> io::Result<PathBuf> {
    let socket = mush_daemon::daemon_socket_path();
    if answers(&socket) {
        match mush_daemon::supervisor::request(&socket, &SupervisorRequest::Status).await {
            Ok(SupervisorReply::Status { build, workers }) if build == mush_daemon::build_id() => {
                let _ = workers;
                return Ok(socket);
            }
            Ok(SupervisorReply::Status { workers, .. }) if workers > 0 => {
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!(
                        "the daemon is from another build and has {workers} worker(s) running; mush shutdown to replace it"
                    ))
                );
                return Ok(socket);
            }
            Ok(_) => {
                let _ =
                    mush_daemon::supervisor::request(&socket, &SupervisorRequest::Shutdown).await;
                wait_until_gone(&socket).await;
            }
            Err(error) => {
                return Err(io::Error::other(format!(
                    "the daemon on {} did not answer a status request: {error}",
                    socket.display()
                )));
            }
        }
    }
    let _ = std::fs::remove_file(&socket);
    spawn(&socket)?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if answers(&socket) {
            return Ok(socket);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the daemon did not answer on {}", socket.display()),
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

async fn wait_until_gone(socket: &Path) {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while answers(socket) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn spawn(socket: &Path) -> io::Result<()> {
    let log_dir = mush_session::data_dir();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))?;
    std::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .process_group(0)
        .spawn()?;
    Ok(())
}

pub async fn open(target: OpenTarget, args: Vec<String>) -> Result<WorkerDescriptor> {
    let socket = ensure_running().await?;
    let request = SupervisorRequest::Open {
        target,
        args,
        cwd: std::env::current_dir()?,
        env: std::env::vars().collect(),
    };
    match mush_daemon::supervisor::request(&socket, &request).await? {
        SupervisorReply::Worker { descriptor, .. } => Ok(descriptor),
        SupervisorReply::Error(error) => Err(eyre!("{error}")),
        other => Err(eyre!("unexpected daemon reply: {other:?}")),
    }
}

pub async fn ps() -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    if !answers(&socket) {
        println!("no daemon running");
        return Ok(());
    }
    let workers = match mush_daemon::supervisor::request(&socket, &SupervisorRequest::List).await? {
        SupervisorReply::Workers(workers) => workers,
        SupervisorReply::Error(error) => return Err(eyre!("{error}")),
        other => return Err(eyre!("unexpected daemon reply: {other:?}")),
    };
    if workers.is_empty() {
        println!("no workers");
        return Ok(());
    }
    let build = mush_daemon::build_id();
    for worker in &workers {
        let stale = if worker.build == build {
            ""
        } else {
            ", other build"
        };
        println!(
            "  {}  {} {}",
            Style::Dim.stdout(&worker.session_id[..8.min(worker.session_id.len())]),
            worker.model_id,
            Style::Dim.stdout(format_args!(
                "(pid {}, {}{stale})",
                worker.pid,
                worker.cwd.display()
            )),
        );
    }
    println!("\nattach with: mush --resume <id>, stop with: mush stop <id>");
    Ok(())
}

pub async fn stop(session_id: &str) -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    if !answers(&socket) {
        println!("no daemon running");
        return Ok(());
    }
    let request = SupervisorRequest::Stop {
        session_id: session_id.to_string(),
    };
    match mush_daemon::supervisor::request(&socket, &request).await? {
        SupervisorReply::Done => Ok(()),
        SupervisorReply::Error(error) => Err(eyre!("{error}")),
        other => Err(eyre!("unexpected daemon reply: {other:?}")),
    }
}

pub async fn shutdown() -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    if !answers(&socket) {
        println!("no daemon running");
        return Ok(());
    }
    match mush_daemon::supervisor::request(&socket, &SupervisorRequest::Shutdown).await? {
        SupervisorReply::Done => Ok(()),
        SupervisorReply::Error(error) => Err(eyre!("{error}")),
        other => Err(eyre!("unexpected daemon reply: {other:?}")),
    }
}
