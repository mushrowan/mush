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

pub async fn shutdown(force: bool) -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    if !answers(&socket) {
        println!("no daemon running");
        return Ok(());
    }
    if !force && !confirm_shutdown(&socket).await? {
        println!("left running");
        return Ok(());
    }
    match mush_daemon::supervisor::request(&socket, &SupervisorRequest::Shutdown).await? {
        SupervisorReply::Done => Ok(()),
        SupervisorReply::Error(error) => Err(eyre!("{error}")),
        other => Err(eyre!("unexpected daemon reply: {other:?}")),
    }
}

async fn confirm_shutdown(socket: &Path) -> Result<bool> {
    let workers = match mush_daemon::supervisor::request(socket, &SupervisorRequest::List).await? {
        SupervisorReply::Workers(workers) => workers.len(),
        _ => 0,
    };
    if workers == 0 {
        return Ok(true);
    }
    if !atty::is(atty::Stream::Stdin) {
        return Err(eyre!(
            "{workers} agent(s) are running, pass --force to stop them"
        ));
    }
    eprint!("stop {workers} running agent(s)? [y/N] ");
    std::io::Write::flush(&mut std::io::stderr())?;
    let mut answer = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

pub async fn send(session: &str, text: &str) -> Result<()> {
    use mush_agent::ipc::control::{Event, Operation, Reply, Request, Target};
    use mush_agent::{IpcMessage, IpcMessageKind};

    let control_dir = mush_agent::ipc::runtime_dir().join("control");
    let sockets: Vec<PathBuf> = std::fs::read_dir(&control_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
        .collect();
    let mut found: Option<(PathBuf, Target)> = None;
    for socket in sockets {
        let Ok(response) = mush_agent::ipc::listener::exchange(
            &socket,
            &IpcMessage {
                id: mush_agent::ipc::control::new_id(),
                from: "mush-send".into(),
                kind: IpcMessageKind::GetTarget,
            },
        )
        .await
        else {
            continue;
        };
        let IpcMessageKind::Target(target) = response.kind else {
            continue;
        };
        if target.session.starts_with(session) {
            if found.is_some() {
                return Err(eyre!("{session} matches more than one running session"));
            }
            found = Some((socket, target));
        }
    }
    let Some((socket, target)) = found else {
        return Err(eyre!("no running session matches {session}"));
    };
    let response = mush_agent::ipc::listener::exchange(
        &socket,
        &IpcMessage {
            id: mush_agent::ipc::control::new_id(),
            from: "mush-send".into(),
            kind: IpcMessageKind::Control(Request {
                target: target.clone(),
                operation: Operation::Send {
                    event: Event::Message {
                        text: text.to_string(),
                    },
                },
            }),
        },
    )
    .await?;
    match response.kind {
        IpcMessageKind::ControlReply(Reply::Accepted { .. }) => {
            println!(
                "queued for {}",
                &target.session[..8.min(target.session.len())]
            );
            Ok(())
        }
        IpcMessageKind::ControlReply(Reply::Error { code }) => Err(eyre!("{code}")),
        other => Err(eyre!("unexpected reply: {other:?}")),
    }
}

pub fn session_spawner(worker_args: Vec<String>) -> mush_runtime::SessionSpawner {
    std::sync::Arc::new(move |request: mush_runtime::CreateSessionRequest| {
        let mut args = worker_args.clone();
        Box::pin(async move {
            if let Some(model) = &request.model {
                args.retain(|arg| arg != "--model");
                args.push("--model".into());
                args.push(model.clone());
            }
            let descriptor = open(OpenTarget::New, args)
                .await
                .map_err(|error| error.to_string())?;
            let mut connection = mush_daemon::DaemonConnection::connect(&descriptor.socket)
                .await
                .map_err(|error| error.to_string())?;
            use mush_runtime::AgentConnection;
            if let Some(name) = &request.name {
                connection
                    .send(mush_runtime::ClientCommand::Session(
                        mush_runtime::SessionCommand::Name {
                            title: Some(name.clone()),
                        },
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
            }
            connection
                .send(mush_runtime::ClientCommand::Prompt {
                    text: request.prompt,
                    images: Vec::new(),
                })
                .await
                .map_err(|error| error.to_string())?;
            Ok(mush_runtime::CreatedSession {
                session_id: descriptor.session_id,
                name: request.name,
                model: descriptor.model_id,
            })
        })
    })
}
