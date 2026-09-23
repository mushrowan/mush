use std::io;
use std::path::{Path, PathBuf};
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
    mush_daemon::election::elect(
        &socket,
        &std::env::current_exe()?,
        &mush_session::data_dir(),
    )
    .await?;
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

pub async fn open(target: OpenTarget, args: Vec<String>) -> Result<WorkerDescriptor> {
    open_in(target, args, std::env::current_dir()?).await
}

async fn open_in(
    target: OpenTarget,
    args: Vec<String>,
    cwd: std::path::PathBuf,
) -> Result<WorkerDescriptor> {
    let socket = ensure_running().await?;
    let request = SupervisorRequest::Open {
        target,
        args,
        cwd,
        env: std::env::vars().collect(),
    };
    match mush_daemon::supervisor::request(&socket, &request).await? {
        SupervisorReply::Worker { descriptor, .. } => Ok(*descriptor),
        SupervisorReply::Error(error) => Err(eyre!("{error}")),
        other => Err(eyre!("unexpected daemon reply: {other:?}")),
    }
}

pub async fn stop(selector: &str, json: bool) -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    let session_id = if answers(&socket) {
        let workers = crate::agents::live().await?;
        let Some(worker) = crate::agents::matching(&workers, selector)? else {
            return Err(eyre!("no worker for session {selector}"));
        };
        let request = SupervisorRequest::Stop {
            session_id: worker.session_id.clone(),
        };
        match mush_daemon::supervisor::request(&socket, &request).await? {
            SupervisorReply::Done => worker.session_id.clone(),
            SupervisorReply::Error(error) => return Err(eyre!("{error}")),
            other => return Err(eyre!("unexpected daemon reply: {other:?}")),
        }
    } else {
        let running = mush_daemon::supervisor::running_workers(&mush_daemon::workers_dir());
        let Some(worker) = crate::agents::matching(&running, selector)? else {
            if json {
                return Err(eyre!("no worker for session {selector}"));
            }
            println!("no daemon running");
            return Ok(());
        };
        mush_daemon::supervisor::stop_unsupervised(worker).await?;
        worker.session_id.clone()
    };
    if json {
        println!("{}", serde_json::json!({ "stopped": session_id }));
    }
    Ok(())
}

pub async fn shutdown(force: bool, json: bool) -> Result<()> {
    let socket = mush_daemon::daemon_socket_path();
    let stopped = if answers(&socket) {
        let workers = crate::agents::live().await?;
        if !force && !confirm_shutdown(workers.len(), json)? {
            println!("left running");
            return Ok(());
        }
        match mush_daemon::supervisor::request(&socket, &SupervisorRequest::Shutdown).await? {
            SupervisorReply::Done => workers,
            SupervisorReply::Error(error) => return Err(eyre!("{error}")),
            other => return Err(eyre!("unexpected daemon reply: {other:?}")),
        }
    } else {
        shutdown_unsupervised(force, json).await?
    };
    if json {
        let stopped: Vec<&str> = stopped
            .iter()
            .map(|worker| worker.session_id.as_str())
            .collect();
        println!("{}", serde_json::json!({ "stopped": stopped }));
    }
    Ok(())
}

async fn shutdown_unsupervised(force: bool, json: bool) -> Result<Vec<WorkerDescriptor>> {
    let running = mush_daemon::supervisor::running_workers(&mush_daemon::workers_dir());
    if running.is_empty() {
        if !json {
            println!("no daemon running");
        }
        return Ok(running);
    }
    if !force {
        return Err(eyre!(
            "no daemon answers but {} agent(s) are still running, pass --force to kill them",
            running.len()
        ));
    }
    for worker in &running {
        mush_daemon::supervisor::stop_unsupervised(worker).await?;
    }
    Ok(running)
}

fn confirm_shutdown(workers: usize, json: bool) -> Result<bool> {
    if workers == 0 {
        return Ok(true);
    }
    if json || !atty::is(atty::Stream::Stdin) {
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
                args = with_model(args, model);
            }
            let cwd = match request.cwd {
                Some(cwd) => {
                    let cwd = std::env::current_dir()
                        .map_err(|error| error.to_string())?
                        .join(cwd);
                    if !cwd.is_dir() {
                        return Err(format!("cwd {} is not a directory", cwd.display()));
                    }
                    cwd
                }
                None => std::env::current_dir().map_err(|error| error.to_string())?,
            };
            let descriptor = open_in(OpenTarget::New, args, cwd)
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
            if let Some(level) = request.thinking {
                connection
                    .send(mush_runtime::ClientCommand::SetThinking(level))
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

fn with_model(args: Vec<String>, model: &str) -> Vec<String> {
    let mut kept = Vec::with_capacity(args.len() + 2);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--model" {
            args.next();
            continue;
        }
        kept.push(arg);
    }
    kept.push("--model".into());
    kept.push(model.into());
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_new_model_replaces_the_flag_and_its_value() {
        let args = strings(&["--thinking", "--model", "old", "--max-turns", "4"]);
        assert_eq!(
            with_model(args, "new"),
            strings(&["--thinking", "--max-turns", "4", "--model", "new"])
        );
    }

    #[test]
    fn a_model_is_added_when_none_was_set() {
        assert_eq!(
            with_model(strings(&["--no-tools"]), "new"),
            strings(&["--no-tools", "--model", "new"])
        );
    }
}
