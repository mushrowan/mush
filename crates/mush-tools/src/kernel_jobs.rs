use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Notify};

pub type NoticeSink = Arc<dyn Fn(JobNotice) + Send + Sync>;

const NOTICE_COMMAND_CAP: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BashDone {
    pub pid: Option<u32>,
    pub exit_code: i32,
    pub command: String,
}

impl BashDone {
    #[must_use]
    pub fn text(&self) -> String {
        let pid = self
            .pid
            .map_or_else(|| "?".to_string(), |pid| pid.to_string());
        let command = serde_json::to_string(&self.command).unwrap_or_default();
        format!(
            "[bash-done pid:{pid} exit:{}]\n\nCommand: {command}",
            self.exit_code
        )
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("[bash-done pid:")?;
        let (pid, rest) = rest.split_once(" exit:")?;
        let (exit_code, rest) = rest.split_once("]\n\nCommand: ")?;
        Some(Self {
            pid: pid.parse().ok(),
            exit_code: exit_code.parse().ok()?,
            command: serde_json::from_str(rest.trim_end()).ok()?,
        })
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        self.exit_code != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobNotice {
    pub done: BashDone,
    pub origin: Option<String>,
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const KILL_GRACE: Duration = Duration::from_secs(5);
const CANCEL_TERM_GRACE: Duration = Duration::from_millis(500);
const CANCEL_KILL_WAIT: Duration = Duration::from_secs(2);
const OUTPUT_HEAD_BYTES: usize = 512 * 1024;
const OUTPUT_TAIL_BYTES: usize = 3 * 512 * 1024;
const QUIET_ENV: &[(&str, &str)] = &[
    ("EDITOR", "false"),
    ("VISUAL", "false"),
    ("GIT_EDITOR", "false"),
    ("JJ_EDITOR", "false"),
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("CLICOLOR", "0"),
    ("FORCE_COLOR", "0"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_ASKPASS", "true"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

#[derive(Debug, Default)]
struct BoundedOutput {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    dropped: usize,
}

impl BoundedOutput {
    fn push(&mut self, mut chunk: &[u8]) {
        if self.head.len() < OUTPUT_HEAD_BYTES {
            let take = chunk.len().min(OUTPUT_HEAD_BYTES - self.head.len());
            self.head.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
        }
        self.tail.extend(chunk);
        if self.tail.len() > OUTPUT_TAIL_BYTES {
            let excess = self.tail.len() - OUTPUT_TAIL_BYTES;
            self.tail.drain(..excess);
            self.dropped += excess;
        }
    }

    fn text(&self) -> String {
        let (front, back) = self.tail.as_slices();
        let tail = [front, back].concat();
        if self.dropped == 0 {
            return String::from_utf8_lossy(&[self.head.as_slice(), &tail].concat()).into_owned();
        }
        format!(
            "{}\n... [{} bytes dropped] ...\n{}",
            String::from_utf8_lossy(&self.head),
            self.dropped,
            String::from_utf8_lossy(&tail)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    Exited(i32),
    TimedOut,
    Killed,
    Failed(String),
}

impl JobEnd {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Exited(code) => *code,
            Self::TimedOut | Self::Killed | Self::Failed(_) => -1,
        }
    }
}

#[derive(Debug)]
struct Job {
    command: String,
    origin: Option<String>,
    pid: Option<u32>,
    output: BoundedOutput,
    end: Option<JobEnd>,
    started: Instant,
    finished: Option<Instant>,
    read: bool,
    pgid: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSnapshot {
    pub job_id: String,
    pub pid: Option<u32>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub end: Option<JobEnd>,
    pub output: String,
    pub duration: Duration,
}

#[derive(Clone, Default)]
pub struct KernelJobs {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    jobs: Mutex<HashMap<String, Arc<JobCell>>>,
    counter: std::sync::atomic::AtomicU64,
    notice: std::sync::Mutex<Option<NoticeSink>>,
    orphans: std::sync::Mutex<Option<crate::orphans::OrphanJournal>>,
}

struct JobCell {
    state: Mutex<Job>,
    done: Notify,
    waiting: std::sync::atomic::AtomicUsize,
}

struct Waiting<'a>(&'a JobCell);

impl<'a> Waiting<'a> {
    fn new(cell: &'a JobCell) -> Self {
        cell.waiting
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(cell)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0
            .waiting
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl KernelJobs {
    pub fn set_notice_sink(&self, sink: NoticeSink) {
        *self
            .inner
            .notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    pub fn set_orphan_journal(&self, journal: crate::orphans::OrphanJournal) {
        *self
            .inner
            .orphans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(journal);
    }

    pub fn journal(&self, pid: Option<u32>, active: bool) {
        let Some(pid) = pid else { return };
        if let Some(journal) = self
            .inner
            .orphans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            journal.record(pid, active);
        }
    }

    pub async fn start(
        &self,
        cwd: &Path,
        command: &str,
        timeout: Option<Duration>,
        origin: Option<&str>,
        env: Option<&[(String, String)]>,
    ) -> Result<(String, Option<u32>), String> {
        let id = format!(
            "job-{}",
            self.inner
                .counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1
        );
        let (reader, writer) = std::io::pipe().map_err(|e| format!("pipe: {e}"))?;
        let stderr = writer.try_clone().map_err(|e| format!("pipe: {e}"))?;

        let mut cmd = tokio::process::Command::new("bash");
        match env {
            Some(env) => {
                let path = env
                    .iter()
                    .find(|(key, _)| key == "PATH")
                    .map(|(_, value)| value.into())
                    .unwrap_or_default();
                cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
                mush_agent::exec_context::apply_control_environment_with_path(&mut cmd, path);
            }
            None => mush_agent::exec_context::apply_control_environment(&mut cmd),
        }
        cmd.arg("-c")
            .arg(crate::bash::command_with_direnv(command))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(stderr));
        cmd.envs(QUIET_ENV.iter().copied());
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;
        let pid = child.id();
        self.journal(pid, true);
        let cell = Arc::new(JobCell {
            state: Mutex::new(Job {
                command: command.to_string(),
                origin: origin.map(str::to_owned),
                pid,
                output: BoundedOutput::default(),
                end: None,
                started: Instant::now(),
                finished: None,
                read: false,
                pgid: pid.and_then(|p| i32::try_from(p).ok()),
            }),
            done: Notify::new(),
            waiting: std::sync::atomic::AtomicUsize::new(0),
        });
        self.inner
            .jobs
            .lock()
            .await
            .insert(id.clone(), cell.clone());

        let reader =
            tokio::fs::File::from_std(std::fs::File::from(std::os::fd::OwnedFd::from(reader)));
        let pump = {
            let cell = cell.clone();
            tokio::spawn(async move {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            cell.state.lock().await.output.push(&buf[..n]);
                        }
                    }
                }
            })
        };

        let jobs = self.clone();
        let job_id = id.clone();
        let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);
        tokio::spawn(async move {
            let end = match tokio::time::timeout(timeout, child.wait()).await {
                Ok(Ok(status)) => {
                    let killed = cell.state.lock().await.end == Some(JobEnd::Killed);
                    if killed {
                        JobEnd::Killed
                    } else {
                        JobEnd::Exited(status.code().unwrap_or(-1))
                    }
                }
                Ok(Err(error)) => JobEnd::Failed(error.to_string()),
                Err(_) => {
                    kill_group(cell.state.lock().await.pgid);
                    let _ = child.wait().await;
                    JobEnd::TimedOut
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
            let notice = {
                let mut job = cell.state.lock().await;
                job.end = Some(end.clone());
                job.finished = Some(Instant::now());
                if cell.waiting.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    job.read = true;
                }
                (!job.read).then(|| JobNotice {
                    done: BashDone {
                        pid: job.pid,
                        exit_code: end.exit_code(),
                        command: capped_command(&job.command),
                    },
                    origin: job.origin.clone(),
                })
            };
            cell.done.notify_waiters();
            jobs.journal(pid, false);
            if let Some(notice) = notice {
                jobs.emit_notice(&job_id, notice).await;
            }
        });

        Ok((id, pid))
    }

    async fn emit_notice(&self, job_id: &str, notice: JobNotice) {
        let already_read = match self.inner.jobs.lock().await.get(job_id) {
            Some(cell) => cell.state.lock().await.read,
            None => true,
        };
        if already_read {
            return;
        }
        let sink = self
            .inner
            .notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(sink) = sink {
            sink(notice);
        }
    }

    async fn cell(&self, job_id: &str) -> Result<Arc<JobCell>, String> {
        self.inner
            .jobs
            .lock()
            .await
            .get(job_id)
            .cloned()
            .ok_or_else(|| format!("unknown job {job_id}"))
    }

    pub async fn snapshot(&self, job_id: &str) -> Result<JobSnapshot, String> {
        let cell = self.cell(job_id).await?;
        let mut job = cell.state.lock().await;
        if job.end.is_some() {
            job.read = true;
        }
        Ok(snapshot_of(job_id, &job))
    }

    pub async fn running(&self, job_id: &str) -> Result<bool, String> {
        let cell = self.cell(job_id).await?;
        Ok(cell.state.lock().await.end.is_none())
    }

    pub async fn wait(&self, job_id: &str) -> Result<JobSnapshot, String> {
        let cell = self.cell(job_id).await?;
        let _waiting = Waiting::new(&cell);
        loop {
            let notified = cell.done.notified();
            {
                let mut job = cell.state.lock().await;
                if job.end.is_some() {
                    job.read = true;
                    return Ok(snapshot_of(job_id, &job));
                }
            }
            notified.await;
        }
    }

    pub async fn cancel(&self, job_id: &str) -> Result<(), String> {
        let cell = self.cell(job_id).await?;
        let pgid = {
            let mut job = cell.state.lock().await;
            job.read = true;
            if job.end.is_none() {
                job.end = Some(JobEnd::Killed);
            }
            job.pgid
        };
        terminate_group(pgid);
        if !group_exits_within(pgid, CANCEL_TERM_GRACE).await {
            kill_group(pgid);
            group_exits_within(pgid, CANCEL_KILL_WAIT).await;
        }
        Ok(())
    }

    pub async fn kill(&self, job_id: &str) -> Result<(), String> {
        let cell = self.cell(job_id).await?;
        let pgid = {
            let mut job = cell.state.lock().await;
            if job.end.is_none() {
                job.end = Some(JobEnd::Killed);
            } else if !group_alive(job.pgid) {
                return Ok(());
            }
            job.pgid
        };
        terminate_group(pgid);
        tokio::spawn(async move {
            tokio::time::sleep(KILL_GRACE).await;
            if group_alive(pgid) {
                kill_group(pgid);
            }
        });
        Ok(())
    }
}

fn capped_command(command: &str) -> String {
    let mut chars = command.chars();
    let head: String = chars.by_ref().take(NOTICE_COMMAND_CAP).collect();
    if chars.next().is_some() {
        format!("{head}\n... [command truncated]")
    } else {
        head
    }
}

fn snapshot_of(job_id: &str, job: &Job) -> JobSnapshot {
    JobSnapshot {
        job_id: job_id.to_string(),
        pid: job.pid,
        running: job.end.is_none(),
        exit_code: job.end.as_ref().map(JobEnd::exit_code),
        end: job.end.clone(),
        output: job.output.text(),
        duration: job
            .finished
            .map_or_else(|| job.started.elapsed(), |f| f.duration_since(job.started)),
    }
}

#[cfg(unix)]
fn terminate_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
    }
}

#[cfg(unix)]
fn kill_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

async fn group_exits_within(pgid: Option<i32>, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while group_alive(pgid) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

#[cfg(unix)]
fn group_alive(pgid: Option<i32>) -> bool {
    pgid.is_some_and(|pgid| unsafe { libc::kill(-pgid, 0) } == 0)
}

#[cfg(not(unix))]
fn group_alive(_pgid: Option<i32>) -> bool {
    false
}

#[cfg(not(unix))]
fn terminate_group(_pgid: Option<i32>) {}

#[cfg(not(unix))]
fn kill_group(_pgid: Option<i32>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    #[tokio::test]
    async fn a_job_runs_to_completion_and_wait_returns_its_output() {
        let jobs = KernelJobs::default();

        let (id, pid) = jobs
            .start(&cwd(), "echo one; echo two >&2; exit 4", None, None, None)
            .await
            .unwrap();
        let done = jobs.wait(&id).await.unwrap();

        assert!(pid.is_some());
        assert_eq!(done.exit_code, Some(4));
        assert!(!done.running);
        assert_eq!(done.output, "one\ntwo\n");
    }

    #[tokio::test]
    async fn a_running_job_shows_partial_output_and_can_be_killed() {
        let jobs = KernelJobs::default();

        let (id, _) = jobs
            .start(
                &cwd(),
                "echo started; sleep 30; echo never",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mid = jobs.snapshot(&id).await.unwrap();
        jobs.kill(&id).await.unwrap();
        let done = tokio::time::timeout(Duration::from_secs(5), jobs.wait(&id))
            .await
            .unwrap()
            .unwrap();

        assert!(mid.running);
        assert_eq!(mid.output, "started\n");
        assert_eq!(done.end, Some(JobEnd::Killed));
        assert!(!done.output.contains("never"));
    }

    #[tokio::test]
    async fn a_timed_out_job_is_reported_as_such() {
        let jobs = KernelJobs::default();

        let (id, _) = jobs
            .start(
                &cwd(),
                "sleep 30",
                Some(Duration::from_millis(200)),
                None,
                None,
            )
            .await
            .unwrap();
        let done = tokio::time::timeout(Duration::from_secs(5), jobs.wait(&id))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(done.end, Some(JobEnd::TimedOut));
        assert_eq!(done.exit_code, Some(-1));
    }

    #[tokio::test]
    async fn an_unread_finish_sends_a_notice_and_a_read_one_does_not() {
        let jobs = KernelJobs::default();
        let notices = Arc::new(std::sync::Mutex::new(Vec::<JobNotice>::new()));
        jobs.set_notice_sink({
            let notices = notices.clone();
            Arc::new(move |notice| notices.lock().unwrap().push(notice))
        });

        let (_, pid) = jobs
            .start(&cwd(), "exit 2", None, Some("call_7"), None)
            .await
            .unwrap();
        let (read, _) = jobs
            .start(&cwd(), "sleep 0.3", None, None, None)
            .await
            .unwrap();
        let _ = jobs.wait(&read).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let notices = notices.lock().unwrap().clone();
        assert_eq!(
            notices,
            vec![JobNotice {
                done: BashDone {
                    pid,
                    exit_code: 2,
                    command: "exit 2".into(),
                },
                origin: Some("call_7".into()),
            }]
        );
    }

    #[tokio::test]
    async fn polling_a_running_job_keeps_its_finish_notice() {
        let jobs = KernelJobs::default();
        let notices = Arc::new(std::sync::Mutex::new(Vec::<JobNotice>::new()));
        jobs.set_notice_sink({
            let notices = notices.clone();
            Arc::new(move |notice| notices.lock().unwrap().push(notice))
        });

        let (id, pid) = jobs
            .start(&cwd(), "sleep 0.3; exit 1", None, None, None)
            .await
            .unwrap();
        let early = jobs.snapshot(&id).await.unwrap();
        assert!(early.running);
        let deadline = Instant::now() + Duration::from_secs(5);
        while notices.lock().unwrap().is_empty() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let notices = notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].done.pid, pid);
        assert_eq!(notices[0].done.exit_code, 1);
    }

    #[test]
    fn a_bash_done_notice_round_trips_through_its_text() {
        let done = BashDone {
            pid: Some(4242),
            exit_code: 3,
            command: "echo \"hi\"\nsleep 1".into(),
        };

        let text = done.text();

        assert_eq!(
            text,
            "[bash-done pid:4242 exit:3]\n\nCommand: \"echo \\\"hi\\\"\\nsleep 1\""
        );
        assert_eq!(BashDone::parse(&text), Some(done));
        assert_eq!(BashDone::parse("Background command finished"), None);
    }

    #[tokio::test]
    async fn a_job_is_journaled_while_it_runs_and_struck_off_when_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orphans.jsonl");
        let jobs = KernelJobs::default();
        jobs.set_orphan_journal(crate::orphans::OrphanJournal::at(&path));

        let (id, pid) = jobs
            .start(&cwd(), "sleep 0.2", None, None, None)
            .await
            .unwrap();
        let running = std::fs::read_to_string(&path).unwrap();
        let _ = jobs.wait(&id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let finished = std::fs::read_to_string(&path).unwrap();

        let pid = pid.unwrap();
        assert!(running.contains(&format!("\"pid\":{pid},")), "{running}");
        assert!(running.contains("\"active\":true"), "{running}");
        assert_eq!(finished.lines().count(), 2, "{finished}");
        assert!(
            finished
                .lines()
                .last()
                .unwrap()
                .contains("\"active\":false"),
            "{finished}"
        );
    }

    #[test]
    fn output_keeps_the_head_and_tail_and_marks_the_dropped_middle() {
        let mut output = BoundedOutput::default();

        output.push(&vec![b'h'; OUTPUT_HEAD_BYTES]);
        output.push(&vec![b'm'; 1000]);
        output.push(&vec![b't'; OUTPUT_TAIL_BYTES]);

        let text = output.text();
        let (head, rest) = text.split_once('\n').unwrap();
        let (marker, tail) = rest.split_once('\n').unwrap();
        assert_eq!(head, "h".repeat(OUTPUT_HEAD_BYTES));
        assert_eq!(marker, "... [1000 bytes dropped] ...");
        assert_eq!(tail, "t".repeat(OUTPUT_TAIL_BYTES));
    }

    #[tokio::test]
    async fn a_job_runs_with_a_non_interactive_environment() {
        let jobs = KernelJobs::default();

        let (id, _) = jobs
            .start(
                &cwd(),
                "echo $TERM $PAGER $NO_COLOR $GIT_ASKPASS",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let done = jobs.wait(&id).await.unwrap();

        assert_eq!(done.output, "dumb cat 1 true\n");
    }

    #[tokio::test]
    async fn kill_reaches_a_group_left_behind_after_the_shell_exits() {
        let jobs = KernelJobs::default();

        let (id, pid) = jobs
            .start(&cwd(), "sleep 30 &", None, None, None)
            .await
            .unwrap();
        let done = tokio::time::timeout(Duration::from_secs(5), jobs.wait(&id))
            .await
            .unwrap()
            .unwrap();
        let pgid = pid.and_then(|pid| i32::try_from(pid).ok());
        assert_eq!(done.exit_code, Some(0));
        assert!(group_alive(pgid));
        jobs.kill(&id).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while group_alive(pgid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(!group_alive(pgid));
        assert_eq!(
            jobs.snapshot(&id).await.unwrap().end,
            Some(JobEnd::Exited(0))
        );
    }

    #[tokio::test]
    async fn an_unknown_job_is_an_error() {
        let jobs = KernelJobs::default();

        assert!(jobs.snapshot("job-99").await.is_err());
        assert!(jobs.kill("job-99").await.is_err());
    }
}
