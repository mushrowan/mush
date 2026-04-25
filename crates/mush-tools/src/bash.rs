//! bash tool - executes shell commands with timeout

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// foreground commands are capped at 10 minutes. for longer tasks
/// (builds, test suites), use background: true which has no cap
const MAX_FOREGROUND_TIMEOUT_SECS: u64 = 600;

/// background jobs without an explicit timeout default to this much
/// wall clock before they get killed. matches the registry's expiry
/// so a caller using only `bash_status` polling is guaranteed to see
/// the final state before the job record is reaped
const BACKGROUND_DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;

/// if a foreground command produces zero bytes of output for this long,
/// kill it. catches servers, daemons, and commands that hang waiting
/// for a TTY (e.g. jj split, jj describe without -m)
const NO_OUTPUT_TIMEOUT_SECS: u64 = 240;

/// drop guard that SIGKILLs the bash child's process group when the tool
/// future is cancelled (user abort, agent-loop cancel, dropped pane).
///
/// `tokio::process::Child::kill_on_drop` only SIGKILLs the direct child.
/// pipelines and backgrounded subshells (`cargo test | tail`,
/// `(sleep) & wait`, etc.) inherit the process group set by
/// `cmd.process_group(0)` but would otherwise keep running after bash
/// itself is dead. a surviving descendant that still holds the stdout
/// write-end also blocks the stream_handle await forever.
///
/// all explicit kill paths (timeout, no-output) call `kill_now()` before
/// awaiting the stream handles so the pipe closes and the handles drain.
/// paths that saw the child exit naturally call `disarm()` so the drop
/// kill becomes a no-op and we never SIGKILL a recycled pid.
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

    /// send SIGKILL to the whole group right now and disarm the drop
    /// kill. harmless if the group is already empty (ESRCH).
    fn kill_now(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // safety: libc::kill with a negated pgid targets the whole
            // group and has no safety preconditions beyond a valid signal
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }

    /// disarm after the child has exited naturally so we don't send
    /// a stray SIGKILL to a pid that may have been recycled
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
    /// explicit timeout override (seconds). when omitted, defaults depend on
    /// background mode: foreground = DEFAULT_TIMEOUT_SECS, background =
    /// BACKGROUND_DEFAULT_TIMEOUT_SECS. the UI-facing default is still 120
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    output: BashOutput,
    /// run the command in the background and return a job id immediately.
    /// use bash_status to poll for completion. keeps API cache warm during
    /// long-running commands like nix builds or large test suites
    #[serde(default)]
    background: bool,
    /// allow starting a background job when one is already running.
    /// defaults to false to prevent accidental CPU overload.
    /// hard-capped at 3 concurrent background jobs regardless
    #[serde(default)]
    concurrent: bool,
}

impl BashArgs {
    /// effective timeout.
    ///
    /// background jobs without an explicit value get a generous half-hour
    /// so typical nix builds and big test runs finish well before the
    /// inner `tokio::time::timeout` fires. foreground commands keep the
    /// 120s default and get hard-capped at 10 minutes.
    fn effective_timeout(&self) -> u64 {
        if self.background {
            self.timeout.unwrap_or(BACKGROUND_DEFAULT_TIMEOUT_SECS)
        } else {
            self.timeout
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_FOREGROUND_TIMEOUT_SECS)
        }
    }
}

/// sender for streaming partial output lines from bash
/// detect a command pattern that would hang waiting on `$EDITOR` in
/// non-interactive use, and return a helpful error explaining how to
/// rephrase it. returns `None` for commands that look safe.
///
/// `EDITOR=false` is also set in the child env as a safety net so even
/// patterns this function misses fail fast (with a less helpful error)
/// instead of hanging indefinitely. this function exists to upgrade the
/// most common cases to an actionable message before we even spawn the
/// process
#[must_use]
pub(crate) fn editor_hang_warning(command: &str) -> Option<String> {
    // a bash command line can chain segments with `;`, `&&`, `||`, `|`
    // or background them with `&`. check each segment independently so
    // a hang anywhere in the chain still trips the warning
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

/// split a command line on shell metachars that separate distinct
/// commands. simple lexer: tracks single/double quotes so chars inside
/// quoted strings are passed through verbatim. close enough for the
/// patterns we care about (jj/git/crontab subcommand detection)
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
    // tokenise on whitespace. simple split is fine: we're only looking
    // at the first 2-3 tokens to recognise a subcommand and we tolerate
    // quoted args being mangled because we never inspect them anyway
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

/// look for `-m`, `-m=value`, `--message`, `--message=value`, or `--stdin`
fn has_message_flag(tokens: &[&str]) -> bool {
    tokens.iter().any(|t| {
        *t == "-m"
            || t.starts_with("-m=")
            || *t == "--message"
            || t.starts_with("--message=")
            || *t == "--stdin"
    })
}

pub type OutputSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub struct BashTool {
    cwd: Arc<Path>,
    /// optional callback for streaming output lines as they arrive
    output_sink: Option<OutputSink>,
    /// shared registry for background jobs (None = background mode disabled)
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
        self.output_sink = Some(sink);
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
         If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds. \
         For long-running commands (builds, test suites), set background: true to run \
         asynchronously and poll with bash_status to avoid cache busting. \
         For searching file contents, use the Grep tool instead of running grep or rg via bash. \
         For locating files, use the Find or Glob tools instead of running find via bash. \
         Do not run interactive commands (e.g. vim, python REPL, less) as stdin is not available. \
         Commands like `jj commit`, `git commit` or `crontab -e` without an explicit -m/--message \
         will fail fast because EDITOR is set to `false` in the child environment - always pass \
         the message explicitly on the command line. \
         Commands that produce no output for 240s are killed automatically."
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
                    "description": "timeout in seconds (optional, default 120)"
                },
                "output": {
                    "type": "string",
                    "description": "output format: 'text' (default) or 'json' with structured fields (stdout, stderr, exit_code, timed_out)",
                    "enum": ["text", "json"]
                },
                "background": {
                    "type": "boolean",
                    "description": "run in background, return job id immediately. poll with bash_status tool. use for commands >2min to keep cache warm"
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

        run_command(
            &self.cwd,
            &args.command,
            args.effective_timeout(),
            self.output_sink.as_ref(),
            args.output.is_json(),
        )
        .await
    }
}

impl BashTool {
    async fn execute_background(&self, args: BashArgs) -> ToolResult {
        let registry = match &self.bg_registry {
            Some(r) => r,
            None => {
                return ToolResult::error(
                    "background execution not available (no job registry configured)",
                );
            }
        };

        if let Err(msg) = registry.check_can_start(args.concurrent).await {
            return ToolResult::error(msg);
        }

        let id = registry.next_id();
        let state = crate::background::JobState {
            id: id.clone(),
            command: args.command.clone(),
            status: crate::background::JobStatus::Running,
            stdout: String::new(),
            stderr: String::new(),
            started: std::time::Instant::now(),
            cwd: self.cwd.clone(),
        };

        let handle = registry.insert(state).await;
        let timeout_secs = args.effective_timeout();
        let command = args.command.clone();
        let cwd = self.cwd.clone();

        // spawn the command in a background task
        tokio::spawn(async move {
            run_background_command(handle, &cwd, &command, timeout_secs).await;
        });

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

/// run a command and stream output into a shared job state
async fn run_background_command(
    handle: std::sync::Arc<tokio::sync::RwLock<crate::background::JobState>>,
    cwd: &std::path::Path,
    command: &str,
    timeout_secs: u64,
) {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // same editor-launch guard as run_command_with_no_output_timeout
    for var in ["EDITOR", "VISUAL", "GIT_EDITOR", "JJ_EDITOR"] {
        cmd.env(var, "false");
    }

    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut state = handle.write().await;
            state.status = crate::background::JobStatus::Failed(format!("failed to spawn: {e}"));
            return;
        }
    };

    // kill the whole process group if this task is cancelled (jj abandon,
    // pane close, agent shutdown) so nothing is left running in the
    // background after the registry drops the job.
    let mut pg_guard = ProcessGroupKillOnDrop::new(&child);

    let timeout = tokio::time::Duration::from_secs(timeout_secs);
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_handle = tokio::spawn(stream_pipe(stdout_pipe, None, None));
    let stderr_handle = tokio::spawn(stream_pipe(stderr_pipe, None, None));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            pg_guard.disarm();
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
            let mut state = handle.write().await;
            state.stdout = stdout;
            state.stderr = stderr;
            state.status = crate::background::JobStatus::Failed(format!("command failed: {e}"));
            return;
        }
        Err(_) => {
            // timeout fired. the child might have exited at the same instant,
            // so try_wait first: if it reaped naturally, prefer the real exit
            // code over TimedOut so the agent doesn't see spurious timeouts
            // on long-but-finite builds that land right on the boundary
            let reaped = child.try_wait().ok().flatten();
            if reaped.is_some() {
                pg_guard.disarm();
            } else {
                // kill the whole group so descendants release the pipes
                // and the stream handles can drain to EOF
                pg_guard.kill_now();
            }
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
            let mut state = handle.write().await;
            state.stdout = stdout;
            state.stderr = stderr;
            state.status = match reaped {
                Some(status) => crate::background::JobStatus::Done {
                    exit_code: status.code().unwrap_or(-1),
                },
                None => crate::background::JobStatus::TimedOut,
            };
            return;
        }
    };

    pg_guard.disarm();
    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    let mut state = handle.write().await;
    state.stdout = stdout;
    state.stderr = stderr;
    state.status = crate::background::JobStatus::Done { exit_code };
}

async fn run_command(
    cwd: &std::path::Path,
    command: &str,
    timeout_secs: u64,
    output_sink: Option<&OutputSink>,
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
    output_sink: Option<&OutputSink>,
    json_output: bool,
    no_output_timeout_secs: u64,
) -> ToolResult {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // if the tool future is dropped (agent cancellation, pane closed,
        // user abort), the child process gets SIGKILLed rather than
        // continuing to run as an orphan consuming cpu / holding files
        .kill_on_drop(true);

    // prevent accidental editor launches (`jj commit`, `git commit`,
    // `crontab -e`, ...) from hanging the agent forever. the child
    // fails with a non-zero exit the llm can read and retry with -m
    for var in ["EDITOR", "VISUAL", "GIT_EDITOR", "JJ_EDITOR"] {
        cmd.env(var, "false");
    }

    // isolate child from the TUI's process group so it can't write to the
    // controlling terminal (which would inject bytes into crossterm's parser)
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("failed to spawn command: {e}")),
    };

    // on cancel/drop, kill the whole process group so pipelines and
    // backgrounded subshells die with bash. kill_on_drop(true) alone
    // leaves descendants orphaned and holding the stdout pipe open,
    // which hangs stdout_handle.await on the timeout/no-output paths.
    let mut pg_guard = ProcessGroupKillOnDrop::new(&child);

    let started = std::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);
    let no_output_timeout = tokio::time::Duration::from_secs(no_output_timeout_secs);

    // track whether any output has been produced
    let has_output = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_handle = tokio::spawn(stream_pipe(
        stdout_pipe,
        output_sink.cloned(),
        Some(has_output.clone()),
    ));
    let stderr_handle = tokio::spawn(stream_pipe(
        stderr_pipe,
        output_sink.cloned(),
        Some(has_output.clone()),
    ));

    // race: process exit vs overall timeout vs no-output timeout
    enum WaitResult {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        NoOutput,
    }

    let has_output_ref = has_output.clone();
    let result = tokio::select! {
        status = child.wait() => WaitResult::Exited(status),
        () = tokio::time::sleep(timeout) => WaitResult::TimedOut,
        () = async {
            tokio::time::sleep(no_output_timeout).await;
            if has_output_ref.load(std::sync::atomic::Ordering::Relaxed) {
                // output was produced, don't trigger no-output kill.
                // let the overall timeout handle long-running commands
                std::future::pending::<()>().await;
            }
        } => WaitResult::NoOutput,
    };

    match result {
        WaitResult::Exited(Ok(status)) => {
            pg_guard.disarm();
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
            let exit_code = status.code().unwrap_or(-1);
            let duration = started.elapsed();
            format_result(stdout, stderr, exit_code, false, duration, json_output)
        }
        WaitResult::Exited(Err(e)) => {
            pg_guard.disarm();
            ToolResult::error(format!("command failed: {e}"))
        }
        WaitResult::TimedOut => {
            // kill the whole group (bash + cargo + tail + ...) so the
            // stdout pipe drains and stdout_handle.await returns. killing
            // only bash leaves descendants holding the write end, which
            // would hang this function forever.
            pg_guard.kill_now();
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
            let duration = started.elapsed();
            if json_output {
                return format_result(stdout, stderr, -1, true, duration, true);
            }
            ToolResult::error(format!("command timed out after {timeout_secs}s"))
        }
        WaitResult::NoOutput => {
            pg_guard.kill_now();
            let stdout = stdout_handle.await.unwrap_or_default();
            let stderr = stderr_handle.await.unwrap_or_default();
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

fn format_result(
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    duration: std::time::Duration,
    json_output: bool,
) -> ToolResult {
    let duration_secs = (duration.as_secs_f32() * 10.0).round() / 10.0;

    if json_output {
        let json = serde_json::json!({
            "stdout": &stdout,
            "stderr": &stderr,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "duration_seconds": duration_secs,
            "stdout_lines": stdout.lines().count(),
            "stderr_lines": stderr.lines().count(),
            "stdout_bytes": stdout.len(),
            "stderr_bytes": stderr.len(),
        });
        if exit_code != 0 || timed_out {
            ToolResult {
                content: vec![mush_ai::types::ToolResultContentPart::Text(
                    mush_ai::types::TextContent {
                        text: json.to_string(),
                    },
                )],
                outcome: mush_ai::types::ToolOutcome::Error,
            }
        } else {
            ToolResult::text(json.to_string())
        }
    } else {
        let mut output = String::new();

        if !stdout.is_empty() {
            output.push_str(&stdout);
        }

        if !stderr.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&stderr);
        }

        if output.is_empty() {
            output = "(no output)".into();
        }

        // structured preamble so the model sees exit status without parsing
        let preamble = format!("Exit code: {exit_code}\nDuration: {duration_secs}s\n\n");

        // agent loop handles truncation (middle-out with actionable hint)
        if exit_code != 0 {
            let text = format!("{preamble}{output}");
            ToolResult {
                content: vec![mush_ai::types::ToolResultContentPart::Text(
                    mush_ai::types::TextContent { text },
                )],
                outcome: mush_ai::types::ToolOutcome::Error,
            }
        } else {
            ToolResult::text(format!("{preamble}{output}"))
        }
    }
}

/// read a pipe line-by-line, forwarding to sink with throttling
async fn stream_pipe<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    sink: Option<OutputSink>,
    has_output: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> String {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut reader = BufReader::new(pipe);
    let mut output = String::new();
    let mut line = String::new();
    let mut last_emit = std::time::Instant::now();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                output.push_str(&line);
                if let Some(ref flag) = has_output {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // throttle sink to avoid flooding the TUI (~10 updates/sec)
                if let Some(ref sink) = sink
                    && last_emit.elapsed() >= std::time::Duration::from_millis(100)
                {
                    sink(line.trim_end());
                    last_emit = std::time::Instant::now();
                }
            }
            Err(_) => break,
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;

    #[tokio::test]
    async fn run_echo_command() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo hello", 10, None, false).await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("hello"));
    }

    #[test]
    fn editor_hang_warning_flags_jj_describe_without_message() {
        // common pitfall: `jj describe` opens $EDITOR. without -m the
        // command would hang in non-interactive use. surface a clear
        // error pointing at the missing flag instead of letting the
        // EDITOR=false fallback emit something cryptic
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
        // -l (list) and -r (remove) don't open the editor
        assert!(editor_hang_warning("crontab -l").is_none());
    }

    #[test]
    fn editor_hang_warning_handles_chained_commands() {
        // first segment is fine, second would hang. each segment must
        // be checked independently so the warning catches the bad one
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
        // commands like `jj commit` or `git commit` without -m try to
        // launch $EDITOR in a non-interactive context and hang forever.
        // we force EDITOR/VISUAL/GIT_EDITOR/JJ_EDITOR = false so the
        // child immediately fails with a clear error that the agent
        // can read and retry correctly.
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo editor=$EDITOR", 10, None, false).await;
        let text = extract_text(&result);
        assert!(
            text.contains("editor=false"),
            "expected EDITOR=false in child env, got {text}"
        );
    }

    #[tokio::test]
    async fn dropping_run_command_future_kills_child_process() {
        // kill_on_drop(true) means an aborted tool future SIGKILLs the
        // bash child so sleeps / servers / editor-hangs don't linger
        // past cancellation. use a marker tempfile that only gets
        // touched if the child is allowed to finish its sleep.
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("completed");
        let marker_str = marker.display().to_string();
        let cwd = std::env::current_dir().unwrap();
        // 5s sleep then touch the marker. abort after 200ms.
        let cmd = format!("sleep 5 && touch {marker_str}");
        let fut = run_command(&cwd, &cmd, 30, None, false);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), fut).await;
        // give the OS a moment to settle, then confirm the marker never
        // got created because the bash child was killed mid-sleep
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !marker.exists(),
            "child process survived after future was dropped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_future_kills_bash_descendants() {
        // regression: kill_on_drop(true) only SIGKILLs the direct bash
        // child. descendants spawned via subshells, pipelines, or & keep
        // running after bash dies because they land in a different parent
        // but share the same process group. we must SIGKILL the whole
        // group so infinite-loop tests (cargo nextest | tail, etc.) die
        // when the user aborts.
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("completed");
        let marker_str = marker.display().to_string();
        let cwd = std::env::current_dir().unwrap();
        // bash backgrounds a subshell that outlives bash's foreground
        // wait. if we only kill the bash pid, the subshell sleeps then
        // touches the marker.
        let cmd = format!("( sleep 3 && touch {marker_str} ) & wait");
        let fut = run_command(&cwd, &cmd, 30, None, false);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), fut).await;
        // wait past the subshell's sleep so a surviving descendant
        // would have created the marker by now
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "descendant process survived after future was dropped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_bash_descendants_and_returns() {
        // regression: on timeout we used to SIGKILL only bash and then
        // await the stdout handle. if bash had spawned descendants that
        // held the pipe open (e.g. cargo test | tail), read_line blocks
        // forever and the tool future hangs, so the agent loop never
        // sees the timeout. killing the whole group closes the pipe and
        // lets the stream drain.
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("completed");
        let marker_str = marker.display().to_string();
        let cwd = std::env::current_dir().unwrap();
        // bash backgrounds a descendant that holds stdout open. a
        // surviving descendant keeps the pipe open and would block the
        // stdout handle forever.
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
        // give any surviving descendant its chance to finish the sleep.
        // if the group kill worked, the marker is never created.
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
    }

    #[tokio::test]
    async fn run_json_output_failure() {
        let cwd = std::env::current_dir().unwrap();
        let result = run_command(&cwd, "echo oops >&2; exit 42", 10, None, true).await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["exit_code"], 42);
        assert!(json["stderr"].as_str().unwrap().contains("oops"));
    }

    #[test]
    fn output_limit_is_tail() {
        use mush_agent::tool::OutputLimit;
        let tool = BashTool::new(Path::new(".").into());
        assert_eq!(tool.output_limit(), OutputLimit::Tail);
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

        // wait a bit for the background task to finish
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let handle = registry.get("bg_0").await.unwrap();
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
    async fn background_concurrency_guard() {
        let registry = crate::background::BackgroundJobRegistry::new();
        let tool = BashTool::new(std::env::current_dir().unwrap().into())
            .with_background_registry(registry.clone());

        // start a long-running background job
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

        // second job without concurrent flag should fail
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
        // agent passes an absurdly large timeout, should be capped
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
        assert_eq!(args.effective_timeout(), DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn background_timeout_not_capped() {
        // background jobs use their own timeout without the foreground cap
        let args: BashArgs = serde_json::from_value(serde_json::json!({
            "command": "nix build",
            "timeout": 3600,
            "background": true
        }))
        .unwrap();
        assert_eq!(args.effective_timeout(), 3600);
    }

    #[test]
    fn background_timeout_defaults_to_thirty_minutes_when_unset() {
        // regression: an unset timeout used to inherit the 120s foreground
        // default, which killed nix builds / long test runs around the 2
        // minute mark and reported them as TimedOut. background jobs must
        // default to a generous wall-clock so bash_status polling observes
        // natural completion
        let args: BashArgs = serde_json::from_value(serde_json::json!({
            "command": "nix build",
            "background": true
        }))
        .unwrap();
        assert_eq!(args.effective_timeout(), BACKGROUND_DEFAULT_TIMEOUT_SECS);
        assert!(
            args.effective_timeout() >= 30 * 60,
            "background default should give nix builds and big test runs enough room to finish"
        );
    }

    #[test]
    fn background_timeout_explicit_override_honoured() {
        // when the agent knows the job is quick it can still shorten the
        // timeout explicitly
        let args: BashArgs = serde_json::from_value(serde_json::json!({
            "command": "sleep 5",
            "background": true,
            "timeout": 10
        }))
        .unwrap();
        assert_eq!(args.effective_timeout(), 10);
    }

    #[tokio::test]
    async fn no_output_foreground_killed_early() {
        let cwd = std::env::current_dir().unwrap();
        // sleep produces no output, should be killed after no_output_timeout
        let start = std::time::Instant::now();
        // use a short no-output timeout (2s) to keep the test fast
        let result =
            run_command_with_no_output_timeout(&cwd, "sleep 300", 300, None, false, 2).await;
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
