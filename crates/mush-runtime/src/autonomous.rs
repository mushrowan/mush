use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_CONTINUATIONS: u32 = 3;
pub const DEFAULT_MAX_TURNS: u32 = 12;
pub const DEFAULT_MAX_TOKENS: u64 = 80_000;
pub const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
pub const DEFAULT_GATE_RETRIES: u32 = 3;
pub const DEFAULT_GATE_TIMEOUT_MS: u64 = 5 * 60 * 1000;
pub const GATE_OUTPUT_CHARS: usize = 4_000;
pub const DEFAULT_SUBAGENT_KEEP_ALIVE_MS: u64 = 25 * 60 * 1000;

pub const CONTINUATION_PROMPT: &str = "No human is available in autonomous mode. Keep working until \
the configured gates pass or a limit stops the run. If you were going to ask the user something, make \
a reasonable assumption and check it. If you think you are blocked, prove it with evidence you can \
observe from the shell, keep that evidence, and look for safe progress while budget remains. Do not \
end the run yourself; the gates decide completion when they are configured.";

#[must_use]
pub fn usage_tokens(usage: &mush_ai::types::Usage) -> u64 {
    (usage.input_tokens + usage.output_tokens + usage.cache_write_tokens).get()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Limits {
    pub max_continuations: Option<u32>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
}

impl Limits {
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            max_continuations: Some(DEFAULT_MAX_CONTINUATIONS),
            max_turns: Some(DEFAULT_MAX_TURNS),
            max_tokens: Some(DEFAULT_MAX_TOKENS),
            timeout_ms: Some(DEFAULT_TIMEOUT_MS),
        }
    }

    #[must_use]
    pub fn unlimited() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_default_shaped(&self) -> bool {
        self.max_continuations.is_none()
            && self.max_turns.is_none()
            && self.max_tokens.is_none()
            && self.timeout_ms.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gates {
    pub commands: Vec<String>,
    pub max_retries: u32,
    pub timeout_ms: u64,
}

impl Default for Gates {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            max_retries: DEFAULT_GATE_RETRIES,
            timeout_ms: DEFAULT_GATE_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateFailure {
    pub command: String,
    pub attempt: u32,
    pub exit_text: String,
    pub output: String,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutonomousDefaults {
    pub enabled: bool,
    pub limits: Limits,
    pub gates: Gates,
    pub subagent_keep_alive_ms: u64,
}

impl Default for AutonomousDefaults {
    fn default() -> Self {
        Self {
            enabled: false,
            limits: Limits::defaults(),
            gates: Gates::default(),
            subagent_keep_alive_ms: DEFAULT_SUBAGENT_KEEP_ALIVE_MS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousOverrides {
    pub limits: Option<Limits>,
    pub gates: Vec<String>,
    pub gate_retries: Option<u32>,
    pub gate_timeout_ms: Option<u64>,
    pub subagent_keep_alive_ms: Option<u64>,
}

impl AutonomousOverrides {
    #[must_use]
    pub fn resolve(self, defaults: &AutonomousDefaults) -> (Limits, Gates, u64) {
        let mut gates = defaults.gates.clone();
        gates.commands.extend(self.gates);
        if let Some(retries) = self.gate_retries {
            gates.max_retries = retries;
        }
        if let Some(timeout) = self.gate_timeout_ms {
            gates.timeout_ms = timeout;
        }
        (
            self.limits.unwrap_or(defaults.limits),
            gates,
            self.subagent_keep_alive_ms
                .unwrap_or(defaults.subagent_keep_alive_ms),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AutonomousCommand {
    On {
        limits: Limits,
        gates: Gates,
        #[serde(default = "default_keep_alive")]
        subagent_keep_alive_ms: u64,
    },
    Off,
}

fn default_keep_alive() -> u64 {
    DEFAULT_SUBAGENT_KEEP_ALIVE_MS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousStatus {
    pub enabled: bool,
    pub limits: Limits,
    pub gates: Gates,
    pub continuations: u32,
    pub turns: u32,
    pub tokens: u64,
    pub elapsed_ms: u64,
    pub stopped: Option<String>,
    pub last_failure: Option<GateFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Continue(String),
    Stop(String),
    Idle,
}

#[derive(Default)]
struct Inner {
    enabled: bool,
    limits: Limits,
    gates: Gates,
    continuations: u32,
    turns: u32,
    tokens: u64,
    started_at: Option<Instant>,
    stopped: Option<String>,
    last_failure: Option<GateFailure>,
    gate_attempts: HashMap<String, u32>,
    due: bool,
    keep_alive_ms: u64,
    last_decision_at: Option<Instant>,
}

pub struct Autonomous {
    inner: Mutex<Inner>,
    cwd: PathBuf,
}

impl Autonomous {
    #[must_use]
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            cwd,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[must_use]
    pub fn status(&self) -> AutonomousStatus {
        let inner = self.lock();
        AutonomousStatus {
            enabled: inner.enabled,
            limits: inner.limits,
            gates: inner.gates.clone(),
            continuations: inner.continuations,
            turns: inner.turns,
            tokens: inner.tokens,
            elapsed_ms: inner.started_at.map_or(0, |at| {
                u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)
            }),
            stopped: inner.stopped.clone(),
            last_failure: inner.last_failure.clone(),
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    pub fn apply(&self, command: AutonomousCommand) -> AutonomousStatus {
        {
            let mut inner = self.lock();
            match command {
                AutonomousCommand::On {
                    limits,
                    gates,
                    subagent_keep_alive_ms,
                } => {
                    *inner = Inner {
                        enabled: true,
                        limits,
                        gates,
                        started_at: Some(Instant::now()),
                        keep_alive_ms: subagent_keep_alive_ms,
                        last_decision_at: Some(Instant::now()),
                        ..Inner::default()
                    };
                }
                AutonomousCommand::Off => {
                    inner.enabled = false;
                    inner.due = false;
                    inner.stopped = Some("turned off".into());
                }
            }
        }
        self.status()
    }

    pub fn record_usage(&self, tokens: u64) {
        let mut inner = self.lock();
        if inner.enabled {
            inner.tokens += tokens;
        }
    }

    pub fn record_assistant_turn(&self) {
        let mut inner = self.lock();
        if inner.enabled {
            inner.turns += 1;
        }
    }

    pub fn turn_ended(&self) {
        let mut inner = self.lock();
        if inner.enabled {
            inner.due = true;
        }
    }

    #[must_use]
    pub fn due(&self) -> bool {
        let inner = self.lock();
        inner.enabled && inner.due
    }

    fn limit_reached(inner: &Inner) -> Option<String> {
        if inner
            .limits
            .max_continuations
            .is_some_and(|max| inner.continuations >= max)
        {
            return Some(format!("{} continuations used", inner.continuations));
        }
        if inner.limits.max_turns.is_some_and(|max| inner.turns >= max) {
            return Some(format!("{} assistant turns used", inner.turns));
        }
        if inner
            .limits
            .max_tokens
            .is_some_and(|max| inner.tokens >= max)
        {
            return Some(format!("{} tokens used", inner.tokens));
        }
        if let (Some(timeout), Some(started)) = (inner.limits.timeout_ms, inner.started_at)
            && started.elapsed() >= Duration::from_millis(timeout)
        {
            return Some(format!(
                "{} minutes elapsed",
                started.elapsed().as_secs() / 60
            ));
        }
        None
    }

    #[must_use]
    pub fn gate_commands(&self) -> Vec<String> {
        self.lock().gates.commands.clone()
    }

    #[must_use]
    pub fn gate_timeout(&self) -> Duration {
        Duration::from_millis(self.lock().gates.timeout_ms)
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn skip_gate(&self, command: &str, snapshot: Option<&str>) -> bool {
        let inner = self.lock();
        inner.last_failure.as_ref().is_some_and(|failure| {
            failure.command == command
                && snapshot.is_some()
                && failure.snapshot.as_deref() == snapshot
        })
    }

    pub fn keep_alive_due(&self) -> bool {
        let inner = self.lock();
        inner.enabled
            && inner.due
            && inner.keep_alive_ms > 0
            && inner
                .last_decision_at
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(inner.keep_alive_ms))
    }

    pub fn keep_alive(&self) -> Decision {
        let mut inner = self.lock();
        if !inner.enabled || !inner.due {
            return Decision::Idle;
        }
        inner.last_decision_at = Some(Instant::now());
        if let Some(reason) = Self::limit_reached(&inner) {
            inner.enabled = false;
            inner.due = false;
            inner.stopped = Some(reason.clone());
            return Decision::Stop(reason);
        }
        inner.continuations += 1;
        Decision::Continue(format!(
            "[autonomous-continuation: subagents still running]\n\nYour child agents have been \
             running for over {} minutes without a reply. Check whether they are stuck (a \
             stopped process, a question nobody answers) with `await rlm.list_subagents()` and \
             unblock them, or carry on with your own work. Their replies still arrive as agent \
             messages.",
            inner.keep_alive_ms / 60_000
        ))
    }

    pub fn decide(&self, gate_results: Vec<GateRun>) -> Decision {
        let mut inner = self.lock();
        if !inner.enabled || !inner.due {
            return Decision::Idle;
        }
        inner.due = false;
        inner.last_decision_at = Some(Instant::now());
        if let Some(reason) = Self::limit_reached(&inner) {
            inner.enabled = false;
            inner.stopped = Some(reason.clone());
            return Decision::Stop(reason);
        }
        if !inner.gates.commands.is_empty() {
            let failed = gate_results.into_iter().find(|run| !run.passed);
            let Some(run) = failed else {
                inner.enabled = false;
                inner.stopped = Some("all gates passed".into());
                return Decision::Stop("all gates passed".into());
            };
            let attempt = inner.gate_attempts.get(&run.command).copied().unwrap_or(0) + 1;
            inner.gate_attempts.insert(run.command.clone(), attempt);
            let failure = GateFailure {
                command: run.command.clone(),
                attempt,
                exit_text: if run.skipped {
                    "not rerun: workspace unchanged since the previous failure".into()
                } else {
                    run.exit_text.clone()
                },
                output: run.output.clone(),
                snapshot: run.snapshot.clone(),
            };
            inner.last_failure = Some(failure.clone());
            if attempt > inner.gates.max_retries {
                inner.enabled = false;
                let reason = format!("gate `{}` failed {} times", run.command, attempt);
                inner.stopped = Some(reason.clone());
                return Decision::Stop(reason);
            }
            inner.continuations += 1;
            return Decision::Continue(gate_failure_prompt(&failure, inner.gates.max_retries));
        }
        inner.continuations += 1;
        Decision::Continue(format!(
            "[autonomous-continuation {}]\n\n{CONTINUATION_PROMPT}",
            inner.continuations
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRun {
    pub command: String,
    pub passed: bool,
    pub skipped: bool,
    pub exit_text: String,
    pub output: String,
    pub snapshot: Option<String>,
}

fn gate_failure_prompt(failure: &GateFailure, max_retries: u32) -> String {
    let mut text = format!(
        "[autonomous-continuation: gate failed]\n\nQuality gate failed (attempt {}/{}): `{}` {}.\n",
        failure.attempt, max_retries, failure.command, failure.exit_text
    );
    if !failure.output.trim().is_empty() {
        text.push_str("\nOutput:\n");
        text.push_str(failure.output.trim_end());
        text.push('\n');
    }
    if failure.exit_text.starts_with("not rerun") {
        text.push_str(
            "\nThe gate was not run again because nothing in the workspace changed since it last \
             failed. Change source, tests or a blocker note before trying to finish again.\n",
        );
    }
    text.push_str("\nKeep working: fix the failure, then leave evidence the gate can check.");
    text
}

pub fn workspace_snapshot(cwd: &Path) -> Option<String> {
    let status = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "-uall",
            "--no-renames",
            "--",
            ".",
            ":(exclude)target",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let diff = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "diff",
            "--no-color",
            "--",
            ".",
            ":(exclude)target",
        ])
        .current_dir(cwd)
        .output()
        .ok()?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    status.stdout.hash(&mut hasher);
    diff.stdout.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

pub async fn run_gate(command: &str, cwd: &Path, timeout: Duration) -> GateRun {
    let snapshot = workspace_snapshot(cwd);
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output();
    let (passed, exit_text, output) = match tokio::time::timeout(timeout, child).await {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            let exit_text = match output.status.code() {
                Some(code) => format!("exited with {code}"),
                None => "was killed by a signal".into(),
            };
            (
                output.status.success(),
                exit_text,
                tail(&text, GATE_OUTPUT_CHARS),
            )
        }
        Ok(Err(error)) => (false, format!("could not start: {error}"), String::new()),
        Err(_) => (
            false,
            format!("timed out after {}s", timeout.as_secs()),
            String::new(),
        ),
    };
    GateRun {
        command: command.to_string(),
        passed,
        skipped: false,
        exit_text,
        output,
        snapshot,
    }
}

fn tail(text: &str, chars: usize) -> String {
    let count = text.chars().count();
    if count <= chars {
        return text.to_string();
    }
    let skipped = count - chars;
    let kept: String = text.chars().skip(skipped).collect();
    format!("... ({skipped} characters before this)\n{kept}")
}

#[cfg(test)]
mod override_tests {
    use super::*;

    fn defaults() -> AutonomousDefaults {
        AutonomousDefaults {
            enabled: true,
            limits: Limits {
                max_turns: Some(9),
                ..Limits::defaults()
            },
            gates: Gates {
                commands: vec!["cargo fmt --check".into()],
                max_retries: 2,
                timeout_ms: 1_000,
            },
            subagent_keep_alive_ms: 5_000,
        }
    }

    #[test]
    fn nothing_given_takes_the_configured_defaults() {
        let (limits, gates, keep_alive) = AutonomousOverrides::default().resolve(&defaults());
        assert_eq!(limits.max_turns, Some(9));
        assert_eq!(gates.commands, vec!["cargo fmt --check".to_string()]);
        assert_eq!(gates.max_retries, 2);
        assert_eq!(keep_alive, 5_000);
    }

    #[test]
    fn a_gate_on_the_command_line_adds_to_the_configured_ones() {
        let overrides = AutonomousOverrides {
            gates: vec!["cargo test".into()],
            gate_retries: Some(0),
            subagent_keep_alive_ms: Some(0),
            ..AutonomousOverrides::default()
        };
        let (limits, gates, keep_alive) = overrides.resolve(&defaults());
        assert_eq!(
            gates.commands,
            vec!["cargo fmt --check".to_string(), "cargo test".to_string()]
        );
        assert_eq!(gates.max_retries, 0);
        assert_eq!(gates.timeout_ms, 1_000);
        assert_eq!(keep_alive, 0);
        assert_eq!(limits.max_turns, Some(9), "an untouched budget stays");
    }

    #[test]
    fn a_named_budget_flag_replaces_the_whole_budget() {
        let overrides = AutonomousOverrides {
            limits: Some(Limits {
                max_continuations: Some(3),
                ..Limits::unlimited()
            }),
            ..AutonomousOverrides::default()
        };
        let (limits, _, _) = overrides.resolve(&defaults());
        assert_eq!(limits.max_continuations, Some(3));
        assert_eq!(limits.max_turns, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(limits: Limits, gates: Gates) -> Autonomous {
        let autonomous = Autonomous::new(std::env::temp_dir());
        autonomous.apply(AutonomousCommand::On {
            limits,
            gates,
            subagent_keep_alive_ms: DEFAULT_SUBAGENT_KEEP_ALIVE_MS,
        });
        autonomous
    }

    #[test]
    fn autonomous_usage_leaves_out_cache_reads() {
        let usage = mush_ai::types::Usage {
            input_tokens: 100u64.into(),
            output_tokens: 20u64.into(),
            cache_read_tokens: 90_000u64.into(),
            cache_write_tokens: 5_000u64.into(),
        };
        assert_eq!(usage_tokens(&usage), 5_120);
    }

    #[test]
    fn without_gates_it_continues_until_the_continuation_limit() {
        let autonomous = on(
            Limits {
                max_continuations: Some(2),
                ..Limits::unlimited()
            },
            Gates::default(),
        );
        assert_eq!(autonomous.decide(Vec::new()), Decision::Idle);

        autonomous.turn_ended();
        assert!(
            matches!(autonomous.decide(Vec::new()), Decision::Continue(prompt) if prompt.starts_with("[autonomous-continuation 1]"))
        );
        autonomous.turn_ended();
        assert!(matches!(
            autonomous.decide(Vec::new()),
            Decision::Continue(_)
        ));
        autonomous.turn_ended();
        assert_eq!(
            autonomous.decide(Vec::new()),
            Decision::Stop("2 continuations used".into())
        );
        assert!(!autonomous.enabled());
        assert_eq!(
            autonomous.status().stopped.as_deref(),
            Some("2 continuations used")
        );
    }

    #[test]
    fn a_failing_gate_feeds_its_output_and_passing_gates_stop_the_run() {
        let autonomous = on(
            Limits::unlimited(),
            Gates {
                commands: vec!["cargo test".into()],
                max_retries: 1,
                timeout_ms: 1000,
            },
        );
        let failed = GateRun {
            command: "cargo test".into(),
            passed: false,
            skipped: false,
            exit_text: "exited with 101".into(),
            output: "test x failed".into(),
            snapshot: Some("abc".into()),
        };

        autonomous.turn_ended();
        let decision = autonomous.decide(vec![failed.clone()]);
        let Decision::Continue(prompt) = decision else {
            panic!("{decision:?}")
        };
        assert!(prompt.contains("attempt 1/1"));
        assert!(prompt.contains("test x failed"));
        assert!(autonomous.skip_gate("cargo test", Some("abc")));
        assert!(!autonomous.skip_gate("cargo test", Some("def")));

        autonomous.turn_ended();
        assert!(matches!(
            autonomous.decide(vec![GateRun { skipped: true, ..failed }]),
            Decision::Stop(reason) if reason.contains("failed 2 times")
        ));

        let passing = on(
            Limits::unlimited(),
            Gates {
                commands: vec!["true".into()],
                ..Gates::default()
            },
        );
        passing.turn_ended();
        assert_eq!(
            passing.decide(vec![GateRun {
                command: "true".into(),
                passed: true,
                skipped: false,
                exit_text: "exited with 0".into(),
                output: String::new(),
                snapshot: None,
            }]),
            Decision::Stop("all gates passed".into())
        );
    }

    #[tokio::test]
    async fn a_gate_runs_through_bash_with_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let ok = run_gate("echo fine", dir.path(), Duration::from_secs(5)).await;
        assert!(ok.passed);
        assert_eq!(ok.output.trim(), "fine");

        let bad = run_gate("echo nope >&2; exit 3", dir.path(), Duration::from_secs(5)).await;
        assert!(!bad.passed);
        assert_eq!(bad.exit_text, "exited with 3");
        assert_eq!(bad.output.trim(), "nope");

        let slow = run_gate("sleep 5", dir.path(), Duration::from_millis(100)).await;
        assert!(!slow.passed);
        assert!(slow.exit_text.starts_with("timed out"));
    }

    #[test]
    fn the_keep_alive_valve_opens_after_the_window_while_children_run() {
        let autonomous = Autonomous::new(std::env::temp_dir());
        autonomous.apply(AutonomousCommand::On {
            limits: Limits::unlimited(),
            gates: Gates::default(),
            subagent_keep_alive_ms: 1,
        });
        assert!(
            !autonomous.keep_alive_due(),
            "nothing is due before a turn ends"
        );
        autonomous.turn_ended();
        std::thread::sleep(Duration::from_millis(5));
        assert!(autonomous.keep_alive_due());
        assert!(matches!(
            autonomous.keep_alive(),
            Decision::Continue(prompt) if prompt.starts_with("[autonomous-continuation: subagents still running]")
        ));
        assert!(
            autonomous.due(),
            "the real decision is still owed once the children settle"
        );
        assert_eq!(autonomous.status().continuations, 1);
    }

    #[test]
    fn turned_off_stops_deciding() {
        let autonomous = on(Limits::defaults(), Gates::default());
        autonomous.turn_ended();
        autonomous.apply(AutonomousCommand::Off);
        assert_eq!(autonomous.decide(Vec::new()), Decision::Idle);
        assert!(!autonomous.status().enabled);
    }
}
