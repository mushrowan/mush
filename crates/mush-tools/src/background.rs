use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

const MAX_CONCURRENT_JOBS: usize = 3;

const JOB_EXPIRY_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done { exit_code: i32 },
    TimedOut,
    Killed,
    Failed(String),
}

impl JobStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Debug)]
pub struct JobState {
    pub id: String,
    pub command: String,
    pub status: JobStatus,
    pub stdout: String,
    pub stderr: String,
    pub stdout_consumed_bytes: usize,
    pub stderr_consumed_bytes: usize,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub cwd: Arc<Path>,
}

impl JobState {
    pub fn running(id: String, command: String, cwd: Arc<Path>, started: Instant) -> Self {
        Self {
            id,
            command,
            status: JobStatus::Running,
            stdout: String::new(),
            stderr: String::new(),
            stdout_consumed_bytes: 0,
            stderr_consumed_bytes: 0,
            started,
            finished: None,
            cwd,
        }
    }

    pub fn finish(&mut self, status: JobStatus) {
        debug_assert!(
            !status.is_running(),
            "JobState::finish called with Running status"
        );
        self.status = status;
        self.finished = Some(Instant::now());
    }

    pub fn run_duration(&self) -> std::time::Duration {
        match self.finished {
            Some(finished) => finished.duration_since(self.started),
            None => self.started.elapsed(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BackgroundJobRegistry {
    jobs: Arc<RwLock<HashMap<String, Arc<RwLock<JobState>>>>>,
    kill_handles: Arc<RwLock<HashMap<String, JobKillHandle>>>,
    counter: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug, Clone, Copy)]
pub struct JobKillHandle {
    process_group_id: i32,
}

impl JobKillHandle {
    pub fn from_child(child: &tokio::process::Child) -> Option<Self> {
        child.id().map(|pid| Self {
            process_group_id: pid as i32,
        })
    }

    pub fn kill(self) {
        #[cfg(unix)]
        unsafe {
            // SAFETY: negating the process group id targets the whole group.
            libc::kill(-self.process_group_id, libc::SIGKILL);
        }
    }
}

impl BackgroundJobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_id(&self) -> String {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("bg_{n}")
    }

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

    pub async fn check_can_start(&self, concurrent: bool) -> Result<(), String> {
        let running = self.running_count().await;

        if running >= MAX_CONCURRENT_JOBS {
            return Err(format!(
                "hard limit of {MAX_CONCURRENT_JOBS} concurrent background jobs reached. \
                 wait for a running job to finish"
            ));
        }

        if running > 0 && !concurrent {
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

    pub async fn insert(&self, state: JobState) -> Arc<RwLock<JobState>> {
        let id = state.id.clone();
        let handle = Arc::new(RwLock::new(state));
        self.jobs.write().await.insert(id, handle.clone());
        handle
    }

    pub async fn set_kill_handle(&self, id: &str, kill_handle: JobKillHandle) {
        self.kill_handles
            .write()
            .await
            .insert(id.to_string(), kill_handle);
    }

    pub async fn kill(&self, id: &str) -> bool {
        let kill_handle = self.kill_handles.write().await.remove(id);
        let Some(kill_handle) = kill_handle else {
            return false;
        };
        kill_handle.kill();
        true
    }

    pub async fn clear_kill_handle(&self, id: &str) {
        self.kill_handles.write().await.remove(id);
    }

    pub async fn get(&self, id: &str) -> Option<Arc<RwLock<JobState>>> {
        self.jobs.read().await.get(id).cloned()
    }

    pub async fn reap_expired(&self) {
        let mut jobs = self.jobs.write().await;
        jobs.retain(|_, job_handle| {
            if let Ok(state) = job_handle.try_read() {
                state.status.is_running()
                    || state
                        .finished
                        .is_none_or(|finished| finished.elapsed().as_secs() < JOB_EXPIRY_SECS)
            } else {
                true
            }
        });
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job(id: &str, command: &str) -> JobState {
        JobState::running(
            id.to_string(),
            command.to_string(),
            Arc::from(Path::new(".")),
            Instant::now(),
        )
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

        handle
            .write()
            .await
            .finish(JobStatus::Done { exit_code: 0 });

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

        h1.write().await.finish(JobStatus::Done { exit_code: 0 });
        assert_eq!(reg.running_count().await, 1);
    }

    #[tokio::test]
    async fn list_returns_all_jobs() {
        let reg = BackgroundJobRegistry::new();
        reg.insert(make_job("bg_0", "cmd1")).await;
        let h = reg.insert(make_job("bg_1", "cmd2")).await;
        h.write().await.finish(JobStatus::Done { exit_code: 0 });

        let list = reg.list().await;
        assert_eq!(list.len(), 2);
    }
}
