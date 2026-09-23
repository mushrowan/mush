use color_eyre::eyre::{Result, eyre};
use mush_ai::types::Timestamp;
use mush_daemon::{SupervisorReply, SupervisorRequest, WorkerDescriptor};
use mush_session::SessionMeta;
use mush_session::SessionStore;
use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::output::Style;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Working,
    Failed,
    Paused,
    Idle,
    Saved,
}

impl AgentStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Failed => "failed",
            Self::Paused => "paused",
            Self::Idle => "idle",
            Self::Saved => "saved",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::Working => Style::Yellow,
            Self::Failed => Style::Red,
            Self::Paused => Style::Dim,
            Self::Idle => Style::Cyan,
            Self::Saved => Style::Dim,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRow {
    pub name: String,
    pub id: String,
    pub status: AgentStatus,
    pub model: String,
    pub messages: usize,
    pub updated_at: u64,
    pub pid: Option<u32>,
    pub cwd: String,
    pub stale: bool,
    pub clients: usize,
    pub failed: Option<String>,
}

const NAME_WIDTH: usize = 32;

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn cell(text: &str) -> String {
    mush_tui::widgets::top_bar::sanitise_title(&mush_tui::widgets::ansi::strip_ansi(text))
}

fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut used = 0;
    for c in text.chars() {
        let next = used + c.width().unwrap_or(0);
        if next > width.saturating_sub(1) {
            break;
        }
        used = next;
        kept.push(c);
    }
    format!("{}…", kept.trim_end())
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

fn age(updated_at: u64) -> String {
    if updated_at == 0 {
        return String::new();
    }
    Timestamp::from_ms(updated_at.saturating_mul(1000)).age_display()
}

#[must_use]
pub fn rows(workers: &[WorkerDescriptor], saved: &[SessionMeta]) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = workers
        .iter()
        .map(|worker| AgentRow {
            name: worker.title.clone().unwrap_or_default(),
            id: short_id(&worker.session_id),
            status: match (&worker.failed, worker.turn_active, worker.paused) {
                (Some(_), _, _) => AgentStatus::Failed,
                (None, true, _) => AgentStatus::Working,
                (None, false, true) => AgentStatus::Paused,
                (None, false, false) => AgentStatus::Idle,
            },
            model: worker.model_id.clone(),
            messages: worker.messages,
            updated_at: worker.updated_at.max(worker.started_at),
            pid: Some(worker.pid),
            cwd: worker.cwd.display().to_string(),
            stale: worker.build != mush_daemon::build_id(),
            clients: worker.clients,
            failed: worker.failed.clone(),
        })
        .collect();
    for meta in saved {
        if workers
            .iter()
            .any(|worker| worker.session_id == meta.id.as_str())
        {
            continue;
        }
        rows.push(AgentRow {
            name: meta.title.clone().unwrap_or_default(),
            id: short_id(meta.id.as_str()),
            status: AgentStatus::Saved,
            model: meta.model_id.to_string(),
            messages: meta.message_count,
            updated_at: meta.updated_at.as_ms() / 1000,
            pid: None,
            cwd: meta.cwd.clone(),
            stale: false,
            clients: 0,
            failed: None,
        });
    }
    rows.sort_by(|left, right| {
        left.status
            .cmp(&right.status)
            .then(right.updated_at.cmp(&left.updated_at))
    });
    rows
}

#[must_use]
pub fn table(rows: &[AgentRow]) -> String {
    let cells: Vec<[String; 8]> = rows
        .iter()
        .map(|row| {
            [
                clip(&cell(&row.name), NAME_WIDTH),
                if row.stale {
                    format!("{} *", cell(&row.id))
                } else {
                    cell(&row.id)
                },
                row.status.label().to_string(),
                age(row.updated_at),
                cell(&row.model),
                row.messages.to_string(),
                row.clients.to_string(),
                cell(row.failed.as_deref().unwrap_or(&row.cwd)),
            ]
        })
        .collect();
    let headers = [
        "name", "id", "status", "age", "model", "msgs", "clients", "cwd",
    ];
    let mut widths = headers.map(str::len);
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.width());
        }
    }
    let mut out = String::new();
    let header = headers
        .iter()
        .zip(widths)
        .map(|(header, width)| format!("{header:<width$}"))
        .collect::<Vec<_>>()
        .join("  ");
    out.push_str(&Style::Dim.stdout(header.trim_end()).to_string());
    for (row, cell) in rows.iter().zip(&cells) {
        let line = cell
            .iter()
            .zip(widths)
            .enumerate()
            .map(|(column, (value, width))| {
                let padded = pad(value, width);
                match column {
                    2 => row.status.style().stdout(padded).to_string(),
                    7 => Style::Dim.stdout(padded).to_string(),
                    _ => padded,
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        out.push('\n');
        out.push_str(line.trim_end());
    }
    if rows.iter().any(|row| row.stale) {
        out.push_str("\n\n");
        out.push_str(&Style::Dim.stdout("* another build").to_string());
    }
    out
}

pub async fn live() -> Result<Vec<WorkerDescriptor>> {
    let socket = mush_daemon::daemon_socket_path();
    if std::os::unix::net::UnixStream::connect(&socket).is_err() {
        return Ok(Vec::new());
    }
    match mush_daemon::supervisor::request(&socket, &SupervisorRequest::List).await? {
        SupervisorReply::Workers(workers) => Ok(workers),
        SupervisorReply::Error(error) => Err(eyre!("{error}")),
        other => Err(eyre!("unexpected daemon reply: {other:?}")),
    }
}

fn saved_sessions() -> Vec<SessionMeta> {
    let store = SessionStore::new(SessionStore::default_dir());
    store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|meta| meta.parent.is_none())
        .collect()
}

pub fn matching<'a>(
    workers: &'a [WorkerDescriptor],
    selector: &str,
) -> Result<Option<&'a WorkerDescriptor>> {
    let mut found = workers
        .iter()
        .filter(|worker| worker.session_id.starts_with(selector));
    let first = found.next();
    if found.next().is_some() {
        return Err(eyre!("{selector} matches more than one agent"));
    }
    Ok(first)
}

pub async fn connect(worker: &WorkerDescriptor) -> Result<mush_daemon::DaemonConnection> {
    mush_daemon::DaemonConnection::connect(&worker.socket)
        .await
        .map_err(|error| eyre!("{}: {error}", short_id(&worker.session_id)))
}

fn renamed(id: &str, name: &str, json: bool) {
    if json {
        println!("{}", serde_json::json!({ "id": id, "name": name }));
    } else {
        println!("renamed {} to {name}", short_id(id));
    }
}

pub async fn rename(selector: &str, name: &str, json: bool) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(eyre!("a name cannot be empty"));
    }
    let workers = live().await?;
    if let Some(worker) = matching(&workers, selector)? {
        let mut connection = connect(worker).await?;
        mush_runtime::AgentConnection::send(
            &mut connection,
            mush_runtime::ClientCommand::Session(mush_runtime::SessionCommand::Name {
                title: Some(name.to_string()),
            }),
        )
        .await?;
        renamed(&worker.session_id, name, json);
        return Ok(());
    }
    let store = SessionStore::new(SessionStore::default_dir());
    let meta = store
        .resolve(selector)
        .map_err(|error| eyre!("failed to resolve session: {error}"))?;
    if store.is_leased(&meta.id) {
        return Err(eyre!("{} is open elsewhere", short_id(meta.id.as_str())));
    }
    let mut session = store
        .load(&meta.id)
        .map_err(|error| eyre!("failed to load session: {error}"))?;
    session.meta.title = Some(name.to_string());
    store
        .save(&session)
        .map_err(|error| eyre!("failed to save session: {error}"))?;
    renamed(meta.id.as_str(), name, json);
    Ok(())
}

pub async fn list(all: bool, json: bool) -> Result<()> {
    let workers = live().await?;
    let saved = if all { saved_sessions() } else { Vec::new() };
    let rows = rows(&workers, &saved);
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("{}", if all { "no agents" } else { "no live agents" });
        return Ok(());
    }
    println!("{}", table(&rows));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(id: &str, turn_active: bool, updated_at: u64) -> WorkerDescriptor {
        WorkerDescriptor {
            paused: false,
            session_id: id.into(),
            model_id: "gpt-5.4".into(),
            build: mush_daemon::build_id(),
            pid: 42,
            start_id: None,
            cwd: "/tmp".into(),
            socket: "/tmp/a.sock".into(),
            started_at: 1,
            clients: 1,
            failed: None,
            title: Some(format!("agent {id}")),
            turn_active,
            messages: 7,
            updated_at,
        }
    }

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            paused: None,
            id: mush_ai::types::SessionId::from(id.to_string()),
            title: None,
            model_id: "gpt-5.4".into(),
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            message_count: 3,
            cwd: "/tmp".into(),
            parent: None,
            subagent_model: None,
        }
    }

    #[test]
    fn a_working_agent_sorts_above_an_idle_one_and_saved_comes_last() {
        let workers = [
            worker("aaaaaaaa1", false, 100),
            worker("bbbbbbbb2", true, 50),
        ];
        let rows = rows(&workers, &[meta("cccccccc3")]);
        let order: Vec<_> = rows.iter().map(|row| row.status).collect();
        assert_eq!(
            order,
            [AgentStatus::Working, AgentStatus::Idle, AgentStatus::Saved]
        );
    }

    #[test]
    fn a_live_agent_hides_its_saved_session() {
        let workers = [worker("aaaaaaaa1", false, 100)];
        let rows = rows(&workers, &[meta("aaaaaaaa1"), meta("dddddddd4")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, AgentStatus::Idle);
        assert_eq!(rows[1].status, AgentStatus::Saved);
    }

    #[test]
    fn the_newest_agent_comes_first_within_a_status() {
        let workers = [
            worker("aaaaaaaa1", false, 100),
            worker("bbbbbbbb2", false, 900),
        ];
        let rows = rows(&workers, &[]);
        assert_eq!(rows[0].id, "bbbbbbbb");
    }

    #[test]
    fn the_table_has_a_header_and_a_row_per_agent() {
        let rendered = table(&rows(&[worker("aaaaaaaa1", false, 100)], &[]));
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("name"));
        assert!(lines[0].contains("msgs"));
        assert!(lines[1].contains("aaaaaaaa"));
        assert!(lines[1].contains("idle"));
        assert!(lines[1].ends_with("/tmp"));
    }

    #[test]
    fn wide_names_line_up_by_display_width() {
        let mut wide = worker("aaaaaaaa1", false, 100);
        wide.title = Some("日本語".into());
        let mut narrow = worker("bbbbbbbb2", false, 90);
        narrow.title = Some("narrow".into());
        let rendered = table(&rows(&[wide, narrow], &[]));
        let columns: Vec<usize> = rendered
            .lines()
            .skip(1)
            .map(|line| {
                line[..line
                    .find("bbbbbbbb")
                    .or_else(|| line.find("aaaaaaaa"))
                    .unwrap()]
                    .width()
            })
            .collect();
        assert_eq!(columns[0], columns[1], "{rendered}");
    }

    #[test]
    fn a_long_wide_name_is_clipped_to_the_column_width() {
        let clipped = clip(&"字".repeat(40), NAME_WIDTH);
        assert!(clipped.width() <= NAME_WIDTH, "{clipped}");
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn a_name_loses_its_escapes_controls_and_newlines() {
        let mut noisy = worker("aaaaaaaa1", false, 100);
        noisy.title = Some("\x1b[31mred\x1b[0m\nline\ttwo\x07".into());
        let rendered = table(&rows(&[noisy], &[]));
        assert_eq!(rendered.lines().count(), 2, "{rendered:?}");
        assert!(rendered.contains("red line two "), "{rendered:?}");
        assert!(!rendered.contains('\x07'));
    }

    #[test]
    fn a_worker_from_another_build_is_starred() {
        let mut stale = worker("aaaaaaaa1", false, 100);
        stale.build = "other".into();
        let rendered = table(&rows(&[stale], &[]));
        assert!(rendered.contains("aaaaaaaa *"));
        assert!(rendered.ends_with("* another build"));
    }
}
