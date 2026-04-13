//! background job registry for long-running bash commands
//!
//! allows the agent to spawn commands that run asynchronously and poll
//! for results, keeping API cache warm between polls

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

/// hard cap on concurrent background jobs regardless of flags
const MAX_CONCURRENT_JOBS: usize = 3;

/// jobs older than this are reaped automatically
const JOB_EXPIRY_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done { exit_code: i32 },
    TimedOut,
    Failed(String),
}

impl JobStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// shared state for a single background job
#[derive(Debug)]
pub struct JobState {
    pub id: String,
    pub command: String,
    pub status: JobStatus,
    pub stdout: String,
    pub stderr: String,
    pub started: Instant,
    pub cwd: PathBuf,
}

/// registry of active background jobs
///
/// thread-safe, shared between BashTool and BashStatusTool
#[derive(Debug, Clone)]
pub struct BackgroundJobRegistry {
    jobs: Arc<RwLock<HashMap<String, Arc<RwLock<JobState>>>>>,
    counter: Arc<std::sync::atomic::AtomicU64>,
}

impl BackgroundJobRegistry {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(HashMap::new())),
            counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// generate a unique job id
    pub fn next_id(&self) -> String {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("bg_{n}")
    }

    /// count of currently running jobs
    pub async fn running_count(&self) -> usize {
        let jobs = self.jobs.read().await;
        let mut count = 0;
        for job in jobs.values() {
            if job.read().await.status.is_running() {
                count += 1;
            }
        }
        count
    }

    /// check whether a new background job can be started
    ///
    /// returns Ok(()) if allowed, Err with a message describing why not.
    /// `concurrent` opts in to running alongside existing jobs (up to hard cap)
    pub async fn check_can_start(&self, concurrent: bool) -> Result<(), String> {
        let running = self.running_count().await;

        if running >= MAX_CONCURRENT_JOBS {
            return Err(format!(
                "hard limit of {MAX_CONCURRENT_JOBS} concurrent background jobs reached. \
                 wait for a running job to finish"
            ));
        }

        if running > 0 && !concurrent {
            // find the first running job to tell the agent about it
            let jobs = self.jobs.read().await;
            for job in jobs.values() {
                let state = job.read().await;
                if state.status.is_running() {
                    return Err(format!(
                        "background job already running ({}: {}). \
                         use concurrent: true to run in parallel, or poll {} first",
                        state.id, state.command, state.id
                    ));
                }
            }
        }

        Ok(())
    }

    /// register a new job and return its shared state handle
    pub async fn insert(&self, state: JobState) -> Arc<RwLock<JobState>> {
        let id = state.id.clone();
        let handle = Arc::new(RwLock::new(state));
        self.jobs.write().await.insert(id, handle.clone());
        handle
    }

    /// get a job's shared state by id
    pub async fn get(&self, id: &str) -> Option<Arc<RwLock<JobState>>> {
        self.jobs.read().await.get(id).cloned()
    }

    /// remove expired jobs
    pub async fn reap_expired(&self) {
        let mut jobs = self.jobs.write().await;
        jobs.retain(|_, job_handle| {
            // we can't await inside retain, so use try_read
            if let Ok(state) = job_handle.try_read() {
                state.started.elapsed().as_secs() < JOB_EXPIRY_SECS
            } else {
                true // keep jobs we can't read (they're being written to)
            }
        });
    }

    /// list all job ids and their statuses (for display)
    pub async fn list(&self) -> Vec<(String, String, JobStatus)> {
        let jobs = self.jobs.read().await;
        let mut result = Vec::new();
        for (id, handle) in jobs.iter() {
            let state = handle.read().await;
            result.push((id.clone(), state.command.clone(), state.status.clone()));
        }
        result
    }
}

impl Default for BackgroundJobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job(id: &str, command: &str) -> JobState {
        JobState {
            id: id.to_string(),
            command: command.to_string(),
            status: JobStatus::Running,
            stdout: String::new(),
            stderr: String::new(),
            started: Instant::now(),
            cwd: PathBuf::from("."),
        }
    }

    #[tokio::test]
    async fn next_id_increments() {
        let reg = BackgroundJobRegistry::new();
        assert_eq!(reg.next_id(), "bg_0");
        assert_eq!(reg.next_id(), "bg_1");
        assert_eq!(reg.next_id(), "bg_2");
    }

    #[tokio::test]
    async fn empty_registry_allows_start() {
        let reg = BackgroundJobRegistry::new();
        assert!(reg.check_can_start(false).await.is_ok());
    }

    #[tokio::test]
    async fn rejects_second_job_without_concurrent() {
        let reg = BackgroundJobRegistry::new();
        reg.insert(make_job("bg_0", "nix flake check")).await;

        let err = reg.check_can_start(false).await.unwrap_err();
        assert!(err.contains("bg_0"), "should mention running job: {err}");
        assert!(
            err.contains("concurrent: true"),
            "should suggest concurrent flag: {err}"
        );
    }

    #[tokio::test]
    async fn allows_second_job_with_concurrent() {
        let reg = BackgroundJobRegistry::new();
        reg.insert(make_job("bg_0", "nix flake check")).await;

        assert!(reg.check_can_start(true).await.is_ok());
    }

    #[tokio::test]
    async fn hard_cap_at_max_concurrent() {
        let reg = BackgroundJobRegistry::new();
        for i in 0..MAX_CONCURRENT_JOBS {
            reg.insert(make_job(&format!("bg_{i}"), &format!("cmd {i}")))
                .await;
        }

        let err = reg.check_can_start(true).await.unwrap_err();
        assert!(
            err.contains(&MAX_CONCURRENT_JOBS.to_string()),
            "should mention limit: {err}"
        );
    }

    #[tokio::test]
    async fn completed_jobs_dont_count_as_running() {
        let reg = BackgroundJobRegistry::new();
        let handle = reg.insert(make_job("bg_0", "echo done")).await;

        // mark it complete
        handle.write().await.status = JobStatus::Done { exit_code: 0 };

        // should now allow a new job without concurrent flag
        assert!(reg.check_can_start(false).await.is_ok());
    }

    #[tokio::test]
    async fn get_returns_job_state() {
        let reg = BackgroundJobRegistry::new();
        reg.insert(make_job("bg_0", "sleep 100")).await;

        let handle = reg.get("bg_0").await.unwrap();
        let state = handle.read().await;
        assert_eq!(state.command, "sleep 100");
        assert_eq!(state.status, JobStatus::Running);
    }

    #[tokio::test]
    async fn get_nonexistent_returns_none() {
        let reg = BackgroundJobRegistry::new();
        assert!(reg.get("bg_99").await.is_none());
    }

    #[tokio::test]
    async fn running_count_tracks_active() {
        let reg = BackgroundJobRegistry::new();
        assert_eq!(reg.running_count().await, 0);

        let h1 = reg.insert(make_job("bg_0", "cmd1")).await;
        reg.insert(make_job("bg_1", "cmd2")).await;
        assert_eq!(reg.running_count().await, 2);

        h1.write().await.status = JobStatus::Done { exit_code: 0 };
        assert_eq!(reg.running_count().await, 1);
    }

    #[tokio::test]
    async fn list_returns_all_jobs() {
        let reg = BackgroundJobRegistry::new();
        reg.insert(make_job("bg_0", "cmd1")).await;
        let h = reg.insert(make_job("bg_1", "cmd2")).await;
        h.write().await.status = JobStatus::Done { exit_code: 0 };

        let list = reg.list().await;
        assert_eq!(list.len(), 2);
    }
}
