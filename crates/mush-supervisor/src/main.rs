use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use mush_supervisor::{Action, Failure, classify, decide};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    Run {
        manifest: PathBuf,
    },
    Status {
        directory: PathBuf,
    },
    Stop {
        directory: PathBuf,
    },
    Poke {
        directory: PathBuf,
        worker: String,
        prompt: String,
    },
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    directory: PathBuf,
    executable: PathBuf,
    until: DateTime<Utc>,
    concurrency: usize,
    attempt_seconds: u64,
    silence_seconds: u64,
    max_rounds: u32,
    model: String,
    fallback_model: String,
    #[serde(default)]
    status_file: bool,
    workers: Vec<Worker>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Worker {
    name: String,
    cwd: PathBuf,
    prompt: PathBuf,
    report: PathBuf,
    #[serde(default)]
    gate: Option<PathBuf>,
    #[serde(default)]
    repeat_done: bool,
    #[serde(default = "pause_seconds")]
    pause_seconds: u64,
}

fn pause_seconds() -> u64 {
    30
}

#[derive(Serialize)]
struct State<'a> {
    name: &'a str,
    state: &'a str,
    model: &'a str,
    round: u32,
    session_id: Option<&'a str>,
    at: DateTime<Utc>,
    detail: &'a str,
    retries: u32,
    stalled: u32,
}

fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_reader(File::open(path).ok()?).ok()
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate(manifest: &Manifest) -> Result<()> {
    if manifest.concurrency == 0
        || manifest.concurrency > 8
        || manifest.max_rounds == 0
        || manifest.attempt_seconds == 0
        || manifest.silence_seconds == 0
        || manifest.until <= Utc::now()
        || manifest.until > Utc::now() + chrono::Duration::hours(24)
    {
        return Err("invalid run limits".into());
    }
    let mut names = HashSet::new();
    let mut dirs = HashSet::new();
    for worker in &manifest.workers {
        if !safe_name(&worker.name)
            || !names.insert(&worker.name)
            || !dirs.insert(fs::canonicalize(&worker.cwd)?)
        {
            return Err("workers require unique names and directories".into());
        }
        if !worker.prompt.is_file() || !worker.report.is_absolute() {
            return Err("worker requires a prompt file and absolute report path".into());
        }
    }
    Ok(())
}

struct Lock(File);

impl Lock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = private_file(path)?;
        file.try_lock()
            .map_err(|_| "another supervisor owns this run")?;
        Ok(Self(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Clone, Copy)]
struct Process {
    parent: u32,
    start: u64,
}

fn processes() -> HashMap<u32, Process> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = rest.split_whitespace().collect();
        if let (Some(parent), Some(start)) = (
            fields.get(1).and_then(|s| s.parse().ok()),
            fields.get(19).and_then(|s| s.parse().ok()),
        ) {
            out.insert(pid, Process { parent, start });
        }
    }
    out
}

fn descendants(root: u32, tracked: &mut HashMap<u32, u64>) {
    let all = processes();
    if let Some(process) = all.get(&root) {
        tracked.entry(root).or_insert(process.start);
    }
    loop {
        let mut added = false;
        for (pid, process) in &all {
            if !tracked.contains_key(pid)
                && tracked.get(&process.parent).is_some_and(|start| {
                    all.get(&process.parent).is_some_and(|p| p.start == *start)
                })
            {
                tracked.insert(*pid, process.start);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
}

async fn signal_tree(tracked: &HashMap<u32, u64>, signal: &str) {
    let current = processes();
    let pids: Vec<_> = tracked
        .iter()
        .filter(|(pid, start)| current.get(pid).is_some_and(|p| p.start == **start))
        .map(|(pid, _)| pid.to_string())
        .collect();
    if !pids.is_empty() {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            Command::new("kill")
                .arg(signal)
                .arg("--")
                .args(pids)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
    }
}

async fn terminate(child: &mut tokio::process::Child, tracked: &mut HashMap<u32, u64>) {
    if let Some(pid) = child.id() {
        descendants(pid, tracked);
    }
    signal_tree(tracked, "-TERM").await;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    signal_tree(tracked, "-KILL").await;
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn tail(path: &Path, bytes: u64) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(bytes)));
    let mut buf = Vec::new();
    let _ = file.take(bytes).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn session_from_status(path: &Path) -> Option<String> {
    read_json(path)?
        .get("session_id")?
        .as_str()
        .map(str::to_owned)
}

fn failure_from_status(path: &Path) -> Option<Failure> {
    let value = read_json(path)?;
    let classification = value
        .get("error_classification")
        .or_else(|| value.get("error_class"))
        .or_else(|| value.get("error_kind"))
        .or_else(|| value.get("failure"))?
        .as_str()?;
    Some(match classification {
        "quota" | "quota_exhausted" => Failure::Quota,
        "rate_limit" => Failure::RateLimit,
        "authentication" => Failure::Authentication,
        _ => Failure::Other,
    })
}

async fn sleep_or_stop(seconds: u64, stop: &mut watch::Receiver<bool>) {
    tokio::select! {
        () = tokio::time::sleep(Duration::from_secs(seconds)) => {},
        _ = stop.changed() => {},
    }
}

async fn worker(
    manifest: Arc<Manifest>,
    job: Worker,
    permits: Arc<Semaphore>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let dir = manifest.directory.join(&job.name);
    fs::create_dir_all(&dir)?;
    let data = dir.join("mush-data");
    fs::create_dir_all(&data)?;
    if let Some(home) = std::env::var_os("HOME") {
        let discovery = PathBuf::from(home).join(".local/share/mush/discovered-models.json");
        if discovery.is_file() {
            fs::copy(discovery, data.join("discovered-models.json"))?;
        }
    }
    let state_path = dir.join("state.json");
    let previous = read_json(&state_path);
    let previous_state = previous.as_ref().and_then(|v| v["state"].as_str());
    if matches!(previous_state, Some("blocked" | "error"))
        || previous_state == Some("done") && !job.repeat_done
    {
        return Ok(());
    }
    let previous_round = previous
        .as_ref()
        .and_then(|v| v["round"].as_u64())
        .map(u32::try_from)
        .transpose()?
        .unwrap_or(0);
    let latest_status = dir.join(format!("round-{previous_round:03}.json"));
    let mut session = session_from_status(&latest_status).or_else(|| {
        previous
            .as_ref()
            .and_then(|v| v["session_id"].as_str())
            .map(str::to_owned)
    });
    let mut model = previous
        .as_ref()
        .and_then(|v| v["model"].as_str())
        .unwrap_or(&manifest.model)
        .to_owned();
    if model != manifest.model && model != manifest.fallback_model {
        return Err("saved model is outside the authorised provider set".into());
    }
    let mut fallback = model == manifest.fallback_model;
    let counter = |key: &str| -> Result<u32> {
        previous
            .as_ref()
            .and_then(|v| v.get(key))
            .map(|value| serde_json::from_value(value.clone()).map_err(Into::into))
            .unwrap_or(Ok(0))
    };
    let mut retries = counter("retries")?;
    let mut stalled = counter("stalled")?;
    let mut last_round = previous_round;
    for round in previous_round.saturating_add(1)..=manifest.max_rounds {
        last_round = round;
        let mut detail = String::new();
        if *stop.borrow() {
            break;
        }
        while job.gate.as_ref().is_some_and(|p| !p.exists()) && !*stop.borrow() {
            atomic(
                &state_path,
                &State {
                    name: &job.name,
                    state: "waiting",
                    model: &model,
                    round,
                    session_id: session.as_deref(),
                    at: Utc::now(),
                    detail: "waiting for coordinator handoff",
                    retries,
                    stalled,
                },
            )?;
            sleep_or_stop(2, &mut stop).await;
        }
        if *stop.borrow() {
            break;
        }
        atomic(
            &state_path,
            &State {
                name: &job.name,
                state: "queued",
                model: &model,
                round,
                session_id: session.as_deref(),
                at: Utc::now(),
                detail: "waiting for an execution slot",
                retries,
                stalled,
            },
        )?;
        let permit = tokio::select! {
            permit = permits.acquire() => permit?,
            _ = stop.changed() => break,
        };
        if *stop.borrow() {
            break;
        }
        let base = fs::read_to_string(&job.prompt)?;
        let prompt = if round == 1 {
            base
        } else {
            format!(
                "{base}\n\nautomatic continuation {round}. read your data/overnight-state.json and inspect jj status before working. continue your scoped backlog or independently review your completed slice. don't repeat finished work. preserve blocked/done states. deadline {}. external effects remain forbidden.",
                manifest.until
            )
        };
        let inbox = manifest.directory.join("inbox").join(&job.name);
        let mut prompt = prompt;
        if inbox.exists() {
            let mut paths: Vec<_> = fs::read_dir(&inbox)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            paths.sort();
            for path in paths {
                if path.extension().is_some_and(|e| e == "txt") {
                    let text = fs::read_to_string(&path)?;
                    prompt.push_str("\n\ncoordinator task update (within original scope only):\n");
                    prompt.push_str(&text);
                    fs::rename(&path, path.with_extension("delivered"))?;
                }
            }
        }
        let log_path = dir.join(format!("round-{round:03}.log"));
        let headless_status = dir.join(format!("round-{round:03}.json"));
        let log = private_file(&log_path)?;
        let mut cmd = Command::new(&manifest.executable);
        cmd.current_dir(&job.cwd)
            .env("MUSH_DATA_DIR", &data)
            .env("CARGO_BUILD_JOBS", "2")
            .env("CARGO_INCREMENTAL", "0")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .process_group(0)
            .arg("--model")
            .arg(&model)
            .arg("--max-turns")
            .arg("160");
        let status_enabled =
            manifest.status_file || manifest.directory.join("status-enabled").exists();
        if status_enabled {
            cmd.arg("--status-file").arg(&headless_status);
        }
        if status_enabled && let Some(id) = &session {
            cmd.arg("--continue").arg(id);
        }
        cmd.arg("-p").arg(prompt);
        atomic(
            &state_path,
            &State {
                name: &job.name,
                state: "running",
                model: &model,
                round,
                session_id: session.as_deref(),
                at: Utc::now(),
                detail: "",
                retries,
                stalled,
            },
        )?;
        let mut child = cmd.spawn()?;
        let pid = child.id().ok_or("missing child pid")?;
        let mut tracked = HashMap::new();
        descendants(pid, &mut tracked);
        let started = Instant::now();
        let mut activity = started;
        let mut size = 0;
        let mut timer = tokio::time::interval(Duration::from_secs(2));
        let mut timed_out = false;
        let exit = loop {
            tokio::select! {
                result = child.wait() => break Some(result?),
                _ = stop.changed() => {
                    terminate(&mut child, &mut tracked).await;
                    break None;
                },
                _ = timer.tick() => {
                    descendants(pid, &mut tracked);
                    let next_size = fs::metadata(&log_path).map_or(0, |m| m.len());
                    if next_size != size { size = next_size; activity = Instant::now(); }
                    if activity.elapsed().as_secs() >= manifest.silence_seconds || started.elapsed().as_secs() >= manifest.attempt_seconds {
                        timed_out = true;
                        terminate(&mut child, &mut tracked).await;
                        break None;
                    }
                },
            }
        };
        signal_tree(&tracked, "-TERM").await;
        signal_tree(&tracked, "-KILL").await;
        drop(permit);
        if status_enabled {
            session = session_from_status(&headless_status).or(session);
        }
        if *stop.borrow() {
            break;
        }
        let report = read_json(&job.report);
        let reported_state = report
            .as_ref()
            .and_then(|r| r.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("working");
        let reported_state = if reported_state == "done" && job.repeat_done {
            "working"
        } else {
            reported_state
        };
        let mut failure = if exit.is_some_and(|s| s.success()) {
            None
        } else {
            Some(
                failure_from_status(&headless_status)
                    .unwrap_or_else(|| classify(&tail(&log_path, 16384))),
            )
        };
        if timed_out {
            stalled = stalled.saturating_add(1);
            detail = "worker exceeded its attempt or silence limit".into();
            failure = Some(if stalled < 2 {
                Failure::RateLimit
            } else {
                Failure::Other
            });
        }
        let action = decide(reported_state, failure, fallback, retries);
        let state = match action {
            Action::Continue => {
                retries = 0;
                "idle"
            }
            Action::Fallback => {
                fallback = true;
                model.clone_from(&manifest.fallback_model);
                retries = 0;
                "switching_provider"
            }
            Action::Retry => {
                retries += 1;
                "retrying"
            }
            Action::Stop => {
                if failure.is_some() {
                    "error"
                } else {
                    reported_state
                }
            }
        };
        if detail.is_empty() {
            detail = failure.map_or_else(String::new, |f| format!("{f:?}"));
        }
        atomic(
            &state_path,
            &State {
                name: &job.name,
                state,
                model: &model,
                round,
                session_id: session.as_deref(),
                at: Utc::now(),
                detail: &detail,
                retries,
                stalled,
            },
        )?;
        if action == Action::Stop {
            return Ok(());
        }
        sleep_or_stop(
            if action == Action::Retry {
                60 * u64::from(retries)
            } else {
                job.pause_seconds
            },
            &mut stop,
        )
        .await;
    }
    atomic(
        &state_path,
        &State {
            name: &job.name,
            state: "stopped",
            model: &model,
            round: last_round,
            session_id: session.as_deref(),
            at: Utc::now(),
            detail: "deadline, cancellation or round limit",
            retries,
            stalled,
        },
    )?;
    Ok(())
}

async fn run(path: &Path) -> Result<()> {
    let manifest: Manifest = serde_json::from_reader(File::open(path)?)?;
    validate(&manifest)?;
    fs::create_dir_all(&manifest.directory)?;
    let _lock = Lock::acquire(&manifest.directory.join("supervisor.lock"))?;
    if manifest.directory.join("STOP").exists() {
        return Err("run is stopped".into());
    }
    atomic(
        &manifest.directory.join("supervisor.json"),
        &json!({"state":"running", "pid":std::process::id(), "until":manifest.until, "started_at":Utc::now()}),
    )?;
    let manifest = Arc::new(manifest);
    let permits = Arc::new(Semaphore::new(manifest.concurrency));
    let (cancel, receiver) = watch::channel(false);
    let mut jobs = JoinSet::new();
    for job in manifest.workers.clone() {
        let name = job.name.clone();
        let m = manifest.clone();
        let p = permits.clone();
        let s = receiver.clone();
        jobs.spawn(async move { (name, worker(m, job, p, s).await) });
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut errors = Vec::new();
    while !jobs.is_empty() {
        tokio::select! {
            result = jobs.join_next() => match result {
                Some(Ok((name, Err(error)))) => { errors.push(json!({"worker":name,"error":error.to_string()})); },
                Some(Err(error)) => { errors.push(json!({"task_error":error.to_string()})); },
                Some(Ok((_, Ok(())))) | None => {},
            },
            _ = terminate.recv() => { cancel.send_replace(true); },
            _ = tokio::signal::ctrl_c() => { cancel.send_replace(true); },
            _ = tick.tick() => {
                if Utc::now() >= manifest.until || manifest.directory.join("STOP").exists() { cancel.send_replace(true); }
            },
        }
    }
    atomic(
        &manifest.directory.join("supervisor.json"),
        &json!({"state":"stopped", "until":manifest.until, "finished_at":Utc::now(), "errors":errors}),
    )?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Operation::Run { manifest } => run(&manifest).await,
        Operation::Stop { directory } => {
            private_file(&directory.join("STOP"))?;
            Ok(())
        }
        Operation::Poke {
            directory,
            worker,
            prompt,
        } => {
            if !safe_name(&worker) {
                return Err("invalid worker name".into());
            }
            if directory.join("STOP").exists() {
                return Err("run is stopped".into());
            }
            let dir = directory.join("inbox").join(worker);
            fs::create_dir_all(&dir)?;
            let path = dir.join(format!(
                "{}-{}.txt",
                Utc::now().timestamp_micros(),
                std::process::id()
            ));
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(prompt.as_bytes())?;
            Ok(())
        }
        Operation::Status { directory } => {
            let mut states = Vec::new();
            for entry in fs::read_dir(&directory)?.flatten() {
                if let Some(value) = read_json(&entry.path().join("state.json")) {
                    states.push(value);
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"supervisor":read_json(&directory.join("supervisor.json")),"workers":states})
                )?
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_the_run_directory() {
        for name in ["", "..", "a/b", "/tmp/x", "a.b"] {
            assert!(!safe_name(name));
        }
        assert!(safe_name("worker-1"));
    }

    #[test]
    fn only_one_supervisor_can_own_a_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let first = Lock::acquire(&path).unwrap();
        assert!(Lock::acquire(&path).is_err());
        drop(first);
        assert!(Lock::acquire(&path).is_ok());
    }

    #[test]
    fn status_updates_replace_complete_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic(&path, &json!({"state":"running"})).unwrap();
        atomic(&path, &json!({"state":"done"})).unwrap();
        assert_eq!(read_json(&path).unwrap()["state"], "done");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn headless_quota_classification_is_read_from_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("headless.json");
        atomic(&path, &json!({"error_classification":"quota_exhausted"})).unwrap();
        assert_eq!(failure_from_status(&path), Some(Failure::Quota));
    }
}
