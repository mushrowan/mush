use color_eyre::eyre::{Result, eyre};
use mush_ai::types::Timestamp;
use mush_daemon::WorkerDescriptor;
use mush_runtime::{AgentConnection, ClientCommand, ClientEvent, Job, JobStatus, ScheduleCommand};
use serde::Serialize;

use crate::agents;
use crate::output::{Style, short_id};

#[derive(Debug, Clone, Serialize)]
pub struct JobRow {
    pub agent: String,
    pub id: String,
    pub label: Option<String>,
    pub expression: String,
    pub status: String,
    pub next_at: Option<u64>,
    pub runs: u32,
    pub prompt: String,
}

fn agent_label(worker: &WorkerDescriptor) -> String {
    match worker.title.as_deref() {
        Some(title) if !title.is_empty() => format!("{} {}", short_id(&worker.session_id), title),
        _ => short_id(&worker.session_id).to_owned(),
    }
}

fn until(at: u64, now: u64) -> String {
    if at <= now {
        return "due".into();
    }
    let secs = (at - now) / 1000;
    if secs < 60 {
        return format!("in {secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("in {mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("in {hours}h");
    }
    format!("in {}d", hours / 24)
}

async fn jobs_of(worker: &WorkerDescriptor) -> Result<Vec<Job>> {
    let mut connection = agents::connect(worker).await?;
    let events = connection
        .send(ClientCommand::Schedule(ScheduleCommand::List))
        .await?;
    for event in events {
        if let ClientEvent::ScheduleChanged(jobs) = event {
            return Ok(jobs);
        }
    }
    Ok(Vec::new())
}

fn row(worker: &WorkerDescriptor, job: &Job) -> JobRow {
    JobRow {
        agent: agent_label(worker),
        id: job.id.clone(),
        label: job.label.clone(),
        expression: job.expression.clone(),
        status: format!("{:?}", job.status).to_lowercase(),
        next_at: job.next_at(),
        runs: job.runs,
        prompt: job.prompt.lines().next().unwrap_or_default().to_string(),
    }
}

async fn selected(selector: Option<&str>) -> Result<Vec<WorkerDescriptor>> {
    let workers = agents::live().await?;
    match selector {
        Some(selector) => Ok(vec![agents::sole_match(workers, selector)?]),
        None => Ok(workers),
    }
}

pub async fn list(selector: Option<&str>, all: bool, json: bool) -> Result<()> {
    let mut rows = Vec::new();
    for worker in selected(selector).await? {
        let jobs = match jobs_of(&worker).await {
            Ok(jobs) => jobs,
            Err(error) => {
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!("{}: {error}", short_id(&worker.session_id)))
                );
                continue;
            }
        };
        for job in jobs {
            if !all && !matches!(job.status, JobStatus::Active | JobStatus::Paused) {
                continue;
            }
            rows.push(row(&worker, &job));
        }
    }
    rows.sort_by_key(|row| row.next_at.unwrap_or(u64::MAX));
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nothing scheduled");
        return Ok(());
    }
    println!("{}", table(&rows));
    Ok(())
}

#[must_use]
pub fn table(rows: &[JobRow]) -> String {
    let now = Timestamp::now().as_ms();
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|row| {
            [
                row.agent.clone(),
                row.id.clone(),
                row.expression.clone(),
                row.next_at.map_or_else(String::new, |at| until(at, now)),
                row.status.clone(),
                row.label.clone().unwrap_or_else(|| row.prompt.clone()),
            ]
        })
        .collect();
    let headers = ["agent", "job", "when", "next", "status", "prompt"];
    let mut widths = headers.map(str::len);
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let header = headers
        .iter()
        .zip(widths)
        .map(|(header, width)| format!("{header:<width$}"))
        .collect::<Vec<_>>()
        .join("  ");
    let mut out = Style::Dim.stdout(header.trim_end()).to_string();
    for row in &cells {
        let line = row
            .iter()
            .zip(widths)
            .map(|(value, width)| format!("{value:<width$}"))
            .collect::<Vec<_>>()
            .join("  ");
        out.push('\n');
        out.push_str(line.trim_end());
    }
    out
}

pub async fn add(selector: &str, expression: &str, message: &str) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        return Err(eyre!("a scheduled prompt needs text"));
    }
    let worker = agents::sole_match(agents::live().await?, selector)?;
    let mut connection = agents::connect(&worker).await?;
    let events = connection
        .send(ClientCommand::Schedule(ScheduleCommand::Add {
            expression: expression.to_string(),
            prompt: message.to_string(),
            label: None,
            source: mush_runtime::JobSource::Schedule,
        }))
        .await?;
    let now = Timestamp::now().as_ms();
    for event in events {
        if let ClientEvent::ScheduleChanged(jobs) = event
            && let Some(job) = jobs.last()
        {
            println!(
                "scheduled {} on {} {}",
                job.id,
                short_id(&worker.session_id),
                job.next_at().map_or_else(String::new, |at| until(at, now)),
            );
            return Ok(());
        }
    }
    Err(eyre!("the agent did not answer with a schedule"))
}

pub async fn cancel(id: &str) -> Result<()> {
    for worker in agents::live().await? {
        let jobs = jobs_of(&worker).await.unwrap_or_default();
        if !jobs.iter().any(|job| job.id == id) {
            continue;
        }
        let mut connection = agents::connect(&worker).await?;
        connection
            .send(ClientCommand::Schedule(ScheduleCommand::Cancel(
                id.to_string(),
            )))
            .await?;
        println!("cancelled {id} on {}", short_id(&worker.session_id));
        return Ok(());
    }
    Err(eyre!("no live agent has job {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, next_at: Option<u64>) -> JobRow {
        JobRow {
            agent: "aaaaaaaa scout".into(),
            id: id.into(),
            label: None,
            expression: "every 10m".into(),
            status: "active".into(),
            next_at,
            runs: 0,
            prompt: "check the build".into(),
        }
    }

    #[test]
    fn the_table_shows_the_agent_and_the_prompt() {
        let now = Timestamp::now().as_ms();
        let rendered = table(&[job("j1", Some(now + 600_000))]);
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("agent"));
        assert!(lines[1].contains("aaaaaaaa scout"));
        assert!(lines[1].contains("every 10m"));
        assert!(lines[1].contains("in 10m"));
        assert!(lines[1].ends_with("check the build"));
    }

    #[test]
    fn a_job_with_no_next_run_has_an_empty_next_column() {
        let rendered = table(&[job("j1", None)]);
        assert!(!rendered.contains("in "));
    }

    #[test]
    fn a_due_job_says_so() {
        let now = Timestamp::now().as_ms();
        assert_eq!(until(now - 1000, now), "due");
        assert_eq!(until(now + 30_000, now), "in 30s");
        assert_eq!(until(now + 7_200_000, now), "in 2h");
        assert_eq!(until(now + 172_800_000, now), "in 2d");
    }
}
