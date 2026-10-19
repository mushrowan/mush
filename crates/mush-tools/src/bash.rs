use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

const MAX_FOREGROUND_TIMEOUT_SECS: u64 = 600;

const NO_OUTPUT_TIMEOUT_SECS: u64 = 240;
const STREAM_PIPE_YIELD_LINES: usize = 256;
const BACKGROUND_OUTPUT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

#[cfg(unix)]
struct ProcessGroupKillOnDrop {
    pgid: Option<libc::pid_t>,
}

#[cfg(unix)]
impl ProcessGroupKillOnDrop {
    fn new(child: &tokio::process::Child) -> Self {
        Self {
            pgid: child.id().map(|p| p as libc::pid_t),
        }
    }

    fn kill_now(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            crate::util::signal_group(pgid, libc::SIGKILL);
        }
    }

    fn disarm(&mut self) {
        self.pgid = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKillOnDrop {
    fn drop(&mut self) {
        self.kill_now();
    }
}

#[cfg(not(unix))]
struct ProcessGroupKillOnDrop;

#[cfg(not(unix))]
impl ProcessGroupKillOnDrop {
    fn new(_child: &tokio::process::Child) -> Self {
        Self
    }
    fn kill_now(&mut self) {}
    fn disarm(&mut self) {}
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BashOutput {
    #[default]
    Text,
    Json,
}

impl BashOutput {
    fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default, deserialize_with = "mush_agent::de::opt_uint")]
    timeout: Option<u64>,
    #[serde(default)]
    output: BashOutput,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    concurrent: bool,
}

impl BashArgs {
    fn effective_timeout(&self) -> u64 {
        self.timeout
            .unwrap_or(crate::DEFAULT_TOOL_TIMEOUT_SECS)
            .min(MAX_FOREGROUND_TIMEOUT_SECS)
    }
}

pub(crate) fn command_with_direnv(command: &str) -> String {
    format!(
        r#"if command -v direnv >/dev/null 2>&1; then
  eval "$(DIRENV_LOG_FORMAT= direnv export bash)"
fi
{command}"#
    )
}

#[must_use]
pub(crate) fn editor_hang_warning(command: &str) -> Option<String> {
    for segment in split_command_segments(command) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(msg) = check_segment(trimmed) {
            return Some(msg);
        }
    }
    None
}

fn split_command_segments(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let bytes = command.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        let b = bytes[i];
        if !in_single && b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_double && b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if in_single || in_double {
            i += 1;
            continue;
        }
        let split_len = match (b, bytes.get(i + 1).copied()) {
            (b'&', Some(b'&')) | (b'|', Some(b'|')) => 2,
            (b';' | b'&' | b'|', _) => 1,
            _ => 0,
        };
        if split_len > 0 {
            segments.push(&command[start..i]);
            i += split_len;
            start = i;
            continue;
        }
        i += 1;
    }
    segments.push(&command[start..]);
    segments
}

fn check_segment(segment: &str) -> Option<String> {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let head = tokens.first().copied()?;
    match head {
        "jj" => check_jj(&tokens),
        "git" => check_git(&tokens),
        "crontab" => check_crontab(&tokens),
        _ => None,
    }
}

fn check_jj(tokens: &[&str]) -> Option<String> {
    let sub = tokens.get(1).copied()?;
    if !matches!(sub, "describe" | "commit") {
        return None;
    }
    if has_message_flag(&tokens[2..]) {
        return None;
    }
    Some(format!(
        "`jj {sub}` without -m / --message would open $EDITOR and hang in this context. \
         pass the message inline, e.g. `jj {sub} -m 'your message'`"
    ))
}

fn check_git(tokens: &[&str]) -> Option<String> {
    let sub = tokens.get(1).copied()?;
    if sub != "commit" {
        return None;
    }
    let rest = &tokens[2..];
    if has_message_flag(rest)
        || rest.iter().any(|t| {
            *t == "--no-edit"
                || *t == "-F"
                || t.starts_with("--file=")
                || *t == "--file"
                || *t == "-c"
                || *t == "-C"
        })
    {
        return None;
    }
    Some(
        "`git commit` without -m / -F / --no-edit would open $EDITOR and hang in this context. \
         pass `-m 'your message'`, `-F file`, or `--amend --no-edit` to keep the existing message"
            .to_string(),
    )
}

fn check_crontab(tokens: &[&str]) -> Option<String> {
    if tokens.contains(&"-e") {
        return Some(
            "`crontab -e` opens $EDITOR and hangs in this context. \
             write the new crontab to a file and pipe it in: `cat new.cron | crontab -`"
                .to_string(),
        );
    }
    None
}

fn has_message_flag(tokens: &[&str]) -> bool {
    tokens.iter().any(|t| {
        *t == "-m"
            || t.starts_with("-m=")
            || *t == "--message"
            || t.starts_with("--message=")
            || *t == "--stdin"
    })
}

pub type OutputSink = std::sync::Arc<dyn Fn(&str, &str) + Send + Sync>;

type LineSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

// TODO: lines are redacted one at a time, so a multi-line secret such as a PEM body
// still streams to the screen live
#[must_use]
pub fn redacting_sink(sink: OutputSink) -> OutputSink {
    std::sync::Arc::new(move |exec_id: &str, line: &str| {
        sink(exec_id, &mush_agent::redact::redact(line));
    })
}

pub struct BashTool {
    cwd: Arc<Path>,
    output_sink: Option<OutputSink>,
    bg_registry: Option<crate::background::BackgroundJobRegistry>,
}

impl BashTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            cwd,
            output_sink: None,
            bg_registry: None,
        }
    }

    pub fn with_output_sink(mut self, sink: OutputSink) -> Self {
        self.output_sink = Some(redacting_sink(sink));
        self
    }

    pub fn with_background_registry(
        mut self,
        registry: crate::background::BackgroundJobRegistry,
    ) -> Self {
        self.bg_registry = Some(registry);
        self
    }
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn label(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. \
         Output is truncated to the last 2000 lines or 50KB (whichever is hit first). \
         If truncated, the full output is saved and the tool result includes the path. \
         Do not pre-truncate with `| head` / `| tail` / `| grep` when reading help text or \
         unfamiliar command output - truncation is automatic and the full output stays \
         recoverable, but `| head` discards everything past the cut before it reaches us. \
         Optionally provide a timeout in seconds for foreground commands. \
         For long-running commands (builds, test suites), set background: true to run \
         asynchronously and poll with bash_status to avoid cache busting. \
         Background jobs are not killed by timeout; use bash_status action=kill to stop one. \
         For searching file contents, use the Grep tool instead of running grep or rg via bash. \
         For locating files, use the Find or Glob tools instead of running find via bash. \
         Do not run interactive commands (e.g. vim, python REPL, less) as stdin is not available. \
         Commands like `jj commit`, `git commit` or `crontab -e` without an explicit -m/--message \
         will fail fast because EDITOR is set to `false` in the child environment - always pass \
         the message explicitly on the command line. \
         If a foreground command exceeds its timeout and background jobs are available, it is \
         moved to the background instead of killed. Commands that produce no output for 240s \
         before any longer foreground timeout are killed automatically. \
         Bash loads direnv environment exports for the current working directory when direnv is \
         installed and the directory is allowed."
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Tail
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": format!(
                        "foreground timeout in seconds (optional, default {}). background jobs ignore timeout and run until completion or explicit bash_status kill",
                        crate::DEFAULT_TOOL_TIMEOUT_SECS
                    ),
                    "default": crate::DEFAULT_TOOL_TIMEOUT_SECS
                },
                "output": {
                    "type": "string",
                    "description": "output format: 'text' (default) or 'json' with structured fields (stdout, stderr, exit_code, timed_out)",
                    "enum": ["text", "json"]
                },
                "background": {
                    "type": "boolean",
                    "description": "run in background, return job id immediately. poll with bash_status tool. use for commands >2min to keep cache warm. kill explicitly through bash_status if needed"
                },
                "concurrent": {
                    "type": "boolean",
                    "description": "allow running alongside existing background jobs (default: only 1 at a time, hard cap: 3)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<BashArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        if let Some(warning) = editor_hang_warning(&args.command) {
            return ToolResult::error(warning);
        }

        if args.background {
            return self.execute_background(args).await;
        }

        let line_sink = self.line_sink_for_current_exec();
        run_foreground(
            &self.cwd,
            &args.command,
            ForegroundLimits {
                timeout_secs: args.effective_timeout(),
                no_output_timeout_secs: NO_OUTPUT_TIMEOUT_SECS,
            },
            line_sink.as_ref(),
            args.output.is_json(),
            self.bg_registry.as_ref(),
        )
        .await
    }
}

impl BashTool {
    fn line_sink_for_current_exec(&self) -> Option<LineSink> {
        self.output_sink.as_ref().map(|sink| {
            let sink = sink.clone();
            let exec_id = mush_agent::exec_context::current_exec_id();
            std::sync::Arc::new(move |line: &str| sink(&exec_id, line)) as LineSink
        })
    }

    async fn execute_background(&self, args: BashArgs) -> ToolResult {
        let Some(registry) = &self.bg_registry else {
            return ToolResult::error(
                "background execution not available (no job registry configured)",
            );
        };

        if let Err(msg) = registry.check_can_start(args.concurrent).await {
            return ToolResult::error(msg);
        }

        let id = registry.next_id();
        let state = crate::background::JobState::running(
            id.clone(),
            args.command.clone(),
            std::time::Instant::now(),
        );

        let handle = registry.insert(state).await;
        let command = args.command.clone();
        let cwd = self.cwd.clone();
        let registry = registry.clone();
        let task_id = id.clone();
        let (spawn_ready_tx, spawn_ready_rx) = tokio::sync::oneshot::channel();
        let endpoint = mush_agent::exec_context::control_endpoint();

        tokio::spawn(async move {
            mush_agent::exec_context::CONTROL_ENDPOINT
                .scope(
                    endpoint,
                    run_background_command(
                        handle,
                        registry,
                        task_id,
                        &cwd,
                        &command,
                        spawn_ready_tx,
                    ),
                )
                .await;
        });
        let _ = spawn_ready_rx.await;

        let json = serde_json::json!({
            "job_id": id,
            "status": "running",
            "command": args.command,
            "message": format!(
                "command started in background. poll with bash_status tool using job_id: {id}"
            ),
        });
        ToolResult::text(json.to_string())
    }
}

async fn run_background_command(
    handle: std::sync::Arc<tokio::sync::RwLock<crate::background::JobState>>,
    registry: crate::background::BackgroundJobRegistry,
    job_id: String,
    cwd: &std::path::Path,
    command: &str,
    spawn_ready: tokio::sync::oneshot::Sender<()>,
) {
    let mut child = match bash_command(cwd, command).spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut state = handle.write().await;
            state.finish(crate::background::JobStatus::Failed(format!(
                "failed to spawn: {e}"
            )));
            let _ = spawn_ready.send(());
            return;
        }
    };

    let pg_guard = ProcessGroupKillOnDrop::new(&child);
    if let Some(kill_handle) = crate::background::JobKillHandle::from_child(&child) {
        registry.set_kill_handle(&job_id, kill_handle).await;
    }
    let _ = spawn_ready.send(());

    let pipes = Pipes::spawn(&mut child, None, None);
    follow_background_job(&handle, &registry, &job_id, child, pg_guard, pipes).await;
}

async fn follow_background_job(
    handle: &tokio::sync::RwLock<crate::background::JobState>,
    registry: &crate::background::BackgroundJobRegistry,
    job_id: &str,
    mut child: tokio::process::Child,
    mut pg_guard: ProcessGroupKillOnDrop,
    pipes: Pipes,
) {
    let mut publish = tokio::time::interval(BACKGROUND_OUTPUT_INTERVAL);
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            _ = publish.tick() => pipes.publish(&mut *handle.write().await),
        }
    };
    pg_guard.disarm();
    let (stdout, stderr) = pipes.finish().await;

    let mut state = handle.write().await;
    state.stdout = stdout;
    state.stderr = stderr;
    if state.status.is_running() {
        state.finish(match status {
            Ok(status) => crate::background::JobStatus::Done {
                exit_code: status.code().unwrap_or(-1),
            },
            Err(error) => crate::background::JobStatus::Failed(format!("command failed: {error}")),
        });
    }
    drop(state);
    registry.clear_kill_handle(job_id).await;
}

#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<String>>);

impl Captured {
    fn lock(&self) -> std::sync::MutexGuard<'_, String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn append_since(&self, seen: &mut String) {
        if let Some(new) = self.lock().get(seen.len()..) {
            seen.push_str(new);
        }
    }
}

struct Pipes {
    stdout: Captured,
    stderr: Captured,
    readers: [tokio::task::JoinHandle<()>; 2],
}

impl Pipes {
    fn spawn(
        child: &mut tokio::process::Child,
        sink: Option<&LineSink>,
        has_output: Option<&Arc<std::sync::atomic::AtomicBool>>,
    ) -> Self {
        let stdout = Captured::default();
        let stderr = Captured::default();
        let readers = [
            tokio::spawn(stream_pipe(
                child.stdout.take(),
                sink.cloned(),
                has_output.cloned(),
                stdout.clone(),
            )),
            tokio::spawn(stream_pipe(
                child.stderr.take(),
                sink.cloned(),
                has_output.cloned(),
                stderr.clone(),
            )),
        ];
        Self {
            stdout,
            stderr,
            readers,
        }
    }

    fn publish(&self, state: &mut crate::background::JobState) {
        self.stdout.append_since(&mut state.stdout);
        self.stderr.append_since(&mut state.stderr);
    }

    async fn finish(self) -> (String, String) {
        for reader in self.readers {
            let _ = reader.await;
        }
        (
            std::mem::take(&mut *self.stdout.lock()),
            std::mem::take(&mut *self.stderr.lock()),
        )
    }
}

fn bash_command(cwd: &std::path::Path, command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("bash");
    mush_agent::exec_context::apply_control_environment(&mut cmd);
    cmd.arg("-c")
        .arg(command_with_direnv(command))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for var in ["EDITOR", "VISUAL", "GIT_EDITOR", "JJ_EDITOR"] {
        cmd.env(var, "false");
    }

    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

#[derive(Clone, Copy)]
struct ForegroundLimits {
    timeout_secs: u64,
    no_output_timeout_secs: u64,
}

async fn run_foreground(
    cwd: &std::path::Path,
    command: &str,
    limits: ForegroundLimits,
    output_sink: Option<&LineSink>,
    json_output: bool,
    timeout_adoption: Option<&crate::background::BackgroundJobRegistry>,
) -> ToolResult {
    let ForegroundLimits {
        timeout_secs,
        no_output_timeout_secs,
    } = limits;
    let mut child = match bash_command(cwd, command).kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("failed to spawn command: {e}")),
    };

    let mut pg_guard = ProcessGroupKillOnDrop::new(&child);

    let started = std::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);
    let no_output_timeout = tokio::time::Duration::from_secs(no_output_timeout_secs);

    let has_output = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let pipes = Pipes::spawn(&mut child, output_sink, Some(&has_output));

    enum WaitResult {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        NoOutput,
    }

    let result = tokio::select! {
        status = child.wait() => WaitResult::Exited(status),
        () = tokio::time::sleep(timeout) => WaitResult::TimedOut,
        () = async {
            tokio::time::sleep(no_output_timeout).await;
            if has_output.load(std::sync::atomic::Ordering::Relaxed) {
                std::future::pending::<()>().await;
            }
        } => WaitResult::NoOutput,
    };

    match result {
        WaitResult::Exited(Ok(status)) => {
            pg_guard.disarm();
            let (stdout, stderr) = pipes.finish().await;
            let exit_code = status.code().unwrap_or(-1);
            let duration = started.elapsed();
            format_result(stdout, stderr, exit_code, false, duration, json_output)
        }
        WaitResult::Exited(Err(e)) => {
            pg_guard.disarm();
            ToolResult::error(format!("command failed: {e}"))
        }
        WaitResult::TimedOut => {
            let duration = started.elapsed();
            if let Some(registry) = timeout_adoption {
                return continue_in_background_after_timeout(
                    registry,
                    command,
                    timeout_secs,
                    TimedOutCommand {
                        child,
                        pg_guard,
                        pipes,
                        duration,
                    },
                    json_output,
                )
                .await;
            }

            pg_guard.kill_now();
            let (stdout, stderr) = pipes.finish().await;
            if json_output {
                return format_result(stdout, stderr, -1, true, duration, true);
            }
            ToolResult::error(format!("command timed out after {timeout_secs}s"))
        }
        WaitResult::NoOutput => {
            pg_guard.kill_now();
            let (stdout, stderr) = pipes.finish().await;
            let duration = started.elapsed();
            if json_output {
                return format_result(stdout, stderr, -1, true, duration, true);
            }
            ToolResult::error(format!(
                "command produced no output for {no_output_timeout_secs}s and was killed. \
                 this usually means the command is interactive, waiting for a TTY, \
                 or running as a daemon. use background: true for long-running commands, \
                 or check that the command works in non-interactive mode"
            ))
        }
    }
}

struct TimedOutCommand {
    child: tokio::process::Child,
    pg_guard: ProcessGroupKillOnDrop,
    pipes: Pipes,
    duration: std::time::Duration,
}

async fn continue_in_background_after_timeout(
    registry: &crate::background::BackgroundJobRegistry,
    command: &str,
    timeout_secs: u64,
    running: TimedOutCommand,
    json_output: bool,
) -> ToolResult {
    let id = registry.next_id();
    let duration = running.duration;
    let kill_handle = crate::background::JobKillHandle::from_child(&running.child);
    let state = crate::background::JobState::running(
        id.clone(),
        command.to_string(),
        std::time::Instant::now() - duration,
    );
    let handle = registry.insert(state).await;
    if let Some(kill_handle) = kill_handle {
        registry.set_kill_handle(&id, kill_handle).await;
    }
    let registry = registry.clone();
    let job_id = id.clone();

    tokio::spawn(async move {
        let TimedOutCommand {
            child,
            pg_guard,
            pipes,
            ..
        } = running;
        follow_background_job(&handle, &registry, &job_id, child, pg_guard, pipes).await;
    });

    format_backgrounded_timeout_result(&id, command, timeout_secs, duration, json_output)
}

fn format_backgrounded_timeout_result(
    id: &str,
    command: &str,
    timeout_secs: u64,
    duration: std::time::Duration,
    json_output: bool,
) -> ToolResult {
    let duration_secs = tenths_of_seconds(duration);
    let message = format!(
        "exceeded tool timeout after {timeout_secs}s, continuing in the background. check bash_status with job_id: {id} to monitor"
    );

    if json_output {
        let json = serde_json::json!({
            "job_id": id,
            "status": "backgrounded",
            "command": command,
            "timed_out": true,
            "timeout_seconds": timeout_secs,
            "duration_seconds": duration_secs,
            "message": message,
        });
        ToolResult::text(json.to_string())
    } else {
        ToolResult::text(format!(
            "Exit code: pending\nStatus: backgrounded\nDuration: {duration_secs}s\n\n{message}"
        ))
    }
}

fn format_result(
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    duration: std::time::Duration,
    json_output: bool,
) -> ToolResult {
    let duration_secs = tenths_of_seconds(duration);
    let status = command_status(exit_code, timed_out);

    let body = if json_output {
        serde_json::json!({
            "stdout": &stdout,
            "stderr": &stderr,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "status": status.as_str(),
            "duration_seconds": duration_secs,
            "stdout_lines": stdout.lines().count(),
            "stderr_lines": stderr.lines().count(),
            "stdout_bytes": stdout.len(),
            "stderr_bytes": stderr.len(),
        })
        .to_string()
    } else {
        let streams: Vec<&str> = [stdout.as_str(), stderr.as_str()]
            .into_iter()
            .filter(|stream| !stream.is_empty())
            .collect();
        let output = if streams.is_empty() {
            "(no output)".to_string()
        } else {
            streams.join("\n")
        };
        format!(
            "Exit code: {exit_code}\nStatus: {}\nDuration: {duration_secs}s\n\n{output}",
            status.as_str()
        )
    };

    if exit_code != 0 || timed_out {
        ToolResult::error(body)
    } else {
        ToolResult::text(body)
    }
}

fn tenths_of_seconds(duration: std::time::Duration) -> f32 {
    (duration.as_secs_f32() * 10.0).round() / 10.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandStatus {
    Success,
    Failed,
    TimedOut,
}

impl CommandStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }
}

const fn command_status(exit_code: i32, timed_out: bool) -> CommandStatus {
    if timed_out {
        CommandStatus::TimedOut
    } else if exit_code == 0 {
        CommandStatus::Success
    } else {
        CommandStatus::Failed
    }
}

async fn stream_pipe<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    sink: Option<LineSink>,
    has_output: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    output: Captured,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let Some(pipe) = pipe else {
        return;
    };
    let mut reader = BufReader::new(pipe);
    let mut bytes = Vec::new();
    let mut last_emit = std::time::Instant::now();
    let mut lines_read = 0usize;

    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes).await {
            Ok(0) => break,
            Ok(_) => {
                lines_read += 1;
                let line = String::from_utf8_lossy(&bytes);
                output.lock().push_str(&line);
                if let Some(ref flag) = has_output {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(ref sink) = sink
                    && last_emit.elapsed() >= std::time::Duration::from_millis(100)
                {
                    sink(line.trim_end());
                    last_emit = std::time::Instant::now();
                }
                if lines_read.is_multiple_of(STREAM_PIPE_YIELD_LINES) {
                    tokio::task::yield_now().await;
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;

    async fn run_command(
        cwd: &std::path::Path,
        command: &str,
        timeout_secs: u64,
        output_sink: Option<&LineSink>,
        json_output: bool,
    ) -> ToolResult {
        run_command_with_no_output_timeout(
            cwd,
            command,
            timeout_secs,
            output_sink,
            json_output,
            NO_OUTPUT_TIMEOUT_SECS,
        )
        .await
    }

    async fn run_command_with_no_output_timeout(
        cwd: &std::path::Path,
        command: &str,
        timeout_secs: u64,
        output_sink: Option<&LineSink>,
        json_output: bool,
        no_output_timeout_secs: u64,
    ) -> ToolResult {
        let limits = ForegroundLimits {
            timeout_secs,
            no_output_timeout_secs,
        };
        run_foreground(cwd, command, limits, output_sink, json_output, None).await
    }

    #[tokio::test]
    async fn control_environment_is_isolated_for_foreground_background_and_batch() {
        use mush_agent::exec_context::{CONTROL_ENDPOINT, ControlEndpoint};
        let dir = tempfile::tempdir().unwrap();
        let a = ControlEndpoint {
            socket: dir.path().join("a.sock"),
            target: mush_agent::ipc::control::Control::new("a".into()).target(),
        };
        let b = ControlEndpoint {
            socket: dir.path().join("b.sock"),
            target: mush_agent::ipc::control::Control::new("b".into()).target(),
        };
        let command = "printf '%s/%s/%s' \"$MUSH_CONTROL_INSTANCE\" \"$MUSH_CONTROL_SESSION\" \"$MUSH_CONTROL_ACTIVATION\"";
        let tool = BashTool::new(dir.path().into());
        let (ra, rb) = tokio::join!(
            CONTROL_ENDPOINT.scope(
                Some(a.clone()),
                tool.execute(serde_json::json!({"command":command}))
            ),
            CONTROL_ENDPOINT.scope(
                Some(b.clone()),
                tool.execute(serde_json::json!({"command":command}))
            ),
        );
        assert!(extract_text(&ra).contains(&a.target.instance));
        assert!(!extract_text(&ra).contains(&b.target.instance));
        assert!(extract_text(&rb).contains(&b.target.instance));
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = tool.with_background_registry(registry.clone());
        let result = CONTROL_ENDPOINT
            .scope(
                Some(a.clone()),
                tool.execute(serde_json::json!({"command":command,"background":true})),
            )
            .await;
        assert!(result.outcome.is_success());
        let handle = registry.get("bg_0").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !handle.read().await.status.is_running() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(handle.read().await.stdout.contains(&a.target.instance));
        let mut tools = mush_agent::tool::ToolRegistry::new();
        tools.register_shared(Arc::new(tool));
        let batch = crate::batch::BatchTool::new(tools);
        let result = CONTROL_ENDPOINT.scope(Some(b.clone()), batch.execute(serde_json::json!({"tool_calls":[{"tool":"bash","parameters":{"command":command}}]}))).await;
        assert!(extract_text(&result).contains(&b.target.instance));
        assert!(mush_agent::exec_context::control_endpoint().is_none());
    }

    #[tokio::test]
    async fn run_echo_command() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo hello", 10, None, false).await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn output_after_invalid_utf8_is_kept() {
        let cwd = tempfile::tempdir().unwrap();
        let result = run_command(
            cwd.path(),
            "printf 'caf\\351\\n'; echo after",
            10,
            None,
            false,
        )
        .await;
        assert!(result.outcome.is_success(), "{result:?}");
        let text = extract_text(&result);
        assert!(text.contains("caf\u{fffd}\nafter"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_pipe_yields_while_draining_ready_output() {
        let mut input = String::new();
        for line in 0..10_000 {
            input.push_str(&format!("line {line}\n"));
        }
        let reader = std::io::Cursor::new(input.into_bytes());
        let yielded = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let drain = {
            let yielded = yielded.clone();
            async move {
                stream_pipe(Some(reader), None, None, Captured::default()).await;
                assert!(
                    yielded.load(std::sync::atomic::Ordering::Relaxed),
                    "stream_pipe drained buffered output without yielding"
                );
            }
        };
        let marker = {
            let yielded = yielded.clone();
            async move {
                tokio::task::yield_now().await;
                yielded.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };

        tokio::join!(drain, marker);
    }

    #[test]
    fn editor_hang_warning_flags_jj_describe_without_message() {
        let warning = editor_hang_warning("jj describe");
        assert!(warning.is_some());
        let msg = warning.unwrap();
        assert!(
            msg.contains("jj describe") && msg.contains("-m"),
            "expected actionable warning, got {msg:?}"
        );
    }

    #[test]
    fn editor_hang_warning_accepts_jj_describe_with_message() {
        assert!(editor_hang_warning("jj describe -m 'feat: thing'").is_none());
        assert!(editor_hang_warning("jj describe --message='feat: thing'").is_none());
    }

    #[test]
    fn editor_hang_warning_flags_jj_commit_without_message() {
        let warning = editor_hang_warning("jj commit");
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("-m"));
    }

    #[test]
    fn editor_hang_warning_flags_git_commit_without_message_or_no_edit() {
        let warning = editor_hang_warning("git commit");
        assert!(warning.is_some());
        let msg = warning.unwrap();
        assert!(
            msg.contains("git commit") && (msg.contains("-m") || msg.contains("--no-edit")),
            "expected git-commit warning, got {msg:?}"
        );
    }

    #[test]
    fn editor_hang_warning_accepts_git_commit_with_safe_flag() {
        assert!(editor_hang_warning("git commit -m 'fix'").is_none());
        assert!(editor_hang_warning("git commit --no-edit").is_none());
        assert!(editor_hang_warning("git commit -F message.txt").is_none());
        assert!(editor_hang_warning("git commit --amend --no-edit").is_none());
    }

    #[test]
    fn editor_hang_warning_flags_crontab_e() {
        assert!(editor_hang_warning("crontab -e").is_some());
        assert!(editor_hang_warning("crontab -l").is_none());
    }

    #[test]
    fn editor_hang_warning_handles_chained_commands() {
        let warning = editor_hang_warning("jj st && jj describe");
        assert!(
            warning.is_some(),
            "chained command with hang in the second segment must warn"
        );
    }

    #[test]
    fn editor_hang_warning_passes_through_safe_commands() {
        for cmd in [
            "echo hello",
            "ls -la",
            "jj log",
            "git status",
            "git push",
            "jj describe -m 'msg'",
            "rg pattern src/",
        ] {
            assert!(
                editor_hang_warning(cmd).is_none(),
                "false positive on `{cmd}`"
            );
        }
    }

    #[tokio::test]
    async fn editor_var_is_false_so_editor_launches_fail_fast() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo editor=$EDITOR", 10, None, false).await;
        let text = extract_text(&result);
        assert!(
            text.contains("editor=false"),
            "expected EDITOR=false in child env, got {text}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn run_command_loads_direnv_environment() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".envrc"),
            "export MUSH_DIRENV_TEST=loaded\n",
        )
        .unwrap();

        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let direnv = bin.join("direnv");
        let original_path = std::env::var_os("PATH");
        let bash = original_path
            .as_deref()
            .and_then(|path| {
                std::env::split_paths(path)
                    .map(|dir| dir.join("bash"))
                    .find(|candidate| candidate.is_file())
            })
            .expect("bash must be present in PATH for bash tool tests");
        std::fs::write(
            &direnv,
            format!(
                r#"#!{}
if [ "$1" = export ] && [ "$2" = bash ]; then
  printf 'export MUSH_DIRENV_TEST=loaded\n'
  exit 0
fi
exit 64
"#,
                bash.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&direnv).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&direnv, permissions).unwrap();

        let path = match &original_path {
            Some(original) => format!("{}:{}", bin.display(), original.to_string_lossy()),
            None => bin.display().to_string(),
        };
        unsafe {
            std::env::set_var("PATH", path);
        }

        let result = run_command(
            tmp.path(),
            "printf '%s' \"$MUSH_DIRENV_TEST\"",
            10,
            None,
            false,
        )
        .await;

        unsafe {
            if let Some(original) = original_path {
                std::env::set_var("PATH", original);
            } else {
                std::env::remove_var("PATH");
            }
        }

        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(
            text.contains("loaded"),
            "expected bash command to inherit direnv exports, got {text}"
        );
    }

    #[tokio::test]
    async fn dropping_run_command_future_kills_child_process() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("completed");
        let marker_str = marker.display().to_string();
        let cwd = std::env::current_dir().unwrap();
        let cmd = format!("sleep 5 && touch {marker_str}");
        let fut = run_command(&cwd, &cmd, 30, None, false);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), fut).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !marker.exists(),
            "child process survived after future was dropped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_future_kills_bash_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("completed");
        let marker_str = marker.display().to_string();
        let cwd = std::env::current_dir().unwrap();
        let cmd = format!("( sleep 3 && touch {marker_str} ) & wait");
        let fut = run_command(&cwd, &cmd, 30, None, false);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), fut).await;
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "descendant process survived after future was dropped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_bash_descendants_and_returns() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("completed");
        let marker_str = marker.display().to_string();
        let cwd = std::env::current_dir().unwrap();
        let cmd = format!("( sleep 10 && touch {marker_str} ) & wait");
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_command(&cwd, &cmd, 1, None, false),
        )
        .await;
        assert!(
            result.is_ok(),
            "run_command hung instead of returning on timeout"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 5,
            "timeout path took too long: {elapsed:?}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(11)).await;
        assert!(
            !marker.exists(),
            "descendant process survived after timeout kill"
        );
    }

    #[tokio::test]
    async fn run_failing_command() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "exit 1", 10, None, false).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("Exit code: 1"));
        assert!(text.contains("Status: failed"));
    }

    #[tokio::test]
    async fn run_command_with_stderr() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo error >&2", 10, None, false).await;
        let text = extract_text(&result);
        assert!(text.contains("error"));
    }

    #[tokio::test]
    async fn run_command_timeout() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "sleep 30", 1, None, false).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("timed out"));
    }

    #[tokio::test]
    async fn run_json_output() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo hello && echo err >&2", 10, None, true).await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(json["stdout"].as_str().unwrap().contains("hello"));
        assert!(json["stderr"].as_str().unwrap().contains("err"));
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["timed_out"], false);
        assert_eq!(json["status"], "success");
    }

    #[tokio::test]
    async fn run_json_output_failure() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo oops >&2; exit 42", 10, None, true).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["exit_code"], 42);
        assert_eq!(json["status"], "failed");
        assert!(json["stderr"].as_str().unwrap().contains("oops"));
    }

    #[test]
    fn description_warns_against_pre_truncating_output() {
        let tool = BashTool::new(Path::new(".").into());
        let desc = tool.description();
        assert!(
            desc.contains("pre-truncate") || desc.contains("pipe to head"),
            "bash description must warn against manual head/tail/grep \
             on unfamiliar output. got: {desc}"
        );
    }

    #[tokio::test]
    async fn background_returns_job_id() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap().into())
            .with_background_registry(registry.clone());

        let result = tool
            .execute(serde_json::json!({
                "command": "echo hello",
                "background": true
            }))
            .await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("bg_0"), "should contain job id: {text}");
        assert!(text.contains("running"), "should show running: {text}");
    }

    #[tokio::test]
    async fn background_without_registry_errors() {
        let tool = BashTool::new(std::env::current_dir().unwrap().into());
        let result = tool
            .execute(serde_json::json!({
                "command": "echo hello",
                "background": true
            }))
            .await;
        assert!(result.outcome.is_error());
    }

    async fn finished_job(
        registry: &crate::background::BackgroundJobRegistry,
        id: &str,
    ) -> Arc<tokio::sync::RwLock<crate::background::JobState>> {
        let handle = registry.get(id).await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while handle.read().await.status.is_running() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        handle
    }

    #[tokio::test]
    async fn background_job_completes() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap().into())
            .with_background_registry(registry.clone());

        let result = tool
            .execute(serde_json::json!({
                "command": "echo background_done",
                "background": true
            }))
            .await;
        assert!(
            result.outcome.is_success(),
            "background spawn should succeed"
        );

        let handle = finished_job(&registry, "bg_0").await;
        let state = handle.read().await;
        assert!(
            !state.status.is_running(),
            "job should have completed: {:?}",
            state.status
        );
        assert!(
            state.stdout.contains("background_done"),
            "should capture output: {}",
            state.stdout
        );
    }

    #[tokio::test]
    async fn background_job_is_not_killed_by_tool_timeout() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap().into())
            .with_background_registry(registry.clone());

        let result = tool
            .execute(serde_json::json!({
                "command": "sleep 2; echo background_survived",
                "background": true,
                "timeout": 1
            }))
            .await;
        assert!(result.outcome.is_success());

        let handle = finished_job(&registry, "bg_0").await;
        let state = handle.read().await;
        assert_eq!(
            state.status,
            crate::background::JobStatus::Done { exit_code: 0 },
            "background jobs should run until natural completion unless explicitly killed"
        );
        assert!(
            state.stdout.contains("background_survived"),
            "background job should not be killed at the tool timeout boundary: {}",
            state.stdout
        );
    }

    #[tokio::test]
    async fn foreground_timeout_continues_in_background() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap().into())
            .with_background_registry(registry.clone());

        let result = tool
            .execute(serde_json::json!({
                "command": "sleep 2; echo foreground_done",
                "timeout": 1
            }))
            .await;
        let text = extract_text(&result);

        assert!(
            result.outcome.is_success(),
            "foreground timeout should become a background job: {text}"
        );
        assert!(
            text.contains("exceeded tool timeout") && text.contains("bg_0"),
            "result should explain how to poll the background job: {text}"
        );

        let handle = finished_job(&registry, "bg_0").await;
        let state = handle.read().await;
        assert_eq!(
            state.status,
            crate::background::JobStatus::Done { exit_code: 0 }
        );
        assert!(
            state.stdout.contains("foreground_done"),
            "background job should retain command output: {}",
            state.stdout
        );
    }

    #[tokio::test]
    async fn background_concurrency_guard() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap().into())
            .with_background_registry(registry.clone());

        let result = tool
            .execute(serde_json::json!({
                "command": "sleep 10",
                "background": true
            }))
            .await;
        assert!(
            result.outcome.is_success(),
            "first background job should start"
        );

        let result = tool
            .execute(serde_json::json!({
                "command": "echo second",
                "background": true
            }))
            .await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(
            text.contains("concurrent: true"),
            "should suggest flag: {text}"
        );
    }

    #[test]
    fn foreground_timeout_capped_at_max() {
        let args: BashArgs = serde_json::from_value(serde_json::json!({
            "command": "echo hi",
            "timeout": 99999
        }))
        .unwrap();
        assert_eq!(args.effective_timeout(), MAX_FOREGROUND_TIMEOUT_SECS);
    }

    #[test]
    fn foreground_timeout_normal_passthrough() {
        let args: BashArgs = serde_json::from_value(serde_json::json!({
            "command": "echo hi",
            "timeout": 30
        }))
        .unwrap();
        assert_eq!(args.effective_timeout(), 30);
    }

    #[test]
    fn foreground_timeout_default() {
        let args: BashArgs = serde_json::from_value(serde_json::json!({
            "command": "echo hi"
        }))
        .unwrap();
        assert_eq!(args.effective_timeout(), crate::DEFAULT_TOOL_TIMEOUT_SECS);
    }

    #[test]
    fn bash_schema_declares_shared_timeout_default() {
        let tool = BashTool::new(Arc::from(Path::new(".")));
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["properties"]["timeout"]["default"],
            crate::DEFAULT_TOOL_TIMEOUT_SECS
        );
    }

    #[tokio::test]
    async fn no_output_foreground_killed_early() {
        let cwd = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let result =
            run_command_with_no_output_timeout(cwd.path(), "sleep 300", 300, None, false, 2).await;
        let elapsed = start.elapsed();
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(
            text.contains("no output"),
            "should mention no output: {text}"
        );
        assert!(
            elapsed.as_secs() < 10,
            "should be killed after ~2s, took {elapsed:?}"
        );
    }
}
