use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, Notify};

pub type NoticeSink = Arc<dyn Fn(String) + Send + Sync>;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
const KILL_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_CAP_BYTES: usize = 4 * 1024 * 1024;

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
    pid: Option<u32>,
    output: String,
    end: Option<JobEnd>,
    started: Instant,
    finished: Option<Instant>,
    read: bool,
    waiting: usize,
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
}

struct JobCell {
    state: Mutex<Job>,
    done: Notify,
}

impl KernelJobs {
    pub fn set_notice_sink(&self, sink: NoticeSink) {
        *self
            .inner
            .notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    pub async fn start(
        &self,
        cwd: &Path,
        command: &str,
        timeout: Option<Duration>,
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
        mush_agent::exec_context::apply_control_environment(&mut cmd);
        cmd.arg("-c")
            .arg(crate::bash::command_with_direnv(command))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(stderr));
        for var in ["EDITOR", "VISUAL", "GIT_EDITOR", "JJ_EDITOR"] {
            cmd.env(var, "false");
        }
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;
        let pid = child.id();
        let cell = Arc::new(JobCell {
            state: Mutex::new(Job {
                command: command.to_string(),
                pid,
                output: String::new(),
                end: None,
                started: Instant::now(),
                finished: None,
                read: false,
                waiting: 0,
                pgid: pid.and_then(|p| i32::try_from(p).ok()),
            }),
            done: Notify::new(),
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
                            let mut job = cell.state.lock().await;
                            job.output.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if job.output.len() > OUTPUT_CAP_BYTES {
                                let cut = job.output.len() - OUTPUT_CAP_BYTES;
                                let cut = job.output.floor_char_boundary(cut);
                                job.output.drain(..cut);
                            }
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
                if job.waiting > 0 {
                    job.read = true;
                }
                (!job.read).then(|| {
                    format!(
                        "Background command finished: `{}` (pid {}, exit {}). Read the saved handle with `await h.output()` or `await h` and continue.",
                        command_preview(&job.command),
                        job.pid.map_or("?".to_string(), |p| p.to_string()),
                        end.exit_code()
                    )
                })
            };
            cell.done.notify_waiters();
            if let Some(text) = notice {
                jobs.emit_notice(&job_id, text).await;
            }
        });

        Ok((id, pid))
    }

    async fn emit_notice(&self, job_id: &str, text: String) {
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
            sink(text);
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
        job.read = true;
        Ok(snapshot_of(job_id, &job))
    }

    pub async fn wait(&self, job_id: &str) -> Result<JobSnapshot, String> {
        let cell = self.cell(job_id).await?;
        loop {
            let notified = cell.done.notified();
            {
                let mut job = cell.state.lock().await;
                if job.end.is_some() {
                    job.read = true;
                    return Ok(snapshot_of(job_id, &job));
                }
                job.waiting += 1;
            }
            notified.await;
            cell.state.lock().await.waiting -= 1;
        }
    }

    pub async fn kill(&self, job_id: &str) -> Result<(), String> {
        let cell = self.cell(job_id).await?;
        let pgid = {
            let mut job = cell.state.lock().await;
            if job.end.is_some() {
                return Ok(());
            }
            job.end = Some(JobEnd::Killed);
            job.pgid
        };
        terminate_group(pgid);
        let cell = cell.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KILL_GRACE).await;
            let job = cell.state.lock().await;
            if job.finished.is_none() {
                kill_group(job.pgid);
            }
        });
        Ok(())
    }
}

fn command_preview(command: &str) -> String {
    let flat = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let head: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{head}...")
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
        output: job.output.clone(),
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
            .start(&cwd(), "echo one; echo two >&2; exit 4", None)
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
            .start(&cwd(), "echo started; sleep 30; echo never", None)
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
            .start(&cwd(), "sleep 30", Some(Duration::from_millis(200)))
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
        let notices = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        jobs.set_notice_sink({
            let notices = notices.clone();
            Arc::new(move |text| notices.lock().unwrap().push(text))
        });

        let (unread, _) = jobs.start(&cwd(), "exit 2", None).await.unwrap();
        let (read, _) = jobs.start(&cwd(), "sleep 0.3", None).await.unwrap();
        let _ = jobs.wait(&read).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let notices = notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("exit 2"), "{notices:?}");
        assert!(notices[0].contains("`exit 2`"), "{notices:?}");
        let _ = unread;
    }

    #[tokio::test]
    async fn an_unknown_job_is_an_error() {
        let jobs = KernelJobs::default();

        assert!(jobs.snapshot("job-99").await.is_err());
        assert!(jobs.kill("job-99").await.is_err());
    }
}
