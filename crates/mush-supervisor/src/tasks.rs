use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;

use crate::{Lock, Manifest, Result, Worker, atomic, read_json, safe_name, tail, validate};

#[derive(Args)]
pub struct Prepare {
    pub tasks: PathBuf,
    #[arg(long)]
    pub repository: PathBuf,
    #[arg(long)]
    pub directory: PathBuf,
    #[arg(long)]
    pub prefix: String,
    #[arg(long)]
    pub until: DateTime<Utc>,
    #[arg(long)]
    pub model: String,
    #[arg(long)]
    pub fallback_model: String,
    #[arg(long)]
    pub executable: PathBuf,
    #[arg(long)]
    pub instructions: Option<PathBuf>,
    #[arg(long, default_value = "@-")]
    pub revision: String,
    #[arg(long, default_value_t = 3)]
    pub concurrency: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    name: String,
    prompt: String,
}

async fn jj(root: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new("jj")
            .current_dir(root)
            .args(["--color=never", "--no-pager"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() {
        return Err(format!("jj failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn create(path: &Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

fn ensure_absent(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!("path already exists: {}", path.display()).into()),
        Err(error) => Err(error.into()),
    }
}

pub async fn prepare(args: Prepare) -> Result<()> {
    if !safe_name(&args.prefix) {
        return Err("invalid workspace prefix".into());
    }
    if args.tasks.metadata()?.len() > 1024 * 1024 {
        return Err("task list exceeds 1 mib".into());
    }
    let tasks: Vec<Task> = serde_json::from_slice(&fs::read(&args.tasks)?)?;
    let mut names = HashSet::new();
    if tasks.is_empty() || tasks.len() > 32 {
        return Err("choose between 1 and 32 tasks".into());
    }
    for task in &tasks {
        if !safe_name(&task.name)
            || !safe_name(&format!("{}-{}", args.prefix, task.name))
            || !names.insert(&task.name)
            || task.prompt.trim().is_empty()
            || task.prompt.len() > 128 * 1024
        {
            return Err("tasks require unique names and nonempty prompts".into());
        }
    }
    let instructions = args
        .instructions
        .as_ref()
        .map(fs::read_to_string)
        .transpose()?
        .unwrap_or_default();
    let root = fs::canonicalize(jj(&args.repository, &["root"]).await?)?;
    if root
        .join("workspaces")
        .symlink_metadata()
        .is_ok_and(|m| !m.is_dir())
    {
        return Err("workspace directory must be a directory".into());
    }
    let revision = jj(
        &root,
        &[
            "log",
            "--no-graph",
            "-r",
            &args.revision,
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )
    .await?;
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("choose exactly one base revision".into());
    }
    let mut manifest = Manifest {
        directory: std::path::absolute(&args.directory)?.join("run"),
        executable: fs::canonicalize(&args.executable)?,
        until: args.until,
        concurrency: args.concurrency,
        attempt_seconds: 2400,
        silence_seconds: 600,
        max_rounds: 200,
        model: args.model,
        fallback_model: args.fallback_model,
        status_file: true,
        orchestrator_inbox: None,
        workers: Vec::new(),
    };
    validate(&manifest)?;
    let directory = std::path::absolute(&args.directory)?;
    ensure_absent(&directory)?;
    for task in &tasks {
        ensure_absent(
            &root
                .join("workspaces")
                .join(format!("{}-{}", args.prefix, task.name)),
        )?;
    }
    fs::create_dir_all(directory.parent().ok_or("run directory needs a parent")?)?;
    fs::DirBuilder::new().mode(0o700).create(&directory)?;
    fs::create_dir(directory.join("prompts"))?;
    fs::create_dir(&manifest.directory)?;
    let _lock = Lock::acquire(&directory.join("prepare.lock"))?;
    atomic(&directory.join("tasks.json"), &tasks)?;
    fs::create_dir_all(root.join("workspaces"))?;
    for task in tasks {
        let name = format!("{}-{}", args.prefix, task.name);
        let cwd = root.join("workspaces").join(&name);
        let cwd_text = cwd.to_str().ok_or("workspace path must be utf-8")?;
        jj(
            &root,
            &[
                "workspace",
                "add",
                "--name",
                &name,
                "-r",
                &revision,
                "--",
                cwd_text,
            ],
        )
        .await?;
        fs::create_dir_all(cwd.join("data"))?;
        let report = cwd.join("data/agent-state.json");
        let prompt = directory.join("prompts").join(format!("{}.md", task.name));
        create(
            &prompt,
            &format!(
                "{instructions}\n\n{}\n\nworkspace: {}\nreport: {}\ndeadline: {}\nupdate the report before ending a turn. use state working, blocked or done. include completed, next, tests, commits and blockers. do not merge or edit the root checkout.\n",
                task.prompt,
                cwd.display(),
                report.display(),
                manifest.until
            ),
        )?;
        manifest.workers.push(Worker {
            name: task.name.clone(),
            cwd,
            prompt,
            report,
            gate: None,
            repeat_done: false,
            pause_seconds: 30,
        });
        atomic(
            &directory.join("prepared.json"),
            &json!({"base":revision,"workers":manifest.workers}),
        )?;
        let worker_dir = manifest.directory.join(task.name);
        fs::create_dir(&worker_dir)?;
        atomic(
            &worker_dir.join("state.json"),
            &json!({"state":"prepared","round":0,"model":manifest.model}),
        )?;
    }
    validate(&manifest)?;
    atomic(&manifest.directory.join("manifest.json"), &manifest)?;
    atomic(&directory.join("manifest.json"), &manifest)?;
    println!("{}", directory.join("manifest.json").display());
    Ok(())
}

pub fn status(directory: &Path) -> Result<()> {
    let manifest = read_json(&directory.join("manifest.json"))
        .map(serde_json::from_value::<Manifest>)
        .transpose()?;
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<std::result::Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut workers = Vec::new();
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(state) = read_json(&entry.path().join("state.json")) else {
            continue;
        };
        let job = manifest
            .as_ref()
            .and_then(|m| m.workers.iter().find(|w| w.name == name));
        let round = state["round"].as_u64().unwrap_or(0);
        workers.push(json!({
            "name":name, "state":state,
            "headless":read_json(&entry.path().join(format!("round-{round:03}.json"))),
            "report":job.and_then(|j| read_json(&j.report)),
            "cwd":job.map(|j| &j.cwd),
            "report_path":job.map(|j| &j.report),
            "log":entry.path().join(format!("round-{round:03}.log")),
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "supervisor":read_json(&directory.join("supervisor.json")), "workers":workers,
        }))?
    );
    Ok(())
}

pub fn logs(directory: &Path, worker: &str, bytes: u64) -> Result<()> {
    if !safe_name(worker) || bytes == 0 || bytes > 1024 * 1024 {
        return Err("invalid log request".into());
    }
    let state = read_json(&directory.join(worker).join("state.json")).ok_or("unknown worker")?;
    let round = state["round"].as_u64().ok_or("worker has no round")?;
    let path = directory.join(worker).join(format!("round-{round:03}.log"));
    if !path.is_file() {
        return Err("worker has no log yet".into());
    }
    print!("{}", tail(&path, bytes));
    Ok(())
}
