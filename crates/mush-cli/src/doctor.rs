use std::path::{Path, PathBuf};
use std::time::Duration;

use color_eyre::eyre::Result;
use mush_daemon::{SupervisorReply, SupervisorRequest, WorkerDescriptor};
use serde::Serialize;
use tokio::io::AsyncBufReadExt;

use crate::output::Style;

const PROBE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    Idle,
    Unreachable,
    Stale,
}

impl Health {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Idle => "idle",
            Self::Unreachable => "unreachable",
            Self::Stale => "stale",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Ok => Style::Green,
            Self::Idle => Style::Dim,
            Self::Unreachable => Style::Red,
            Self::Stale => Style::Yellow,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub what: String,
    pub health: Health,
    pub detail: String,
    pub fixed: bool,
}

async fn answers(socket: &Path) -> bool {
    let Ok(Ok(stream)) = tokio::time::timeout(PROBE, tokio::net::UnixStream::connect(socket)).await
    else {
        return false;
    };
    let mut lines = tokio::io::BufReader::new(stream).lines();
    matches!(
        tokio::time::timeout(PROBE, lines.next_line()).await,
        Ok(Ok(Some(_)))
    )
}

fn listening(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

fn descriptors() -> Vec<(PathBuf, WorkerDescriptor)> {
    let dir = mush_daemon::workers_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let bytes = std::fs::read(&path).ok()?;
            let descriptor = serde_json::from_slice::<WorkerDescriptor>(&bytes).ok()?;
            Some((path, descriptor))
        })
        .collect()
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn remove_if_present(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

pub async fn findings(fix: bool) -> Result<Vec<Finding>> {
    let descriptors = descriptors();
    let sockets: Vec<PathBuf> = std::fs::read_dir(mush_daemon::workers_dir())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
        .collect();
    let mut findings = Vec::new();
    let socket = mush_daemon::daemon_socket_path();
    let build = mush_daemon::build_id();
    if socket.exists() {
        match mush_daemon::supervisor::request(&socket, &SupervisorRequest::Status).await {
            Ok(SupervisorReply::Status {
                build: theirs,
                workers,
            }) => findings.push(Finding {
                what: "daemon".into(),
                health: if theirs == build {
                    Health::Ok
                } else {
                    Health::Stale
                },
                detail: if theirs == build {
                    format!("{workers} worker(s)")
                } else {
                    format!("{workers} worker(s), another build")
                },
                fixed: false,
            }),
            _ => {
                let fixed = fix && std::fs::remove_file(&socket).is_ok();
                findings.push(Finding {
                    what: "daemon".into(),
                    health: Health::Unreachable,
                    detail: format!("nothing answers on {}", socket.display()),
                    fixed,
                });
            }
        }
    } else {
        findings.push(Finding {
            what: "daemon".into(),
            health: Health::Idle,
            detail: "not running".into(),
            fixed: false,
        });
    }

    let mut known = Vec::new();
    for (path, descriptor) in descriptors {
        known.push(descriptor.socket.clone());
        let what = format!("worker {}", short_id(&descriptor.session_id));
        if !descriptor.alive() {
            let fixed = fix && remove_if_present(&path) && remove_if_present(&descriptor.socket);
            findings.push(Finding {
                what,
                health: Health::Stale,
                detail: format!("pid {} is gone", descriptor.pid),
                fixed,
            });
            continue;
        }
        if !answers(&descriptor.socket).await {
            findings.push(Finding {
                what,
                health: Health::Unreachable,
                detail: format!("pid {} is alive but its socket is silent", descriptor.pid),
                fixed: false,
            });
            continue;
        }
        findings.push(Finding {
            what,
            health: if descriptor.build == build {
                Health::Ok
            } else {
                Health::Stale
            },
            detail: format!("{} client(s), {}", descriptor.clients, descriptor.model_id),
            fixed: false,
        });
    }

    for path in sockets.into_iter().filter(|path| !known.contains(path)) {
        if !path.exists() {
            continue;
        }
        if listening(&path) {
            findings.push(Finding {
                what: format!("socket {}", path.display()),
                health: Health::Unreachable,
                detail: "a listener with no descriptor".into(),
                fixed: false,
            });
            continue;
        }
        let fixed = fix && remove_if_present(&path);
        findings.push(Finding {
            what: format!("socket {}", path.display()),
            health: Health::Stale,
            detail: "nothing listens here".into(),
            fixed,
        });
    }

    if fix {
        let journal = mush_tools::orphans::OrphanJournal::at(
            mush_session::data_dir().join("orphan-processes.jsonl"),
        );
        let reaped = journal.reap_abandoned();
        if !reaped.killed.is_empty() || !reaped.gone.is_empty() {
            findings.push(Finding {
                what: "bash jobs".into(),
                health: Health::Stale,
                detail: format!(
                    "{} killed, {} already gone",
                    reaped.killed.len(),
                    reaped.gone.len()
                ),
                fixed: !reaped.killed.is_empty(),
            });
        }
    }

    Ok(findings)
}

#[must_use]
pub fn report(findings: &[Finding]) -> String {
    let width = findings
        .iter()
        .map(|finding| finding.what.chars().count())
        .max()
        .unwrap_or(0);
    findings
        .iter()
        .map(|finding| {
            let fixed = if finding.fixed { " (fixed)" } else { "" };
            format!(
                "{:<width$}  {}  {}{fixed}",
                finding.what,
                finding
                    .health
                    .style()
                    .stdout(format!("{:<11}", finding.health.label())),
                Style::Dim.stdout(&finding.detail),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn doctor(fix: bool, json: bool) -> Result<()> {
    let findings = findings(fix).await;
    let findings = findings?;
    if json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
        return Ok(());
    }
    println!("{}", report(&findings));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(what: &str, health: Health) -> Finding {
        Finding {
            what: what.into(),
            health,
            detail: "2 client(s)".into(),
            fixed: health == Health::Stale,
        }
    }

    #[test]
    fn the_report_aligns_and_marks_what_it_fixed() {
        let rendered = report(&[
            finding("daemon", Health::Ok),
            finding("worker abcdefgh", Health::Stale),
        ]);
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("daemon         "));
        assert!(lines[0].contains("ok"));
        assert!(!lines[0].contains("(fixed)"));
        assert!(lines[1].starts_with("worker abcdefgh"));
        assert!(lines[1].ends_with("(fixed)"));
    }

    #[tokio::test]
    async fn a_silent_socket_does_not_answer() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("silent.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        assert!(listening(&socket));
        assert!(!answers(&socket).await);
    }
}
