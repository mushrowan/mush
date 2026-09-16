use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mush_supervisor::inbox::{
    Binding, Event, Inbox, TerminalReason, identifier, private_directory, replace,
};
use mush_supervisor::review::{
    Identity, Notice, Verdict, VerdictResult, commit, decode, digest, hash,
};
use rustix::fs::{Mode, OFlags, openat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, watch};

use crate::{Lock, Manifest, Result, atomic, follow_up, validate, worker};
mod lineage;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Actions {
    resume_rejected: bool,
    return_to_parent: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Producer {
    manifest: PathBuf,
    manifest_sha256: String,
    executable_sha256: String,
    identity: Identity,
    state_sha256: String,
    report_sha256: String,
    checkpoint_sha256: String,
    session_sha256: String,
    prompt_sha256: String,
    head: String,
    workspace_sha256: String,
    correction: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Parent {
    inbox: PathBuf,
    binding: Binding,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    id: String,
    path: PathBuf,
    sha256: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    task: String,
    #[serde(default)]
    predecessor: Option<String>,
    directory: PathBuf,
    assignment: String,
    generation: u32,
    candidate: String,
    reviewer: Identity,
    reviewer_directory: PathBuf,
    reviewer_cwd: PathBuf,
    required_checks: Vec<String>,
    artifacts: Vec<Artifact>,
    regression_commit: Option<String>,
    until: DateTime<Utc>,
    #[serde(default)]
    actions: Actions,
    producer: Producer,
    parent: Option<Parent>,
    admission: PathBuf,
    max_corrections: u32,
    authority_file: PathBuf,
    #[serde(default)]
    rereview: Option<Producer>,
    #[serde(default)]
    continuation_sha256: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    assignment: String,
    generation: u32,
    enabled: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    config_sha256: String,
    sequence: u32,
    producer: String,
    parent: String,
    detail: String,
    capture: Option<String>,
    launch_claimed: bool,
    #[serde(default)]
    review_launch_claimed: bool,
    #[serde(default)]
    reviewer_action: String,
    at: DateTime<Utc>,
}

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if !path.is_absolute() {
        return Err("expected absolute path".into());
    }
    let mut fd = File::open("/")?;
    let components: Vec<_> = path.components().collect();
    use std::os::unix::fs::MetadataExt;
    for (index, component) in components.iter().enumerate() {
        if let std::path::Component::RootDir = component {
            continue;
        }
        let std::path::Component::Normal(name) = component else {
            return Err("unsafe review path".into());
        };
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if index + 1 < components.len() {
            flags |= OFlags::DIRECTORY;
        }
        fd = File::from(openat(&fd, *name, flags, Mode::empty()).map_err(std::io::Error::from)?);
        let meta = fd.metadata()?;
        let uid = rustix::process::geteuid().as_raw();
        if (meta.uid() != uid && meta.uid() != 0)
            || (meta.mode() & 0o022 != 0
                && !(meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o1000 != 0))
        {
            return Err("unsafe review file ownership".into());
        }
    }
    let before = fd.metadata()?;
    if !before.is_file() || before.nlink() != 1 || before.len() > limit {
        return Err("unsafe or oversized review file".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut fd)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    let after = fd.metadata()?;
    if bytes.len() as u64 > limit
        || before.len() != after.len()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err("review file changed during read".into());
    }
    Ok(bytes)
}

fn load(directory: &Path) -> Result<Config> {
    private_directory(directory)?;
    let bytes = read(&directory.join("config.json"), 256 * 1024)?;
    let config: Config = decode(&bytes)?;
    if config.directory != directory {
        return Err("routing directory mismatch".into());
    }
    if directory.join("state.json").exists() && state(directory)?.config_sha256 != digest(&bytes) {
        return Err("registered review configuration changed".into());
    }
    Ok(config)
}

fn state(directory: &Path) -> Result<State> {
    Ok(serde_json::from_slice(&read(
        &directory.join("state.json"),
        64 * 1024,
    )?)?)
}

fn save(directory: &Path, state: &mut State) -> Result<()> {
    state.sequence = state
        .sequence
        .checked_add(1)
        .ok_or("routing history exhausted")?;
    if state.sequence > 4096 {
        return Err("routing history exhausted".into());
    }
    state.at = Utc::now();
    let path = directory
        .join("history")
        .join(format!("{:04}.json", state.sequence));
    if path.exists() {
        return Err("routing journal collision".into());
    }
    atomic(&path, state)?;
    atomic(&directory.join("state.json"), state)
}

fn recover(directory: &Path) -> Result<()> {
    let mut paths = fs::read_dir(directory.join("history"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if paths.len() > 4096 {
        return Err("routing history exhausted".into());
    }
    paths.sort();
    if let Some(path) = paths.last() {
        let saved: State = serde_json::from_slice(&read(path, 64 * 1024)?)?;
        atomic(&directory.join("state.json"), &saved)?;
    }
    Ok(())
}

pub fn register(path: &Path) -> Result<()> {
    let config: Config = decode(&read(path, 256 * 1024)?)?;
    register_config(config)
}

pub fn continue_review(directory: &Path, path: &Path) -> Result<()> {
    register_config(lineage::continuation(directory, path)?)
}

pub fn pair_status(admission: &Path, task: &str) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&lineage::Registry::view(admission)?.inspect(task)?)?
    );
    Ok(())
}

pub fn reassign(admission: &Path, path: &Path) -> Result<()> {
    if path.parent() != Some(admission.join("reassignments").as_path()) {
        return Err("reassignment must be an operator request in admission/reassignments".into());
    }
    let request = decode(&read(path, 256 * 1024)?)?;
    lineage::Registry::open(admission)?.reassign(&request)
}

fn register_config(config: Config) -> Result<()> {
    if config.version != 1
        || !identifier(&config.assignment)
        || config.assignment.len() > 80
        || config.generation == 0
        || !commit(&config.candidate)
        || !config.reviewer.valid()
        || !config.producer.identity.valid()
        || config.until <= Utc::now()
        || config.until > Utc::now() + chrono::Duration::hours(24)
        || config.max_corrections > 1
        || config.actions.resume_rejected && config.max_corrections != 1
        || config.actions.return_to_parent && config.parent.is_none()
        || config.required_checks.len() > 64
        || config.artifacts.len() > 64
        || config.producer.correction.is_empty()
        || config.producer.correction.len() > 64 * 1024
        || config
            .regression_commit
            .as_ref()
            .is_some_and(|s| !commit(s))
    {
        return Err("invalid review assignment".into());
    }
    let mut ids = HashSet::new();
    if config
        .required_checks
        .iter()
        .any(|id| !identifier(id) || !ids.insert(id))
    {
        return Err("invalid required checks".into());
    }
    ids.clear();
    if config.artifacts.iter().any(|a| {
        !identifier(&a.id)
            || !ids.insert(&a.id)
            || !a.path.is_absolute()
            || a.sha256.as_ref().is_some_and(|s| !hash(s))
    }) {
        return Err("invalid artifact binding".into());
    }
    for value in [
        &config.producer.manifest_sha256,
        &config.producer.executable_sha256,
        &config.producer.state_sha256,
        &config.producer.report_sha256,
        &config.producer.session_sha256,
        &config.producer.checkpoint_sha256,
        &config.producer.prompt_sha256,
        &config.producer.workspace_sha256,
    ] {
        if !hash(value) {
            return Err("invalid prepared digest".into());
        }
    }
    if !commit(&config.producer.head) {
        return Err("invalid prepared head".into());
    }
    let mut lineage = lineage::Registry::open(&config.admission)?;
    lineage.check(&config)?;
    if config.directory.exists() {
        let _owner = Lock::acquire(&config.directory.join("router.lock"))?;
        recover(&config.directory)?;
        let existing = load(&config.directory)?;
        if serde_json::to_vec(&existing)? != serde_json::to_vec(&config)? {
            return Err("review registration differs from existing generation".into());
        }
        state(&config.directory)?;
        return lineage.record(&config);
    }
    if stopped(&config) {
        return Err("review authority is missing, revoked or superseded".into());
    }
    if config.actions.resume_rejected {
        let manifest = producer_manifest(&config)?;
        let _run = Lock::acquire(&manifest.directory.join("supervisor.lock"))?;
        let _session = mush_supervisor::admission::session_lock(
            &manifest
                .directory
                .join(&config.producer.identity.worker)
                .join("mush-data/sessions"),
            &config.producer.identity.session,
            None,
        )?;
        prepared(&config, &manifest)?;
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&config.directory)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(config.directory.join("history"))?;
    atomic(&config.directory.join("config.json"), &config)?;
    let mut initial = State {
        version: 1,
        config_sha256: digest(&read(&config.directory.join("config.json"), 256 * 1024)?),
        sequence: 0,
        producer: "waiting_review".into(),
        parent: "pending".into(),
        detail: String::new(),
        capture: None,
        launch_claimed: false,
        review_launch_claimed: false,
        reviewer_action: if config.rereview.is_some() {
            "queued"
        } else {
            "external"
        }
        .into(),
        at: Utc::now(),
    };
    if !config.actions.return_to_parent {
        initial.parent = "disabled".into();
    }
    save(&config.directory, &mut initial)?;
    lineage.record(&config)?;
    Ok(())
}

fn producer_manifest(config: &Config) -> Result<Manifest> {
    let bytes = read(&config.producer.manifest, 1024 * 1024)?;
    if digest(&bytes) != config.producer.manifest_sha256 {
        return Err("producer manifest changed".into());
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    validate(&manifest)?;
    if fs::canonicalize(&manifest.directory)? != manifest.directory
        || read(&manifest.directory.join("manifest.json"), 1024 * 1024)? != bytes
    {
        return Err("producer run manifest changed".into());
    }
    if manifest.admission.as_ref() != Some(&config.admission)
        || !manifest.status_file
        || !manifest.journalled()
        || !manifest.executable.is_absolute()
    {
        return Err("producer requires shared admission and saved sessions".into());
    }
    if digest(&read(&manifest.executable, 512 * 1024 * 1024)?) != config.producer.executable_sha256
    {
        return Err("producer executable changed".into());
    }
    Ok(manifest)
}

pub fn workspace_digest(directory: &Path, report: Option<&Path>) -> Result<String> {
    fn walk(
        root: &Path,
        path: &Path,
        bytes: &mut Vec<u8>,
        files: &mut usize,
        total: &mut u64,
        report: Option<&Path>,
    ) -> Result<()> {
        let mut entries = fs::read_dir(path)?
            .take(4097)
            .collect::<std::io::Result<Vec<_>>>()?;
        if entries.len() > 4096 {
            return Err("workspace digest exceeds limit".into());
        }
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if report == Some(path.as_path()) {
                continue;
            }
            let rel = path.strip_prefix(root)?;
            if rel == Path::new(".jj") || rel == Path::new(".git") {
                continue;
            }
            *files += 1;
            if *files > 4096 || bytes.len() > 64 * 1024 * 1024 {
                return Err("workspace digest exceeds limit".into());
            }
            let meta = path.symlink_metadata()?;
            use std::os::unix::fs::MetadataExt;
            bytes.extend_from_slice(rel.as_os_str().as_encoded_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&meta.mode().to_le_bytes());
            if meta.is_dir() {
                walk(root, &path, bytes, files, total, report)?;
            } else if meta.is_file() {
                *total = total.saturating_add(meta.len());
                if *total > 64 * 1024 * 1024 {
                    return Err("workspace digest exceeds limit".into());
                }
                bytes.extend_from_slice(digest(&read(&path, 16 * 1024 * 1024)?).as_bytes());
            } else if meta.is_symlink() {
                bytes.extend_from_slice(fs::read_link(&path)?.as_os_str().as_encoded_bytes());
            } else {
                return Err("workspace contains an unsupported file".into());
            }
            bytes.push(0);
        }
        Ok(())
    }
    if fs::canonicalize(directory)? != directory {
        return Err("workspace path is not canonical".into());
    }
    let mut bytes = Vec::new();
    if report.is_some_and(|path| {
        !path.starts_with(directory)
            || path == directory
            || path.components().any(|c| {
                !matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
    }) {
        return Err("report must be an absolute workspace file".into());
    }
    walk(directory, directory, &mut bytes, &mut 0, &mut 0, report)?;
    Ok(digest(&bytes))
}

fn prepared(config: &Config, manifest: &Manifest) -> Result<()> {
    let id = &config.producer.identity;
    let job = manifest
        .workers
        .iter()
        .find(|w| w.name == id.worker)
        .ok_or("producer worker missing")?;
    if job.repeat_done || job.gate.is_some() {
        return Err("review correction requires an ungated terminal worker".into());
    }
    let dir = manifest.directory.join(&id.worker);
    if manifest.directory.join("STOP").exists() {
        return Err("producer is stopped".into());
    }
    let checks = [
        (
            dir.join("state.json"),
            &config.producer.state_sha256,
            1024 * 1024,
        ),
        (
            job.report.clone(),
            &config.producer.report_sha256,
            1024 * 1024,
        ),
        (
            dir.join(format!("round-{:03}.json", id.round)),
            &config.producer.checkpoint_sha256,
            1024 * 1024,
        ),
        (
            dir.join("mush-data/sessions")
                .join(format!("{}.json", id.session)),
            &config.producer.session_sha256,
            64 * 1024 * 1024,
        ),
        (
            job.prompt.clone(),
            &config.producer.prompt_sha256,
            128 * 1024,
        ),
    ];
    for (path, expected, limit) in checks {
        if digest(&read(&path, limit)?) != *expected {
            return Err("prepared producer state changed".into());
        }
    }
    let saved: Value = serde_json::from_slice(&read(&dir.join("state.json"), 1024 * 1024)?)?;
    let event: Event = serde_json::from_value(saved["terminal_event"].clone())?;
    event.validate()?;
    if event.run_id != id.run_id
        || event.worker_id != id.worker
        || event.assignment_id != id.assignment
        || event.round != id.round
        || event.session_id.as_ref() != Some(&id.session)
        || event.reason != TerminalReason::Completed
    {
        return Err("producer assignment changed".into());
    }
    let checkpoint: Value = serde_json::from_slice(&read(
        &dir.join(format!("round-{:03}.json", id.round)),
        1024 * 1024,
    )?)?;
    if saved["state"] != "done"
        || saved["round"] != id.round
        || checkpoint["session_id"] != id.session
        || checkpoint["state"] != "completed"
    {
        return Err("producer is not completed in the bound session".into());
    }
    if workspace_digest(&job.cwd, Some(&job.report))? != config.producer.workspace_sha256 {
        return Err("producer workspace changed".into());
    }
    let output = std::process::Command::new("jj")
        .current_dir(&job.cwd)
        .args([
            "--ignore-working-copy",
            "--color=never",
            "--no-pager",
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "commit_id",
        ])
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() || output.stdout != config.producer.head.as_bytes() {
        return Err("producer revision changed".into());
    }
    let conflict = std::process::Command::new("jj")
        .current_dir(&job.cwd)
        .args([
            "--ignore-working-copy",
            "--color=never",
            "--no-pager",
            "log",
            "--no-graph",
            "-r",
            "conflicts() & @",
            "-T",
            "commit_id",
        ])
        .stdin(Stdio::null())
        .output()?;
    if !conflict.status.success() || !conflict.stdout.is_empty() {
        return Err("producer has conflicts".into());
    }
    if let Some(regression) = &config.regression_commit {
        let ancestry = std::process::Command::new("jj")
            .current_dir(&job.cwd)
            .args([
                "--ignore-working-copy",
                "--color=never",
                "--no-pager",
                "log",
                "--no-graph",
                "-r",
                &format!("{} & ::{}", config.candidate, regression),
                "-T",
                "commit_id",
            ])
            .stdin(Stdio::null())
            .output()?;
        if !ancestry.status.success() || ancestry.stdout != config.candidate.as_bytes() {
            return Err("regression revision does not descend from the candidate".into());
        }
    }
    Ok(())
}

fn capture(config: &Config) -> Result<(Verdict, String)> {
    let source = config
        .reviewer_directory
        .join(&config.reviewer.worker)
        .join("event-reports")
        .join(format!("assignment-{:04}.json", config.reviewer.assignment));
    let destination = config.directory.join("capture");
    let committed = destination.join("terminal.json");
    let bytes = if committed.exists() {
        let captured = read(&committed, 3 * 1024 * 1024)?;
        match read(&source, 3 * 1024 * 1024) {
            Ok(current) => {
                let old: Value = serde_json::from_slice(&captured)?;
                let new: Value = serde_json::from_slice(&current)?;
                if old["review"] != new["review"]
                    || old["state"]["terminal_event"] != new["state"]["terminal_event"]
                {
                    return Err("review bytes changed after capture".into());
                }
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        captured
    } else {
        read(&source, 3 * 1024 * 1024)?
    };
    let journal: Value = decode(&bytes)?;
    let event: Event = serde_json::from_value(journal["state"]["terminal_event"].clone())?;
    event.validate()?;
    let id = &config.reviewer;
    if event.run_id != id.run_id
        || event.worker_id != id.worker
        || event.assignment_id != id.assignment
        || event.round != id.round
        || event.session_id.as_ref() != Some(&id.session)
        || !matches!(
            event.reason,
            TerminalReason::Completed | TerminalReason::Blocked | TerminalReason::AwaitingInput
        )
    {
        return Err("review terminal identity or outcome mismatch".into());
    }
    let raw: Vec<u8> = serde_json::from_value(journal["review"]["bytes"].clone())?;
    if raw.len() > 256 * 1024 {
        return Err("review verdict exceeds limit".into());
    }
    let sha = digest(&raw);
    if journal["review"]["sha256"] != sha {
        return Err("review capture digest mismatch".into());
    }
    let verdict: Verdict = decode(&raw)?;
    verdict.validate()?;
    if verdict.task != config.task
        || verdict.assignment != config.assignment
        || verdict.generation != config.generation
        || verdict.candidate != config.candidate
        || verdict.reviewer != config.reviewer
        || verdict.regression_commit != config.regression_commit
    {
        return Err("stale review assignment".into());
    }
    if verdict.complete
        && (config.required_checks.len() != verdict.checks.len()
            || config
                .required_checks
                .iter()
                .any(|s| !verdict.checks.contains(s)))
    {
        return Err("review scope is incomplete".into());
    }
    if verdict.result == VerdictResult::Rejected && event.reason != TerminalReason::Completed {
        return Err("review did not complete".into());
    }
    if destination.exists() {
        if read(&destination.join("verdict.json"), 256 * 1024)? != raw {
            return Err("review bytes changed after capture".into());
        }
        for evidence in &verdict.evidence {
            if digest(&read(
                &destination.join(format!("{}.evidence", evidence.id)),
                1024 * 1024,
            )?) != evidence.sha256
            {
                return Err("captured artifact changed".into());
            }
        }
        return Ok((verdict, sha));
    }
    let pending = config.directory.join("capture-pending");
    fs::DirBuilder::new().mode(0o700).create(&pending)?;
    replace(&pending.join("verdict.json"), &raw)?;
    atomic(&pending.join("terminal.json"), &journal)?;
    let mut total = 0;
    for evidence in &verdict.evidence {
        let binding = config
            .artifacts
            .iter()
            .find(|a| a.id == evidence.id)
            .ok_or("unbound review artifact")?;
        let bytes = read(&binding.path, 1024 * 1024)?;
        total += bytes.len();
        if total > 8 * 1024 * 1024
            || digest(&bytes) != evidence.sha256
            || binding
                .sha256
                .as_ref()
                .is_some_and(|s| s != &evidence.sha256)
        {
            return Err("review artifact changed or exceeds limit".into());
        }
        replace(&pending.join(format!("{}.evidence", evidence.id)), &bytes)?;
    }
    File::open(&pending)?.sync_all()?;
    fs::rename(pending, &destination)?;
    File::open(&config.directory)?.sync_all()?;
    Ok((verdict, sha))
}

fn notify(
    config: &Config,
    state: &mut State,
    verdict: &Verdict,
    sha: &str,
    number: u32,
) -> Result<()> {
    if !config.actions.return_to_parent {
        return Ok(());
    }
    let parent = config.parent.as_ref().ok_or("parent binding missing")?;
    let inbox = Inbox::open(&parent.inbox)?;
    if inbox.binding()? != parent.binding {
        return Err("parent binding changed".into());
    }
    let run_id = format!("review-{}-{}", config.task, config.generation);
    let event_id = format!("{run_id}.{}.{number}", config.reviewer.worker);
    let stored = config.directory.join(format!("parent-{number}.json"));
    let event = if stored.exists() {
        serde_json::from_slice(&read(&stored, 512 * 1024)?)?
    } else {
        let event = Event {
            event_id: event_id.clone(),
            run_id,
            worker_id: config.reviewer.worker.clone(),
            assignment_id: number,
            round: number,
            session_id: Some(config.reviewer.session.clone()),
            reason: TerminalReason::Completed,
            report_path: config.directory.join("capture/verdict.json"),
            review: Some(Notice {
                verdict: verdict.clone(),
                capture_sha256: sha.into(),
                producer_action: if state.producer == "attempt_finished" {
                    format!("{}: {}", state.producer, state.detail)
                } else {
                    state.producer.clone()
                },
                evidence_directory: config.directory.join("capture"),
                pairing: Some(mush_supervisor::review::Pairing {
                    task: config.task.clone(),
                    pair: lineage::Registry::view(&config.admission)?
                        .historical_pair(&config.task, config.generation)?,
                }),
            }),
        };
        atomic(&stored, &event)?;
        event
    };
    inbox.enqueue(event, &config.reviewer_cwd)?;
    let mut acknowledged = true;
    for stage in 1..=3 {
        let path = config.directory.join(format!("parent-{stage}.json"));
        if path.exists() {
            let event: Event = decode(&read(&path, 512 * 1024)?)?;
            acknowledged &= inbox.acknowledged(&event.event_id)?;
        }
    }
    state.parent = if acknowledged {
        "acknowledged"
    } else {
        "queued"
    }
    .into();
    Ok(())
}

async fn launch(
    config: &Config,
    state: &mut State,
    verdict: Option<&Verdict>,
    sha: &str,
    reviewer: bool,
) -> Result<()> {
    let original = config;
    let selected = selected(config, reviewer)?;
    let config = &selected;
    let mut manifest = producer_manifest(config)?;
    let _run = Lock::acquire(&manifest.directory.join("supervisor.lock"))?;
    let job = manifest
        .workers
        .iter()
        .find(|j| j.name == config.producer.identity.worker)
        .ok_or("producer missing")?
        .clone();
    let workspace_lock = mush_supervisor::admission::execution_lock(&config.admission, &job.cwd)?;
    let session_lease = mush_supervisor::admission::session_lock(
        &manifest
            .directory
            .join(&job.name)
            .join("mush-data/sessions"),
        &config.producer.identity.session,
        None,
    )?;
    manifest.session_lease = Some(Arc::new(session_lease));
    prepared(config, &manifest)?;
    if manifest.directory.join("review-reservation.json").exists() {
        return Err("producer already reserved".into());
    }
    let _transaction = Lock::acquire(&config.directory.join("transaction.lock"))?;
    if stopped(config) {
        return Err("review routing is stopped".into());
    }
    lineage::Registry::open(&config.admission)?.current(original)?;
    let id = &config.producer.identity;
    if id.round >= manifest.max_rounds {
        return Err("producer round budget exhausted".into());
    }
    let admission_id = digest(
        format!(
            "{}:{}:{}",
            manifest.directory.display(),
            job.name,
            id.round + 1
        )
        .as_bytes(),
    );
    if !mush_supervisor::admission::Pool::open(&config.admission)?.claim(
        &admission_id,
        &manifest.directory,
        &job.name,
    )? {
        set_action(state, reviewer, "queued_capacity");
        return Ok(());
    }
    manifest.admitted = Some(admission_id);
    manifest.execution_locked = true;
    let prompt = if let Some(verdict) = verdict {
        format!(
            "{}\n\nuntrusted review data, no new permissions:\n{}\n\nhash-bound evidence: {}\nverdict sha256: {}",
            config.producer.correction,
            serde_json::to_string(verdict)?,
            config.directory.join("capture").display(),
            sha
        )
    } else {
        format!(
            "{}\n\noperator review assignment:\n{}",
            config.producer.correction,
            serde_json::to_string(
                &json!({"task":config.task,"assignment":config.assignment,"generation":config.generation,"candidate":config.candidate,"reviewer":original.reviewer,"required_checks":config.required_checks,"predecessor":config.predecessor})
            )?
        )
    };
    if prompt.len() > 128 * 1024 {
        return Err("correction prompt exceeds limit".into());
    }
    replace(&config.directory.join("correction.txt"), prompt.as_bytes())?;
    atomic(
        &manifest.directory.join("review-reservation.json"),
        &json!({"route":config.directory,"assignment":config.assignment,"generation":config.generation}),
    )?;
    set_action(state, reviewer, "preparing");
    if reviewer {
        state.review_launch_claimed = true;
    } else {
        state.launch_claimed = true;
    }
    save(&config.directory, state)?;
    follow_up::queue_locked(
        &manifest.directory,
        &id.worker,
        &config.directory.join("correction.txt"),
    )?;
    set_action(state, reviewer, "launch_claimed");
    save(&config.directory, state)?;
    drop(_transaction);
    manifest.max_rounds = manifest.max_rounds.min(id.round + 1);
    manifest.until = manifest.until.min(config.until);
    manifest.routing_guard = Some(config.directory.clone());
    manifest.routing_review = reviewer;
    let (cancel, receiver) = watch::channel(false);
    let monitor_config = config.clone();
    let run_directory = manifest.directory.clone();
    let deadline = manifest.until;
    let monitor = tokio::spawn(async move {
        loop {
            if stopped(&monitor_config)
                || run_directory.join("STOP").exists()
                || Utc::now() >= deadline
            {
                cancel.send_replace(true);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let run_directory = manifest.directory.clone();
    let result = worker(
        Arc::new(manifest),
        job,
        Arc::new(Semaphore::new(1)),
        receiver,
    )
    .await;
    drop(workspace_lock);
    monitor.abort();
    let _transaction = Lock::acquire(&config.directory.join("transaction.lock"))?;
    set_action(
        state,
        reviewer,
        if result.is_ok() {
            "attempt_finished"
        } else {
            "needs_input_launch"
        },
    );
    let producer_state = read(
        &run_directory.join(&id.worker).join("state.json"),
        1024 * 1024,
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    state.detail = producer_state
        .as_ref()
        .and_then(|state| state["state"].as_str())
        .unwrap_or("producer state unavailable")
        .into();
    save(&config.directory, state)?;
    if result.is_ok() {
        fs::remove_file(run_directory.join("review-reservation.json"))?;
        File::open(&run_directory)?.sync_all()?;
    }
    result
}

pub fn enabled(directory: &Path) -> bool {
    load(directory).is_ok_and(|config| !stopped(&config) && lineage::active(&config).is_ok())
}

fn selected(config: &Config, reviewer: bool) -> Result<Config> {
    let mut selected = config.clone();
    if reviewer {
        selected.producer = config
            .rereview
            .clone()
            .ok_or("review preparation missing")?;
        let manifest = producer_manifest(&selected)?;
        if manifest.directory != config.reviewer_directory
            || !manifest
                .workers
                .iter()
                .any(|w| w.name == config.reviewer.worker && w.cwd == config.reviewer_cwd)
        {
            return Err("reviewer preparation changed paired workspace".into());
        }
    }
    Ok(selected)
}

fn set_action(state: &mut State, reviewer: bool, value: &str) {
    if reviewer {
        state.reviewer_action = value.into();
    } else {
        state.producer = value.into();
    }
}

pub fn launch_ready(directory: &Path, reviewer: bool) -> Result<()> {
    let original = load(directory)?;
    lineage::Registry::open(&original.admission)?.current(&original)?;
    let config = selected(&original, reviewer)?;
    if stopped(&config) || Utc::now() >= config.until {
        return Err("review launch was revoked or expired".into());
    }
    let manifest = producer_manifest(&config)?;
    let job = manifest
        .workers
        .iter()
        .find(|job| job.name == config.producer.identity.worker)
        .ok_or("producer worker missing")?;
    if workspace_digest(&job.cwd, Some(&job.report))? != config.producer.workspace_sha256
        || digest(&read(&job.prompt, 128 * 1024)?) != config.producer.prompt_sha256
        || digest(&read(
            &manifest
                .directory
                .join(&job.name)
                .join("mush-data/sessions")
                .join(format!("{}.json", config.producer.identity.session)),
            64 * 1024 * 1024,
        )?) != config.producer.session_sha256
    {
        return Err("prepared launch state changed".into());
    }
    let archive = manifest
        .directory
        .join(&job.name)
        .join("followups")
        .join(format!("after-{:03}", config.producer.identity.round));
    let expected: Value = decode(&read(&archive.join("next-report.json"), 1024 * 1024)?)?;
    let report: Value = decode(&read(&job.report, 1024 * 1024)?)?;
    if expected != report {
        return Err("prepared follow-up report changed".into());
    }
    let head = std::process::Command::new("jj")
        .current_dir(&job.cwd)
        .args([
            "--ignore-working-copy",
            "--color=never",
            "--no-pager",
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "commit_id",
        ])
        .stdin(Stdio::null())
        .output()?;
    if !head.status.success() || head.stdout != config.producer.head.as_bytes() {
        return Err("producer revision changed before launch".into());
    }
    Ok(())
}

fn stopped(config: &Config) -> bool {
    let authority = read(&config.authority_file, 4096)
        .ok()
        .and_then(|b| serde_json::from_slice::<Authority>(&b).ok());
    config.directory.join("DISABLED").exists()
        || config.directory.join("STOP").exists()
        || authority.is_none_or(|a| {
            !a.enabled || a.assignment != config.assignment || a.generation != config.generation
        })
}

pub async fn run(directory: &Path, once: bool) -> Result<()> {
    let mut directory = directory.to_owned();
    loop {
        let config = load(&directory)?;
        if directory.join("STOP").exists() || directory.join("DISABLED").exists() {
            return Ok(());
        }
        let latest = lineage::Registry::view(&config.admission)?.latest(&config.task)?;
        if latest != directory {
            directory = latest;
        }
        match run_generation(&directory, once).await? {
            Some(next) => directory = next,
            None => return Ok(()),
        }
    }
}

fn historical_delivery(config: &Config) -> Result<()> {
    let directories = lineage::Registry::view(&config.admission)?.generations(&config.task)?;
    for directory in directories {
        if directory == config.directory {
            continue;
        }
        let _lock = match Lock::acquire(&directory.join("router.lock")) {
            Ok(lock) => lock,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let prior = load(&directory)?;
        if !prior.actions.return_to_parent
            || directory.join("STOP").exists()
            || directory.join("DISABLED").exists()
        {
            continue;
        }
        let mut saved = state(&directory)?;
        if saved.capture.is_none() {
            continue;
        }
        let bytes = read(&directory.join("capture/verdict.json"), 256 * 1024)?;
        let verdict: Verdict = decode(&bytes)?;
        let before = saved.parent.clone();
        let sha = digest(&bytes);
        if saved.capture.as_ref() != Some(&sha) {
            return Err("historical verdict changed".into());
        }
        let delivery = notify(&prior, &mut saved, &verdict, &sha, 1).and_then(|()| {
            if saved.launch_claimed {
                notify(&prior, &mut saved, &verdict, &sha, 3)
            } else {
                Ok(())
            }
        });
        if let Err(error) = delivery {
            saved.parent = "pending".into();
            saved.detail = error.to_string();
        }
        if saved.parent != before {
            let _transaction = Lock::acquire(&directory.join("transaction.lock"))?;
            save(&directory, &mut saved)?;
        }
    }
    Ok(())
}

async fn run_generation(directory: &Path, once: bool) -> Result<Option<PathBuf>> {
    let config = load(directory)?;
    let _owner = Lock::acquire(&directory.join("router.lock"))?;
    recover(directory)?;
    let mut current = state(directory)?;
    lineage::Registry::view(&config.admission)?.current(&config)?;
    if current.review_launch_claimed
        && current.reviewer_action != "attempt_finished"
        && current.reviewer_action != "needs_input_launch_unknown"
    {
        current.reviewer_action = "needs_input_launch_unknown".into();
        save(directory, &mut current)?;
    }
    if current.config_sha256 != digest(&read(&directory.join("config.json"), 256 * 1024)?) {
        return Err("registered review configuration changed".into());
    }
    if (current.launch_claimed || current.producer == "preparing")
        && current.producer != "attempt_finished"
        && current.producer != "needs_input_launch_unknown"
    {
        current.launch_claimed = true;
        current.producer = "needs_input_launch_unknown".into();
        save(directory, &mut current)?;
    }
    loop {
        historical_delivery(&config)?;
        if directory.join("STOP").exists() || directory.join("DISABLED").exists() {
            return Ok(None);
        }
        let latest = lineage::Registry::view(&config.admission)?.latest(&config.task)?;
        if latest != directory {
            return Ok(Some(latest));
        }
        let before = serde_json::to_vec(&current)?;
        if stopped(&config) {
            current.detail = "routing disabled, stopped or superseded".into();
        } else {
            if config.rereview.is_some() && !current.review_launch_claimed {
                if !config.actions.resume_rejected {
                    current.reviewer_action = "manual".into();
                } else if Utc::now() >= config.until {
                    current.reviewer_action = "exhausted".into();
                } else if let Err(error) = launch(&config, &mut current, None, "", true).await {
                    current.reviewer_action = if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
                    {
                        "queued_busy"
                    } else {
                        "needs_input"
                    }
                    .into();
                    current.detail = error.to_string();
                }
            }
            match capture(&config) {
                Ok((verdict, sha)) => {
                    if !current.launch_claimed && !current.review_launch_claimed {
                        current.detail.clear();
                    }
                    current.capture = Some(sha.clone());
                    if !current.launch_claimed {
                        current.producer = if !config.actions.resume_rejected {
                            "manual"
                        } else if verdict.result != VerdictResult::Rejected {
                            "not_rejected"
                        } else {
                            "queued"
                        }
                        .into();
                    }
                    if let Err(e) = notify(&config, &mut current, &verdict, &sha, 1) {
                        current.parent = "pending".into();
                        current.detail = e.to_string();
                    }
                    if config.actions.resume_rejected
                        && verdict.result == VerdictResult::Rejected
                        && !current.launch_claimed
                    {
                        if Utc::now() >= config.until {
                            current.producer = "exhausted".into();
                            current.detail = "correction deadline expired".into();
                        } else if let Err(e) =
                            launch(&config, &mut current, Some(&verdict), &sha, false).await
                        {
                            current.producer = "needs_input".into();
                            current.detail = e.to_string();
                        }
                    }
                    if current.launch_claimed
                        || current.producer == "exhausted"
                        || current.producer == "needs_input"
                    {
                        let number = if current.launch_claimed { 3 } else { 2 };
                        if let Err(e) = notify(&config, &mut current, &verdict, &sha, number) {
                            current.parent = "pending".into();
                            current.detail = e.to_string();
                        }
                    }
                }
                Err(e) => {
                    if !current.reviewer_action.starts_with("queued")
                        && current.reviewer_action != "needs_input"
                    {
                        current.detail = e.to_string();
                    }
                    if !current.launch_claimed {
                        current.producer = "needs_input_review".into();
                    }
                    if let Ok(bytes) = read(&directory.join("capture/verdict.json"), 256 * 1024)
                        && let Ok(verdict) = serde_json::from_slice::<Verdict>(&bytes)
                    {
                        let sha = digest(&bytes);
                        let number = if current.launch_claimed { 3 } else { 2 };
                        if let Err(error) = notify(&config, &mut current, &verdict, &sha, number) {
                            current.parent = "pending".into();
                            current.detail.push_str(&format!("; parent: {error}"));
                        }
                    }
                }
            }
        }
        if serde_json::to_vec(&current)? != before {
            let _transaction = Lock::acquire(&directory.join("transaction.lock"))?;
            save(directory, &mut current)?;
        }
        if once || directory.join("STOP").exists() || directory.join("DISABLED").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(None)
}

pub fn status(directory: &Path) -> Result<()> {
    let config = load(directory)?;
    let admission = mush_supervisor::admission::Pool::open(&config.admission)
        .and_then(|pool| pool.status())
        .unwrap_or_else(|error| json!({"error":error.to_string()}));
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"assignment":config.assignment,"generation":config.generation,"actions":config.actions,"disabled":stopped(&config),"state":state(directory)?,"admission":admission,"lineage":lineage::Registry::view(&config.admission)?.inspect(&config.task)?})
        )?
    );
    Ok(())
}

pub fn disable(directory: &Path) -> Result<()> {
    load(directory)?;
    let _transaction = Lock::acquire(&directory.join("transaction.lock"))?;
    replace(&directory.join("DISABLED"), b"disabled")?;
    replace(
        &directory.join(format!("disabled-{}.json", Utc::now().timestamp_micros())),
        &serde_json::to_vec(&json!({"at":Utc::now(),"action":"disable"}))?,
    )?;
    Ok(())
}

pub fn enable(directory: &Path) -> Result<()> {
    load(directory)?;
    let _owner = Lock::acquire(&directory.join("router.lock"))?;
    let _transaction = Lock::acquire(&directory.join("transaction.lock"))?;
    replace(
        &directory.join(format!("enabled-{}.json", Utc::now().timestamp_micros())),
        &serde_json::to_vec(&json!({"at":Utc::now(),"action":"enable"}))?,
    )?;
    fs::remove_file(directory.join("DISABLED"))?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

pub fn retry(directory: &Path) -> Result<()> {
    let config = load(directory)?;
    let _owner = Lock::acquire(&directory.join("router.lock"))?;
    let _transaction = Lock::acquire(&directory.join("transaction.lock"))?;
    let mut current = state(directory)?;
    if current.launch_claimed || current.review_launch_claimed || stopped(&config) {
        return Err("cannot retry a claimed or stopped handoff".into());
    }
    let pending = directory.join("capture-pending");
    if pending.exists() {
        fs::rename(
            pending,
            directory.join(format!("abandoned-capture-{}", current.sequence)),
        )?;
        File::open(directory)?.sync_all()?;
    }
    current.detail = "explicit validation retry".into();
    save(directory, &mut current)
}
