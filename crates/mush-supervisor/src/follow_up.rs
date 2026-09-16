use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use chrono::Utc;
use serde_json::{Value, json};

use crate::{Lock, Manifest, Result, atomic, safe_name, validate};

fn bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if !path.symlink_metadata()?.is_file() {
        return Err("expected a regular follow-up file".into());
    }
    let mut output = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?
        .take(limit + 1)
        .read_to_end(&mut output)?;
    if output.len() as u64 > limit {
        return Err("follow-up input exceeds size limit".into());
    }
    Ok(output)
}

fn save(path: &Path, content: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    Ok(())
}

pub fn queue(directory: &Path, worker: &str, prompt_file: &Path) -> Result<()> {
    if !safe_name(worker) {
        return Err("invalid worker name".into());
    }
    let directory = fs::canonicalize(directory)?;
    let _lock = Lock::acquire(&directory.join("supervisor.lock"))?;
    if directory.join("review-reservation.json").exists() {
        return Err("run has a review reservation".into());
    }
    let manifest: Manifest =
        serde_json::from_slice(&bytes(&directory.join("manifest.json"), 1024 * 1024)?)?;
    let _session = if manifest.admission.is_some() {
        let state: Value = serde_json::from_slice(&bytes(
            &directory.join(worker).join("state.json"),
            1024 * 1024,
        )?)?;
        let id = state["session_id"]
            .as_str()
            .ok_or("supervised session identity missing")?;
        Some(mush_supervisor::admission::session_lock(
            &directory.join(worker).join("mush-data/sessions"),
            id,
            None,
        )?)
    } else {
        None
    };
    queue_locked(&directory, worker, prompt_file)
}

pub fn queue_locked(directory: &Path, worker: &str, prompt_file: &Path) -> Result<()> {
    if directory.join("STOP").exists() {
        return Err("run is stopped".into());
    }
    let manifest: Manifest =
        serde_json::from_slice(&bytes(&directory.join("manifest.json"), 1024 * 1024)?)?;
    validate(&manifest)?;
    if fs::canonicalize(&manifest.directory)? != directory {
        return Err("manifest belongs to another run".into());
    }
    let supervisor: Value =
        serde_json::from_slice(&bytes(&directory.join("supervisor.json"), 1024 * 1024)?)?;
    if supervisor["state"] != "stopped" {
        return Err("stop the supervisor before queueing completed work".into());
    }
    let job = manifest
        .workers
        .iter()
        .find(|w| w.name == worker)
        .ok_or("unknown worker")?;
    let prompt = String::from_utf8(bytes(prompt_file, 128 * 1024)?)?;
    if prompt.trim().is_empty() {
        return Err("follow-up prompt is empty".into());
    }
    let dir = directory.join(worker);
    recover(directory, worker)?;
    if !dir.symlink_metadata()?.is_dir() {
        return Err("invalid worker directory".into());
    }
    let state_path = dir.join("state.json");
    let original_state = bytes(&state_path, 1024 * 1024)?;
    let mut state: Value = serde_json::from_slice(&original_state)?;
    if !matches!(
        state["state"].as_str(),
        Some("done" | "blocked" | "awaiting_input")
    ) {
        return Err("follow-up requires a completed or blocked worker".into());
    }
    let round = state["round"]
        .as_u64()
        .ok_or("worker has no completed round")?;
    if round >= u64::from(manifest.max_rounds) {
        return Err("worker round budget is exhausted".into());
    }
    let assignment = manifest
        .journalled()
        .then(|| crate::events::next_assignment(&state, round))
        .transpose()?;
    if !manifest.status_file && !directory.join("status-enabled").exists() {
        return Err("follow-up requires checkpointed sessions".into());
    }
    let checkpoint = bytes(&dir.join(format!("round-{round:03}.json")), 1024 * 1024)?;
    let checkpoint_value: Value = serde_json::from_slice(&checkpoint)?;
    let session = checkpoint_value["session_id"]
        .as_str()
        .ok_or("worker has no saved session")?;
    if !safe_name(session) {
        return Err("invalid saved session id".into());
    }
    let saved_session = bytes(
        &dir.join("mush-data/sessions")
            .join(format!("{session}.json")),
        64 * 1024 * 1024,
    )?;
    let original_report = bytes(&job.report, 2 * 1024 * 1024)?;
    let mut report: Value = serde_json::from_slice(&original_report)?;
    if !report.is_object() {
        return Err("worker report must be an object".into());
    }
    let history = dir.join("followups");
    if history.is_symlink() {
        return Err("invalid follow-up history directory".into());
    }
    fs::create_dir_all(&history)?;
    let archive = history.join(format!("after-{round:03}"));
    let staged = history.join(format!("preparing-{round:03}"));
    fs::DirBuilder::new().mode(0o700).create(&staged)?;
    save(&staged.join("state.json"), &original_state)?;
    save(&staged.join("report.json"), &original_report)?;
    save(&staged.join("checkpoint.json"), &checkpoint)?;
    save(&staged.join("session.json"), &saved_session)?;
    save(&staged.join("prompt.txt"), prompt.as_bytes())?;
    save(
        &staged.join("prompt.sha256"),
        mush_supervisor::review::digest(prompt.as_bytes()).as_bytes(),
    )?;
    if directory.join("STOP").exists() || Utc::now() >= manifest.until {
        return Err("run stopped while preparing follow-up".into());
    }
    report["state"] = json!("working");
    report["follow_up"] = json!({"after_round":round,"prompt":archive.join("prompt.txt"),"previous_report":archive.join("report.json"),"queued_at":Utc::now()});
    state["state"] = json!("follow_up_pending");
    if let Some(assignment) = assignment {
        state["assignment_id"] = json!(assignment);
    }
    state["session_id"] = json!(session);
    state["detail"] = json!("explicit follow-up queued");
    state["at"] = json!(Utc::now());
    if let Some(state) = state.as_object_mut() {
        state.remove("terminal_event");
    }
    save(
        &staged.join("next-state.json"),
        &serde_json::to_vec(&state)?,
    )?;
    save(
        &staged.join("next-report.json"),
        &serde_json::to_vec(&report)?,
    )?;
    fs::File::open(&staged)?.sync_all()?;
    if archive.exists() {
        return Err("follow-up archive already exists".into());
    }
    fs::rename(&staged, &archive)?;
    fs::File::open(&history)?.sync_all()?;
    atomic(&job.report, &report)?;
    if let Err(error) = atomic(&state_path, &state) {
        atomic(
            &job.report,
            &serde_json::from_slice::<Value>(&original_report)?,
        )?;
        return Err(error);
    }
    save(&archive.join("committed"), b"committed")?;
    fs::File::open(&archive)?.sync_all()?;
    println!("{}", archive.display());
    Ok(())
}

pub fn recover(directory: &Path, worker: &str) -> Result<()> {
    let history = directory.join(worker).join("followups");
    if history.is_symlink() {
        return Err("invalid follow-up history directory".into());
    }
    let entries = match fs::read_dir(&history) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let manifest: Manifest =
        serde_json::from_slice(&bytes(&directory.join("manifest.json"), 1024 * 1024)?)?;
    let job = manifest
        .workers
        .iter()
        .find(|job| job.name == worker)
        .ok_or("unknown follow-up worker")?;
    let mut entries = entries.collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() > 1000 {
        return Err("follow-up history exceeds limit".into());
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("preparing-") {
            return Err("incomplete follow-up archive requires reconciliation".into());
        }
        let archive = entry.path();
        if name.starts_with("after-") && !archive.symlink_metadata()?.is_dir() {
            return Err("unsafe follow-up archive".into());
        }
        if !name.starts_with("after-")
            || archive.join("committed").exists()
            || !archive.join("next-state.json").exists()
        {
            continue;
        }
        let state_path = directory.join(worker).join("state.json");
        let next_state = bytes(&archive.join("next-state.json"), 1024 * 1024)?;
        let next_report = bytes(&archive.join("next-report.json"), 1024 * 1024)?;
        for (current, previous, next) in [
            (&state_path, archive.join("state.json"), &next_state),
            (&job.report, archive.join("report.json"), &next_report),
        ] {
            let current = bytes(current, 1024 * 1024)?;
            let previous = bytes(&previous, 1024 * 1024)?;
            let current: Value = serde_json::from_slice(&current)?;
            if current != serde_json::from_slice::<Value>(&previous)?
                && current != serde_json::from_slice::<Value>(next)?
            {
                return Err("follow-up transaction changed, reconcile manually".into());
            }
        }
        mush_supervisor::inbox::replace(&job.report, &next_report)?;
        mush_supervisor::inbox::replace(&state_path, &next_state)?;
        save(&archive.join("committed"), b"committed")?;
        fs::File::open(&archive)?.sync_all()?;
    }
    Ok(())
}

pub fn instructions(directory: &Path, round: u32) -> Result<Option<String>> {
    let history = directory.join("followups");
    if !history.exists() {
        return Ok(None);
    }
    let mut latest = None;
    for entry in fs::read_dir(history)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(after) = name
            .to_str()
            .and_then(|s| s.strip_prefix("after-"))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if after < round
            && latest
                .as_ref()
                .is_none_or(|(previous, _)| after > *previous)
        {
            latest = Some((after, entry.path()));
        }
    }
    latest
        .map(|(_, path)| {
            let prompt = bytes(&path.join("prompt.txt"), 128 * 1024)?;
            let expected = path.join("prompt.sha256");
            if expected.exists()
                && bytes(&expected, 64)? != mush_supervisor::review::digest(&prompt).as_bytes()
            {
                return Err("follow-up prompt changed".into());
            }
            String::from_utf8(prompt).map_err(Into::into)
        })
        .transpose()
}
