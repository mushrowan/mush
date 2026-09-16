use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::Utc;
use mush_supervisor::inbox::replace;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::{Lock, Manifest, Result, Worker, atomic, private_file, read_json, safe_name};

fn manifest(directory: &Path) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_reader(File::open(directory.join("manifest.json"))?)?;
    if fs::canonicalize(&manifest.directory)? != directory {
        return Err("manifest belongs to another run".into());
    }
    Ok(manifest)
}

pub fn queue(directory: &Path, worker: &str, prompt: &str) -> Result<()> {
    if !safe_name(worker) {
        return Err("invalid worker name".into());
    }
    if prompt.trim().is_empty() || prompt.len() > 128 * 1024 {
        return Err("poke requires a nonempty message of at most 128 kib".into());
    }
    let directory = fs::canonicalize(directory)?;
    let restartable = directory.join("manifest.json").try_exists()?;
    if restartable {
        let manifest = manifest(&directory)?;
        if !manifest.workers.iter().any(|job| job.name == worker) {
            return Err("unknown worker".into());
        }
    } else {
        let state =
            read_json(&directory.join(worker).join("state.json")).ok_or("unknown worker")?;
        if state["state"] != "running" {
            return Err("restarting a worker requires its saved manifest".into());
        }
    }
    let inbox = directory.join("inbox").join(worker);
    fs::create_dir_all(&inbox)?;
    let path = inbox.join(format!(
        "{}-{}.txt",
        Utc::now().timestamp_nanos_opt().ok_or("invalid clock")?,
        std::process::id()
    ));
    replace(&path, prompt.as_bytes())?;
    if !restartable {
        return Ok(());
    }
    let log = private_file(&directory.join("poke.log"))?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("deliver-poke")
        .arg(&directory)
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    tokio::spawn(async move { child.wait().await });
    Ok(())
}

pub fn pending(directory: &Path, worker: &str) -> Result<bool> {
    Ok(!messages(directory, worker)?.is_empty())
}

pub fn messages(directory: &Path, worker: &str) -> Result<Vec<PathBuf>> {
    let inbox = directory.join("inbox").join(worker);
    let entries = match fs::read_dir(inbox) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut messages = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "txt") {
            messages.push(path);
        }
    }
    messages.sort();
    Ok(messages)
}

pub async fn deliver(directory: &Path, message: &Path) -> Result<()> {
    let directory = fs::canonicalize(directory)?;
    let file = private_file(&directory.join("supervisor.lock"))?;
    while message.try_exists()? {
        match file.try_lock() {
            Ok(()) => {
                let lock = Lock(file);
                if !message.try_exists()? {
                    return Ok(());
                }
                let mut manifest = manifest(&directory)?;
                if manifest.until <= Utc::now() {
                    manifest.until = Utc::now()
                        + chrono::Duration::seconds(manifest.attempt_seconds.clamp(1, 86400) as i64);
                }
                crate::validate(&manifest)?;
                match fs::remove_file(directory.join("STOP")) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                return crate::supervise(manifest, lock, true).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

pub async fn reopen(manifest: &Manifest, job: &Worker, previous: Option<&Value>) -> Result<()> {
    let dir = manifest.directory.join(&job.name);
    if let Some(tracked) =
        read_json(&dir.join("process.json")).and_then(|value| serde_json::from_value(value).ok())
    {
        crate::signal_tree(&tracked, "-KILL").await;
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    if previous["state"] == "follow_up_pending" {
        return Ok(());
    }
    let round = previous["round"].as_u64().unwrap_or(0);
    let history = dir.join("pokes");
    fs::create_dir_all(&history)?;
    let report = read_json(&job.report);
    let archive = history.join(format!("after-{round:03}.json"));
    if !archive.exists() {
        atomic(&archive, &json!({"state":previous,"report":report}))?;
    }
    let mut state = previous.clone();
    state["state"] = json!("follow_up_pending");
    state["at"] = json!(Utc::now());
    state["detail"] = json!("explicit message queued");
    if let Some(object) = state.as_object_mut() {
        object.remove("terminal_event");
    }
    if let Some(mut report) = report.filter(Value::is_object) {
        report["state"] = json!("working");
        atomic(&job.report, &report)?;
    }
    atomic(&dir.join("state.json"), &state)
}
