use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use mush_supervisor::inbox::{
    Event, Inbox, TerminalReason, private_directory, read_private, replace,
};
use serde_json::{Value, json};

use crate::{Manifest, Result, State, Worker, atomic, read_json};

pub fn initialise(manifest: &Manifest) -> Result<()> {
    if manifest.journalled() {
        private_directory(&manifest.directory)?;
        let binding = manifest
            .orchestrator_inbox
            .as_ref()
            .map(|p| Inbox::open(p)?.binding())
            .transpose()?;
        if !manifest.status_file || manifest.max_rounds > 1024 || manifest.workers.len() > 32 {
            return Err("orchestrator events require status_file and at most 1024 rounds".into());
        }
        for worker in &manifest.workers {
            let cwd = fs::canonicalize(&worker.cwd)?;
            if binding.as_ref().is_some_and(|b| !b.workers.contains(&cwd)) {
                return Err("worker directory is outside the inbox binding".into());
            }
            if !worker.report.starts_with(cwd)
                || worker
                    .report
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir))
            {
                return Err("worker report must stay inside its workspace".into());
            }
        }
        let identity = manifest.directory.join("event-run.json");
        if !identity.exists() {
            if manifest
                .workers
                .iter()
                .any(|w| manifest.directory.join(&w.name).join("state.json").exists())
            {
                return Err("register orchestrator events before starting workers".into());
            }
            atomic(
                &identity,
                &json!({"run_id":format!("{}-{}", chrono::Utc::now().timestamp_nanos_opt().ok_or("invalid clock")?, std::process::id())}),
            )?;
        }
    }
    Ok(())
}

pub fn run_id(manifest: &Manifest) -> Result<String> {
    let identity: Value = serde_json::from_slice(&read_private(
        &manifest.directory.join("event-run.json"),
        1024,
    )?)?;
    identity["run_id"]
        .as_str()
        .filter(|id| mush_supervisor::inbox::identifier(id))
        .map(str::to_owned)
        .ok_or_else(|| "invalid event run identity".into())
}

pub fn assignment(previous: Option<&Value>, round: u32) -> u32 {
    if previous.is_some_and(|v| v["state"] == "follow_up_pending") {
        round.saturating_add(1)
    } else {
        previous
            .and_then(|v| v["assignment_id"].as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(1)
    }
}

pub fn report_state(
    manifest: &Manifest,
    job: &Worker,
    assignment: u32,
    round: u32,
) -> Result<String> {
    let report = match read_private(&job.report, 2 * 1024 * 1024) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
        Err(_) => None,
    };
    let Some(report) = report else {
        return Ok("working".into());
    };
    if report["run_id"] != run_id(manifest)?
        || report["worker_id"] != job.name
        || report["assignment_id"] != assignment
        || report["round"] != round
    {
        return Ok("working".into());
    }
    Ok(report["state"].as_str().unwrap_or("working").to_owned())
}

pub fn save(
    manifest: &Manifest,
    job: &Worker,
    assignment: u32,
    state: &State<'_>,
    reason: Option<TerminalReason>,
) -> Result<()> {
    let dir = manifest.directory.join(&job.name);
    let mut value = serde_json::to_value(state)?;
    if manifest.journalled() {
        value["assignment_id"] = json!(assignment);
        if let Some(reason) = reason {
            let reports = dir.join("event-reports");
            if !reports.exists() {
                fs::DirBuilder::new().mode(0o700).create(&reports)?;
            }
            private_directory(&reports)?;
            let report_path = reports.join(format!("assignment-{assignment:04}.json"));
            if report_path.exists() {
                let journal: Value =
                    serde_json::from_slice(&read_private(&report_path, 3 * 1024 * 1024)?)?;
                return atomic(&dir.join("state.json"), &journal["state"]);
            }
            let report = read_private(&job.report, 2 * 1024 * 1024)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let review = job
                .review_verdict
                .as_ref()
                .map(|path| read_private(path, 256 * 1024))
                .transpose();
            let review = match review {
                Ok(Some(bytes)) => {
                    json!({"sha256":mush_supervisor::review::digest(&bytes),"bytes":bytes})
                }
                Ok(None) => Value::Null,
                Err(error) => json!({"error":error.to_string()}),
            };
            let run_id = run_id(manifest)?;
            let event = Event {
                event_id: format!("{run_id}.{}.{assignment}", job.name),
                run_id,
                worker_id: job.name.clone(),
                assignment_id: assignment,
                round: state.round.max(assignment),
                session_id: state
                    .session_id
                    .filter(|s| mush_supervisor::inbox::identifier(s))
                    .map(str::to_owned),
                reason,
                report_path: report_path.clone(),
                review: None,
            };
            event.validate()?;
            value["terminal_event"] = serde_json::to_value(event)?;
            replace(
                &report_path,
                &serde_json::to_vec(
                    &json!({"state":value,"worker_report":report,"review":review}),
                )?,
            )?;
        }
    }
    atomic(&dir.join("state.json"), &value)
}

pub fn replay(manifest: &Manifest) -> Result<()> {
    let Some(path) = &manifest.orchestrator_inbox else {
        return Ok(());
    };
    let inbox = Inbox::open(path)?;
    let known = inbox.events()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    for job in &manifest.workers {
        for path in reports(manifest, job)? {
            if std::time::Instant::now() >= deadline {
                return Err("orchestrator replay budget exhausted, events remain queued".into());
            }
            let value: Value = serde_json::from_slice(&read_private(&path, 3 * 1024 * 1024)?)?;
            let event: Event = serde_json::from_value(value["state"]["terminal_event"].clone())?;
            if event.run_id != run_id(manifest)?
                || event.worker_id != job.name
                || event.report_path != path
            {
                return Err("terminal report identity mismatch".into());
            }
            if let Some(existing) = known
                .iter()
                .find(|existing| existing.event_id == event.event_id)
            {
                if *existing != event {
                    return Err("orchestrator event id collision".into());
                }
                continue;
            }
            inbox.enqueue(event, &job.cwd)?;
        }
    }
    Ok(())
}

pub fn saved_reason(state: &str) -> Option<TerminalReason> {
    match state {
        "done" => Some(TerminalReason::Completed),
        "blocked" => Some(TerminalReason::Blocked),
        "awaiting_input" => Some(TerminalReason::AwaitingInput),
        "cancelled" => Some(TerminalReason::Cancelled),
        _ => None,
    }
}

fn reports(manifest: &Manifest, job: &Worker) -> Result<Vec<std::path::PathBuf>> {
    let directory = manifest.directory.join(&job.name).join("event-reports");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    private_directory(&directory)?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json") {
            paths.push(path);
        }
        if paths.len() > 1024 {
            return Err("terminal report retention exceeds limit".into());
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn recover(manifest: &Manifest, job: &Worker) -> Result<()> {
    if !manifest.journalled() {
        return Ok(());
    }
    let state_path = manifest.directory.join(&job.name).join("state.json");
    let previous = read_json(&state_path);
    if previous
        .as_ref()
        .is_some_and(|v| v["state"] == "follow_up_pending")
    {
        return Ok(());
    }
    if let Some(path) = reports(manifest, job)?.last() {
        let value: Value = serde_json::from_slice(&read_private(path, 3 * 1024 * 1024)?)?;
        let state = &value["state"];
        if state["round"]
            .as_u64()
            .ok_or("invalid terminal report round")?
            >= previous
                .as_ref()
                .and_then(|v| v["round"].as_u64())
                .unwrap_or(0)
        {
            atomic(&state_path, state)?;
        }
    }
    Ok(())
}

pub fn has_terminal_event(path: &Path) -> bool {
    read_json(path).is_some_and(|v| v.get("terminal_event").is_some())
}
