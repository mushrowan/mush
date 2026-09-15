use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use mush_supervisor::inbox::{Binding, Inbox, TerminalReason};
use mush_supervisor::{Action, Failure, classify, decide};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

mod events;
mod follow_up;
mod poke;
mod prompts;
mod routing;
mod tasks;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    RegisterReview {
        config: PathBuf,
    },
    RouteReviews {
        directory: PathBuf,
        #[arg(long)]
        once: bool,
    },
    ReviewStatus {
        directory: PathBuf,
    },
    ReviewDisable {
        directory: PathBuf,
    },
    ReviewEnable {
        directory: PathBuf,
    },
    ReviewRetry {
        directory: PathBuf,
    },
    AdmissionStatus {
        directory: PathBuf,
    },
    AdmissionRelease {
        directory: PathBuf,
        claim: String,
        #[arg(long)]
        confirm_processes_exited: bool,
    },
    PreparedWorkspace {
        directory: PathBuf,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    RegisterInbox {
        path: PathBuf,
        #[arg(long)]
        session_id: String,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long, required = true)]
        worker_cwd: Vec<PathBuf>,
    },
    ReplayEvents {
        manifest: PathBuf,
    },
    Prepare(tasks::Prepare),
    FollowUp {
        directory: PathBuf,
        worker: String,
        #[arg(long)]
        prompt_file: PathBuf,
    },
    Run {
        manifest: PathBuf,
    },
    Status {
        directory: PathBuf,
    },
    Logs {
        directory: PathBuf,
        worker: String,
        #[arg(long, default_value_t = 8192)]
        bytes: u64,
    },
    Stop {
        directory: PathBuf,
    },
    Poke {
        directory: PathBuf,
        worker: String,
        prompt: String,
    },
    #[command(hide = true)]
    DeliverPoke {
        directory: PathBuf,
        message: PathBuf,
    },
}

#[derive(Clone, Deserialize, Serialize)]
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
    #[serde(default)]
    durable_events: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    orchestrator_inbox: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admission: Option<PathBuf>,
    #[serde(skip)]
    routing_guard: Option<PathBuf>,
    #[serde(skip)]
    admitted: Option<String>,
    #[serde(skip)]
    execution_locked: bool,
    #[serde(skip)]
    session_lease: Option<Arc<File>>,
    workers: Vec<Worker>,
}

#[derive(Clone, Deserialize, Serialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review_verdict: Option<PathBuf>,
}

fn pause_seconds() -> u64 {
    30
}

impl Manifest {
    fn journalled(&self) -> bool {
        self.durable_events
            || self.orchestrator_inbox.is_some()
            || self.workers.iter().any(|w| w.review_verdict.is_some())
    }
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
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("expected a regular file"));
    }
    Ok(file)
}

fn atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    mush_supervisor::inbox::replace(path, &serde_json::to_vec(value)?)?;
    Ok(())
}

fn read_json(path: &Path) -> Option<Value> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    serde_json::from_reader(file.take(3 * 1024 * 1024)).ok()
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
    group: u32,
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
        if fields.first() == Some(&"Z") {
            continue;
        }
        if let (Some(parent), Some(start)) = (
            fields.get(1).and_then(|s| s.parse().ok()),
            fields.get(19).and_then(|s| s.parse().ok()),
        ) {
            out.insert(
                pid,
                Process {
                    parent,
                    start,
                    group: fields.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
                },
            );
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
    follow_up::recover(&manifest.directory, &job.name)?;
    fs::create_dir_all(&dir)?;
    let data = dir.join("mush-data");
    fs::create_dir_all(&data)?;
    if manifest.admission.is_some() {
        use std::os::unix::fs::DirBuilderExt;
        let sessions = data.join("sessions");
        if !sessions.exists() {
            fs::DirBuilder::new().mode(0o700).create(&sessions)?;
        }
        mush_supervisor::inbox::private_directory(&sessions)?;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let discovery = PathBuf::from(home).join(".local/share/mush/discovered-models.json");
        if discovery.is_file() {
            fs::copy(discovery, data.join("discovered-models.json"))?;
        }
    }
    let state_path = dir.join("state.json");
    events::recover(&manifest, &job)?;
    let previous = read_json(&state_path);
    let explicit = poke::pending(&manifest.directory, &job.name)?;
    if explicit {
        poke::reopen(&manifest, &job, previous.as_ref()).await?;
    }
    let previous = if explicit {
        read_json(&state_path)
    } else {
        previous
    };
    if manifest.journalled()
        && events::has_terminal_event(&state_path)
        && previous
            .as_ref()
            .is_none_or(|v| v["state"] != "follow_up_pending")
    {
        return Ok(());
    }
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
    let assignment = events::assignment(previous.as_ref(), previous_round);
    let save = |state: &State<'_>, reason| events::save(&manifest, &job, assignment, state, reason);
    if manifest.journalled() && previous_state == Some("running") {
        let process = read_json(&dir.join("process.json"));
        if let Some(tracked) =
            process.and_then(|v| serde_json::from_value::<HashMap<u32, u64>>(v).ok())
        {
            signal_tree(&tracked, "-KILL").await;
        }
        save(
            &State {
                name: &job.name,
                state: "error",
                model: &model,
                round: previous_round,
                session_id: session.as_deref(),
                at: Utc::now(),
                detail: "supervisor interrupted during worker execution",
                retries,
                stalled,
            },
            Some(TerminalReason::SupervisorError),
        )?;
        return Ok(());
    }
    let next_round = previous_round
        .checked_add(1)
        .ok_or("worker round overflow")?;
    let max_rounds = if explicit {
        manifest.max_rounds.max(next_round)
    } else {
        manifest.max_rounds
    };
    for round in next_round..=max_rounds {
        last_round = round;
        let mut detail = String::new();
        if *stop.borrow() {
            break;
        }
        while !(explicit && round == next_round)
            && job.gate.as_ref().is_some_and(|p| !p.exists())
            && !*stop.borrow()
        {
            save(
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
                None,
            )?;
            sleep_or_stop(2, &mut stop).await;
        }
        if *stop.borrow() {
            break;
        }
        save(
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
            None,
        )?;
        let permit = tokio::select! {
            permit = permits.acquire() => permit?,
            _ = stop.changed() => break,
        };
        if *stop.borrow() {
            break;
        }
        let admission_id = mush_supervisor::review::digest(
            format!("{}:{}:{round}", manifest.directory.display(), job.name).as_bytes(),
        );
        let _execution = if manifest.execution_locked {
            None
        } else {
            manifest
                .admission
                .as_ref()
                .map(|pool| mush_supervisor::admission::execution_lock(pool, &job.cwd))
                .transpose()?
        };
        if let Some(pool) = &manifest.admission
            && manifest.admitted.as_ref() != Some(&admission_id)
        {
            loop {
                if mush_supervisor::admission::Pool::open(pool)?.claim(
                    &admission_id,
                    &manifest.directory,
                    &job.name,
                )? {
                    break;
                }
                sleep_or_stop(1, &mut stop).await;
                if *stop.borrow() {
                    return Ok(());
                }
            }
        }
        let base = fs::read_to_string(&job.prompt)?;
        let prompt = prompts::assignment(&base, round, &job.report, manifest.until);
        let inbox = manifest.directory.join("inbox").join(&job.name);
        let mut prompt = prompt;
        if manifest.journalled() {
            prompt.push_str(&format!("\n\nwrite an owner-only json report at {} with these exact identity fields: {}. set state to working, done, blocked or awaiting_input. done means this assignment is complete. blocked means a prerequisite is unavailable. awaiting_input means a decision is needed.", job.report.display(), json!({"run_id": events::run_id(&manifest)?, "worker_id":job.name, "assignment_id":assignment, "round":round})));
        }
        if let Some(follow_up) = follow_up::instructions(&dir, round)? {
            prompt.push_str("\n\nexplicit follow-up assignment. the previous completed task remains in history. work on this assignment within the existing permissions, then update your report:\n");
            prompt.push_str(&follow_up);
        }
        let mut messages = Vec::new();
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
                    messages.push(path);
                }
            }
        }
        let log_path = dir.join(format!("round-{round:03}.log"));
        let headless_status = dir.join(format!("round-{round:03}.json"));
        if manifest.journalled() && headless_status.exists() {
            fs::remove_file(&headless_status)?;
        }
        let log = private_file(&log_path)?;
        let mut cmd = Command::new(&manifest.executable);
        cmd.current_dir(&job.cwd)
            .env("MUSH_DATA_DIR", &data)
            .env_remove("MUSH_ORCHESTRATOR_INBOX")
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
        let session_lease = if let Some(lease) = &manifest.session_lease {
            Some(lease.clone())
        } else if manifest.admission.is_some()
            && let Some(session) = &session
        {
            let sessions = dir.join("mush-data/sessions");
            Some(Arc::new(mush_supervisor::admission::session_lock(
                &sessions, session, None,
            )?))
        } else {
            None
        };
        use std::os::fd::AsRawFd;
        if let Some(lease) = &session_lease {
            let fd = lease.as_raw_fd();
            cmd.env("MUSH_SESSION_LEASE_FD", fd.to_string());
            unsafe {
                cmd.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            cmd.env_remove("MUSH_SESSION_LEASE_FD");
        }
        cmd.arg("-p").arg(prompt);
        save(
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
            None,
        )?;
        if manifest.journalled() {
            cmd.env("MUSH_RUN_ID", events::run_id(&manifest)?)
                .env("MUSH_WORKER_ID", &job.name)
                .env("MUSH_ASSIGNMENT_ID", assignment.to_string())
                .env("MUSH_ROUND", round.to_string());
        }
        let launch_guard = manifest
            .routing_guard
            .as_ref()
            .map(|path| Lock::acquire(&path.join("transaction.lock")))
            .transpose()?;
        if *stop.borrow()
            || Utc::now() >= manifest.until
            || manifest.directory.join("STOP").exists()
            || manifest
                .routing_guard
                .as_ref()
                .is_some_and(|path| !routing::enabled(path))
        {
            if let Some(pool) = &manifest.admission {
                mush_supervisor::admission::Pool::open(pool)?.release(&admission_id)?;
            }
            break;
        }
        if let Some(directory) = &manifest.routing_guard {
            routing::launch_ready(directory)?;
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                save(
                    &State {
                        name: &job.name,
                        state: "error",
                        model: &model,
                        round,
                        session_id: session.as_deref(),
                        at: Utc::now(),
                        detail: &error.to_string(),
                        retries,
                        stalled,
                    },
                    Some(TerminalReason::SpawnFailed),
                )?;
                return Err(error.into());
            }
        };
        let pid = child.id().ok_or("missing child pid")?;
        for path in messages {
            fs::rename(&path, path.with_extension("delivered"))?;
        }
        let mut tracked = HashMap::new();
        descendants(pid, &mut tracked);
        if manifest.journalled() || manifest.admission.is_some() {
            atomic(&dir.join("process.json"), &tracked)?;
        }
        if let Some(pool) = &manifest.admission {
            mush_supervisor::admission::Pool::open(pool)?.started(&admission_id, pid, &tracked)?;
        }
        drop(launch_guard);
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
                    if manifest.journalled() {
                        atomic(&dir.join("process.json"), &tracked)?;
                    }
                    if let Some(pool) = &manifest.admission {
                        mush_supervisor::admission::Pool::open(pool)?.started(&admission_id, pid, &tracked)?;
                    }
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
        let survivors: HashMap<_, _> = processes()
            .into_iter()
            .filter(|(id, p)| p.group == pid || tracked.get(id) == Some(&p.start))
            .map(|(pid, p)| (pid, p.start))
            .collect();
        let child_survived = !survivors.is_empty() && exit.is_some();
        tracked.extend(survivors);
        signal_tree(&tracked, "-TERM").await;
        signal_tree(&tracked, "-KILL").await;
        let remaining = processes();
        if let Some(pool) = &manifest.admission
            && !tracked.is_empty()
            && !tracked
                .iter()
                .any(|(pid, start)| remaining.get(pid).is_some_and(|p| p.start == *start))
        {
            mush_supervisor::admission::Pool::open(pool)?.release(&admission_id)?;
        }
        drop(permit);
        if status_enabled {
            session = session_from_status(&headless_status).or(session);
        }
        if *stop.borrow() || Utc::now() >= manifest.until {
            break;
        }
        let report = (!manifest.journalled())
            .then(|| read_json(&job.report))
            .flatten();
        let checked_state = if manifest.journalled() {
            Some(events::report_state(&manifest, &job, assignment, round)?)
        } else {
            None
        };
        let reported_state = checked_state.as_deref().unwrap_or_else(|| {
            report
                .as_ref()
                .and_then(|r| r.get("state"))
                .and_then(Value::as_str)
                .unwrap_or("working")
        });
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
        let status_cancelled =
            read_json(&headless_status).is_some_and(|s| s["state"] == "cancelled");
        if manifest.journalled()
            && (child_survived
                || failure.is_none()
                    && read_json(&headless_status).is_none_or(|s| {
                        s["state"] != "completed"
                            || s["session_id"]
                                .as_str()
                                .is_none_or(|s| !mush_supervisor::inbox::identifier(s))
                    }))
        {
            failure = Some(Failure::Other);
        }
        if timed_out {
            stalled = stalled.saturating_add(1);
            detail = "worker exceeded its attempt or silence limit".into();
            failure = Some(if stalled < 2 {
                Failure::RateLimit
            } else {
                Failure::Other
            });
        }
        let action = if manifest.journalled()
            && (timed_out
                || status_cancelled
                || child_survived
                || failure.is_none() && reported_state == "awaiting_input")
        {
            Action::Stop
        } else {
            decide(reported_state, failure, fallback, retries)
        };
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
        let reason = (action == Action::Stop).then(|| {
            if timed_out {
                TerminalReason::Timeout
            } else if child_survived {
                TerminalReason::ChildSurvived
            } else if status_cancelled {
                TerminalReason::Cancelled
            } else if failure.is_some() {
                TerminalReason::Failed
            } else {
                events::saved_reason(state).unwrap_or(TerminalReason::Failed)
            }
        });
        save(
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
            reason,
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
    let reason = if Utc::now() >= manifest.until {
        TerminalReason::Deadline
    } else if *stop.borrow() {
        TerminalReason::Cancelled
    } else {
        TerminalReason::RoundLimit
    };
    save(
        &State {
            name: &job.name,
            state: "stopped",
            model: &model,
            round: last_round,
            session_id: session.as_deref(),
            at: Utc::now(),
            detail: match reason {
                TerminalReason::Deadline => "deadline",
                TerminalReason::Cancelled => "cancelled",
                _ => "round limit",
            },
            retries,
            stalled,
        },
        Some(reason),
    )?;
    Ok(())
}

async fn run(path: &Path) -> Result<()> {
    let manifest: Manifest = serde_json::from_reader(File::open(path)?)?;
    validate(&manifest)?;
    fs::create_dir_all(&manifest.directory)?;
    let lock = Lock::acquire(&manifest.directory.join("supervisor.lock"))?;
    supervise(manifest, lock, false).await
}

async fn supervise(manifest: Manifest, _lock: Lock, pending_only: bool) -> Result<()> {
    if manifest.directory.join("review-reservation.json").exists() {
        return Err("run has a review reservation".into());
    }
    events::initialise(&manifest)?;
    events::replay(&manifest)?;
    if manifest.directory.join("STOP").exists() {
        return Err("run is stopped".into());
    }
    atomic(
        &manifest.directory.join("supervisor.json"),
        &json!({"state":"running", "pid":std::process::id(), "until":manifest.until, "started_at":Utc::now()}),
    )?;
    let manifest = Arc::new(manifest);
    atomic(&manifest.directory.join("manifest.json"), manifest.as_ref())?;
    let permits = Arc::new(Semaphore::new(manifest.concurrency));
    let (cancel, receiver) = watch::channel(false);
    let mut jobs = JoinSet::new();
    let mut active = HashSet::new();
    let mut attempted = HashSet::new();
    for job in manifest.workers.clone() {
        let messages = poke::messages(&manifest.directory, &job.name)?;
        if !pending_only || !messages.is_empty() {
            attempted.extend(messages);
            active.insert(job.name.clone());
            spawn_worker(
                &mut jobs,
                manifest.clone(),
                job,
                permits.clone(),
                receiver.clone(),
            );
        }
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut errors = Vec::new();
    let mut delivery = JoinSet::new();
    let mut delivery_pending = false;
    loop {
        if !*receiver.borrow()
            && Utc::now() < manifest.until
            && !manifest.directory.join("STOP").exists()
        {
            for job in &manifest.workers {
                if active.contains(&job.name) {
                    continue;
                }
                let messages = poke::messages(&manifest.directory, &job.name)?;
                if messages.iter().any(|path| !attempted.contains(path)) {
                    attempted.extend(messages);
                    active.insert(job.name.clone());
                    spawn_worker(
                        &mut jobs,
                        manifest.clone(),
                        job.clone(),
                        permits.clone(),
                        receiver.clone(),
                    );
                }
            }
        }
        if jobs.is_empty() {
            break;
        }
        tokio::select! {
            result = jobs.join_next() => { match result {
                Some(Ok((name, result))) => {
                    active.remove(&name);
                    if let Err(error) = result {
                        attempted.extend(poke::messages(&manifest.directory, &name)?);
                        errors.push(json!({"worker":name,"error":error.to_string()}));
                    }
                },
                Some(Err(error)) => { errors.push(json!({"task_error":error.to_string()})); },
                None => {},
            }
                delivery_pending = manifest.orchestrator_inbox.is_some();
                if delivery_pending && delivery.is_empty() {
                    let manifest = Arc::clone(&manifest);
                    delivery.spawn_blocking(move || events::replay(&manifest));
                    delivery_pending = false;
                }
            },
            result = delivery.join_next(), if !delivery.is_empty() => {
                if let Some(result) = result {
                    match result {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => eprintln!("orchestrator events remain queued: {error}"),
                        Err(error) => eprintln!("orchestrator delivery interrupted: {error}"),
                    }
                }
            },
            _ = terminate.recv() => { cancel.send_replace(true); },
            _ = tokio::signal::ctrl_c() => { cancel.send_replace(true); },
            _ = tick.tick() => {
                if Utc::now() >= manifest.until || manifest.directory.join("STOP").exists() { cancel.send_replace(true); }
                if delivery_pending && delivery.is_empty() {
                    let manifest = Arc::clone(&manifest);
                    delivery.spawn_blocking(move || events::replay(&manifest));
                    delivery_pending = false;
                }
            },
        }
    }
    while let Some(result) = delivery.join_next().await {
        if let Err(error) = result? {
            eprintln!("orchestrator events remain queued: {error}");
        }
    }
    atomic(
        &manifest.directory.join("supervisor.json"),
        &json!({"state":"stopped", "until":manifest.until, "finished_at":Utc::now(), "errors":errors}),
    )?;
    events::replay(&manifest)?;
    Ok(())
}

fn spawn_worker(
    jobs: &mut JoinSet<(String, Result<()>)>,
    m: Arc<Manifest>,
    job: Worker,
    p: Arc<Semaphore>,
    s: watch::Receiver<bool>,
) {
    let name = job.name.clone();
    jobs.spawn(async move {
        let state_path = m.directory.join(&name).join("state.json");
        let result = worker(Arc::clone(&m), job.clone(), p, s).await;
        if let Err(error) = &result {
            let mut state = read_json(&state_path).unwrap_or_else(|| json!({"name":name}));
            state["state"] = json!("error");
            state["detail"] = json!(error.to_string());
            state["at"] = json!(Utc::now());
            if !events::has_terminal_event(&state_path) {
                let saved = if m.journalled() {
                    let round = state["round"]
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .unwrap_or(1);
                    events::save(
                        &m,
                        &job,
                        events::assignment(Some(&state), round),
                        &State {
                            name: &name,
                            state: "error",
                            model: state["model"].as_str().unwrap_or(&m.model),
                            round,
                            session_id: state["session_id"].as_str(),
                            at: Utc::now(),
                            detail: "supervisor error",
                            retries: 0,
                            stalled: 0,
                        },
                        Some(TerminalReason::SupervisorError),
                    )
                } else {
                    atomic(&state_path, &state)
                };
                if let Err(status_error) = saved {
                    eprintln!("recording worker failure: {status_error}");
                }
            }
        }
        (name, result)
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Operation::RegisterReview { config } => routing::register(&config),
        Operation::RouteReviews { directory, once } => routing::run(&directory, once).await,
        Operation::ReviewStatus { directory } => routing::status(&directory),
        Operation::ReviewDisable { directory } => routing::disable(&directory),
        Operation::ReviewEnable { directory } => routing::enable(&directory),
        Operation::ReviewRetry { directory } => routing::retry(&directory),
        Operation::AdmissionStatus { directory } => {
            println!(
                "{}",
                mush_supervisor::admission::Pool::open(&directory)?.status()?
            );
            Ok(())
        }
        Operation::AdmissionRelease {
            directory,
            claim,
            confirm_processes_exited,
        } => {
            if !confirm_processes_exited {
                return Err("confirm process cleanup before releasing capacity".into());
            }
            let saved = mush_supervisor::admission::Pool::open(&directory)?
                .claims()
                .get(&claim)
                .cloned()
                .ok_or("unknown admission claim")?;
            let _run = Lock::acquire(&saved.run.join("supervisor.lock"))?;
            let mut pool = mush_supervisor::admission::Pool::open(&directory)?;
            let saved = pool.claims().get(&claim).ok_or("unknown admission claim")?;
            let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
            let live = processes();
            if saved.boot_id == boot.trim()
                && saved
                    .processes
                    .iter()
                    .any(|(pid, start)| live.get(pid).is_some_and(|p| p.start == *start))
            {
                return Err("claimed processes are still alive".into());
            }
            pool.release(&claim)?;
            Ok(())
        }
        Operation::PreparedWorkspace { directory, report } => {
            println!(
                "{}",
                routing::workspace_digest(&directory, report.as_deref())?
            );
            Ok(())
        }
        Operation::RegisterInbox {
            path,
            session_id,
            cwd,
            worker_cwd,
        } => {
            Inbox::register(
                &path,
                Binding {
                    session_id,
                    cwd,
                    workers: worker_cwd,
                },
            )?;
            Ok(())
        }
        Operation::ReplayEvents { manifest } => {
            let manifest: Manifest = serde_json::from_reader(File::open(manifest)?)?;
            let _lock = Lock::acquire(&manifest.directory.join("supervisor.lock"))?;
            events::replay(&manifest)
        }
        Operation::Prepare(args) => tasks::prepare(args).await,
        Operation::FollowUp {
            directory,
            worker,
            prompt_file,
        } => follow_up::queue(&directory, &worker, &prompt_file),
        Operation::Logs {
            directory,
            worker,
            bytes,
        } => tasks::logs(&directory, &worker, bytes),
        Operation::Run { manifest } => run(&manifest).await,
        Operation::Stop { directory } => {
            private_file(&directory.join("STOP"))?;
            Ok(())
        }
        Operation::Poke {
            directory,
            worker,
            prompt,
        } => poke::queue(&directory, &worker, &prompt),
        Operation::DeliverPoke { directory, message } => poke::deliver(&directory, &message).await,
        Operation::Status { directory } => tasks::status(&directory),
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
