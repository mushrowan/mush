use std::path::PathBuf;
use std::sync::Mutex;

use mush_ai::types::Timestamp;
use serde::{Deserialize, Serialize};

pub const MIN_INTERVAL_MS: u64 = 10_000;
pub const MAX_JOBS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleKind {
    Once { at: u64 },
    Every { interval_ms: u64, next_at: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobSource {
    Heartbeat,
    RlmHeartbeat,
    Schedule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Active,
    Paused,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub label: Option<String>,
    pub prompt: String,
    pub schedule: ScheduleKind,
    pub expression: String,
    pub source: JobSource,
    pub status: JobStatus,
    pub delivery: Delivery,
    pub runs: u32,
    pub created_at: Timestamp,
    pub last_run_at: Option<Timestamp>,
}

impl Job {
    #[must_use]
    pub fn next_at(&self) -> Option<u64> {
        match (&self.schedule, self.status) {
            (_, JobStatus::Completed | JobStatus::Cancelled | JobStatus::Paused) => None,
            (ScheduleKind::Once { at }, _) => Some(*at),
            (ScheduleKind::Every { next_at, .. }, _) => Some(*next_at),
        }
    }

    #[must_use]
    pub fn prompt_text(&self) -> String {
        let label = self.label.as_deref().unwrap_or(&self.expression);
        let kind = match self.source {
            JobSource::Heartbeat | JobSource::RlmHeartbeat => "heartbeat",
            JobSource::Schedule => "scheduled",
        };
        format!("[{kind}: {label}]\n\n{}", self.prompt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ScheduleCommand {
    Heartbeat {
        expression: String,
        prompt: String,
        delivery: Delivery,
    },
    Add {
        expression: String,
        prompt: String,
        label: Option<String>,
        source: JobSource,
    },
    Pause(String),
    Resume(String),
    Cancel(String),
    HeartbeatPause,
    HeartbeatResume,
    HeartbeatClear,
}

pub fn parse_expression(text: &str, now_ms: u64) -> Result<ScheduleKind, String> {
    let text = text.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    let lower = text.to_ascii_lowercase();
    let unit_ms = |unit: &str| -> Option<u64> {
        match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => Some(1_000),
            "m" | "min" | "mins" | "minute" | "minutes" => Some(60_000),
            "h" | "hr" | "hrs" | "hour" | "hours" => Some(3_600_000),
            "d" | "day" | "days" => Some(86_400_000),
            _ => None,
        }
    };
    let amount_and_unit = |rest: &str| -> Option<u64> {
        let rest = rest.trim();
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let unit = rest[digits.len()..].trim();
        let amount: u64 = digits.parse().ok()?;
        Some(amount * unit_ms(unit)?)
    };
    if let Some(rest) = lower.strip_prefix("in ") {
        let delay =
            amount_and_unit(rest).ok_or_else(|| format!("cannot read a delay in {text:?}"))?;
        return Ok(ScheduleKind::Once { at: now_ms + delay });
    }
    if let Some(rest) = lower
        .strip_prefix("every ")
        .or_else(|| lower.strip_prefix("each "))
    {
        let interval_ms =
            amount_and_unit(rest).ok_or_else(|| format!("cannot read an interval in {text:?}"))?;
        if interval_ms < MIN_INTERVAL_MS {
            return Err(format!(
                "an interval must be at least {} seconds",
                MIN_INTERVAL_MS / 1000
            ));
        }
        return Ok(ScheduleKind::Every {
            interval_ms,
            next_at: now_ms + interval_ms,
        });
    }
    if let Some(rest) = text
        .strip_prefix("at ")
        .or_else(|| text.strip_prefix("At "))
    {
        let when = chrono::DateTime::parse_from_rfc3339(rest.trim())
            .map_err(|_| format!("cannot read a time in {rest:?}, use rfc3339"))?;
        let at = u64::try_from(when.timestamp_millis()).unwrap_or(0);
        if at <= now_ms {
            return Err("that time has passed".into());
        }
        return Ok(ScheduleKind::Once { at });
    }
    Err(format!(
        "cannot read a schedule in {text:?}. use `in 30m`, `every 10m` or `at <rfc3339>`"
    ))
}

pub struct Scheduler {
    jobs: Mutex<Vec<Job>>,
    path: Option<PathBuf>,
}

impl Scheduler {
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        let jobs = path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Vec<Job>>(&bytes).ok())
            .unwrap_or_default();
        Self {
            jobs: Mutex::new(jobs),
            path,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn persist(&self, jobs: &[Job]) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(jobs) {
            let _ = mush_ai::private_io::write_private(path, bytes);
        }
    }

    #[must_use]
    pub fn jobs(&self) -> Vec<Job> {
        self.lock().clone()
    }

    #[must_use]
    pub fn heartbeat(&self) -> Option<Job> {
        self.lock()
            .iter()
            .find(|job| {
                job.source == JobSource::Heartbeat
                    && matches!(job.status, JobStatus::Active | JobStatus::Paused)
            })
            .cloned()
    }

    pub fn apply(&self, command: ScheduleCommand) -> Result<Vec<Job>, String> {
        let now = Timestamp::now();
        let now_ms = now.as_ms();
        let mut jobs = self.lock();
        match command {
            ScheduleCommand::Heartbeat {
                expression,
                prompt,
                delivery,
            } => {
                let schedule = parse_expression(&expression, now_ms)?;
                if !matches!(schedule, ScheduleKind::Every { .. }) {
                    return Err("a heartbeat repeats: use `every 10m`".into());
                }
                if prompt.trim().is_empty() {
                    return Err("a heartbeat needs an instruction".into());
                }
                for job in jobs.iter_mut() {
                    if job.source == JobSource::Heartbeat && job.status != JobStatus::Cancelled {
                        job.status = JobStatus::Cancelled;
                    }
                }
                jobs.push(Job {
                    id: new_id(),
                    label: Some("heartbeat".into()),
                    prompt: prompt.trim().to_string(),
                    schedule,
                    expression: expression.trim().to_string(),
                    source: JobSource::Heartbeat,
                    status: JobStatus::Active,
                    delivery,
                    runs: 0,
                    created_at: now,
                    last_run_at: None,
                });
            }
            ScheduleCommand::Add {
                expression,
                prompt,
                label,
                source,
            } => {
                if jobs
                    .iter()
                    .filter(|job| job.status == JobStatus::Active)
                    .count()
                    >= MAX_JOBS
                {
                    return Err(format!("{MAX_JOBS} jobs are already scheduled"));
                }
                let schedule = parse_expression(&expression, now_ms)?;
                if source == JobSource::RlmHeartbeat
                    && !matches!(schedule, ScheduleKind::Every { .. })
                {
                    return Err("an rlm heartbeat repeats: use `every 10m`".into());
                }
                if prompt.trim().is_empty() {
                    return Err("a scheduled prompt needs text".into());
                }
                jobs.push(Job {
                    id: new_id(),
                    label: label
                        .map(|label| label.trim().to_string())
                        .filter(|l| !l.is_empty()),
                    prompt: prompt.trim().to_string(),
                    schedule,
                    expression: expression.trim().to_string(),
                    source,
                    status: JobStatus::Active,
                    delivery: Delivery::Steer,
                    runs: 0,
                    created_at: now,
                    last_run_at: None,
                });
            }
            ScheduleCommand::Pause(id) => {
                let job = find_mut(&mut jobs, &id)?;
                if job.status != JobStatus::Active {
                    return Err(format!("{id} is {:?}", job.status));
                }
                job.status = JobStatus::Paused;
            }
            ScheduleCommand::Resume(id) => {
                let job = find_mut(&mut jobs, &id)?;
                if job.status != JobStatus::Paused {
                    return Err(format!("{id} is {:?}", job.status));
                }
                job.status = JobStatus::Active;
                if let ScheduleKind::Every {
                    interval_ms,
                    next_at,
                } = &mut job.schedule
                    && *next_at < now_ms
                {
                    *next_at = now_ms + *interval_ms;
                }
            }
            ScheduleCommand::Cancel(id) => {
                let job = find_mut(&mut jobs, &id)?;
                job.status = JobStatus::Cancelled;
            }
            ScheduleCommand::HeartbeatPause
            | ScheduleCommand::HeartbeatResume
            | ScheduleCommand::HeartbeatClear => {
                let job = jobs
                    .iter_mut()
                    .find(|job| {
                        job.source == JobSource::Heartbeat
                            && matches!(job.status, JobStatus::Active | JobStatus::Paused)
                    })
                    .ok_or_else(|| "no heartbeat is set".to_string())?;
                job.status = match command {
                    ScheduleCommand::HeartbeatPause => JobStatus::Paused,
                    ScheduleCommand::HeartbeatResume => {
                        if let ScheduleKind::Every {
                            interval_ms,
                            next_at,
                        } = &mut job.schedule
                            && *next_at < now_ms
                        {
                            *next_at = now_ms + *interval_ms;
                        }
                        JobStatus::Active
                    }
                    _ => JobStatus::Cancelled,
                };
            }
        }
        jobs.retain(|job| {
            !matches!(job.status, JobStatus::Cancelled | JobStatus::Completed)
                || job.created_at.as_ms() + 86_400_000 > now_ms
        });
        let snapshot = jobs.clone();
        drop(jobs);
        self.persist(&snapshot);
        Ok(snapshot)
    }

    pub fn take_due(&self, now_ms: u64) -> Vec<Job> {
        let mut jobs = self.lock();
        let mut due = Vec::new();
        for job in jobs.iter_mut() {
            let Some(at) = job.next_at() else {
                continue;
            };
            if at > now_ms {
                continue;
            }
            job.runs += 1;
            job.last_run_at = Some(Timestamp::from_ms(now_ms));
            match &mut job.schedule {
                ScheduleKind::Once { .. } => job.status = JobStatus::Completed,
                ScheduleKind::Every {
                    interval_ms,
                    next_at,
                } => *next_at = now_ms + *interval_ms,
            }
            due.push(job.clone());
        }
        if !due.is_empty() {
            let snapshot = jobs.clone();
            drop(jobs);
            self.persist(&snapshot);
        }
        due
    }
}

fn find_mut<'a>(jobs: &'a mut [Job], id: &str) -> Result<&'a mut Job, String> {
    jobs.iter_mut()
        .find(|job| job.id == id || job.id.starts_with(id))
        .ok_or_else(|| format!("no job {id}"))
}

fn new_id() -> String {
    short_id()
}

#[must_use]
pub fn short_id() -> String {
    let id = mush_ai::types::SessionId::new().to_string();
    id[id.len() - 8..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions_parse_into_once_and_every() {
        assert_eq!(
            parse_expression("in 30m", 1_000).unwrap(),
            ScheduleKind::Once {
                at: 1_000 + 30 * 60_000
            }
        );
        assert_eq!(
            parse_expression("every 10 minutes", 1_000).unwrap(),
            ScheduleKind::Every {
                interval_ms: 600_000,
                next_at: 601_000
            }
        );
        assert!(
            parse_expression("every 5s", 0)
                .unwrap_err()
                .contains("at least")
        );
        assert!(parse_expression("tomorrow", 0).is_err());
        assert!(matches!(
            parse_expression("at 2999-01-01T00:00:00Z", 0).unwrap(),
            ScheduleKind::Once { .. }
        ));
    }

    #[test]
    fn a_heartbeat_fires_on_its_interval_and_replaces_the_previous_one() {
        let scheduler = Scheduler::new(None);
        scheduler
            .apply(ScheduleCommand::Heartbeat {
                expression: "every 10s".into(),
                prompt: "check the build".into(),
                delivery: Delivery::Steer,
            })
            .unwrap();
        let first = scheduler.heartbeat().unwrap();
        scheduler
            .apply(ScheduleCommand::Heartbeat {
                expression: "every 20s".into(),
                prompt: "check the tests".into(),
                delivery: Delivery::FollowUp,
            })
            .unwrap();
        let second = scheduler.heartbeat().unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(second.delivery, Delivery::FollowUp);

        let now = Timestamp::now().as_ms();
        assert!(scheduler.take_due(now).is_empty());
        let due = scheduler.take_due(now + 21_000);
        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].prompt_text(),
            "[heartbeat: heartbeat]\n\ncheck the tests"
        );
        assert!(scheduler.take_due(now + 22_000).is_empty());
        assert!(!scheduler.take_due(now + 42_000).is_empty());

        scheduler.apply(ScheduleCommand::HeartbeatPause).unwrap();
        assert!(scheduler.take_due(now + 90_000).is_empty());
        scheduler.apply(ScheduleCommand::HeartbeatResume).unwrap();
        scheduler.apply(ScheduleCommand::HeartbeatClear).unwrap();
        assert!(scheduler.heartbeat().is_none());
        assert!(scheduler.apply(ScheduleCommand::HeartbeatPause).is_err());
    }

    #[test]
    fn a_one_shot_completes_after_it_fires_and_the_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduled-jobs.json");
        let scheduler = Scheduler::new(Some(path.clone()));
        let jobs = scheduler
            .apply(ScheduleCommand::Add {
                expression: "in 1m".into(),
                prompt: "look at the log".into(),
                label: Some("log".into()),
                source: JobSource::Schedule,
            })
            .unwrap();
        let id = jobs[0].id.clone();

        let now = Timestamp::now().as_ms();
        let due = scheduler.take_due(now + 61_000);
        assert_eq!(due[0].prompt_text(), "[scheduled: log]\n\nlook at the log");
        assert_eq!(scheduler.jobs()[0].status, JobStatus::Completed);

        let reloaded = Scheduler::new(Some(path));
        assert_eq!(reloaded.jobs()[0].id, id);
        assert!(reloaded.apply(ScheduleCommand::Pause(id)).is_err());
    }
}
