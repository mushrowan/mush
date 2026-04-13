//! lifecycle hooks: user-configured shell commands that run at
//! specific points in the agent loop
//!
//! - PreToolUse: before a tool executes (can block)
//! - PostToolUse: after a tool executes (linters, formatters)
//! - Stop: before the agent declares done (test gates)

use std::time::Duration;

use tokio::process::Command;

/// when in the lifecycle the hook fires
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// runs once at session start, before the first LLM call
    PreSession,
    PreToolUse,
    PostToolUse,
    Stop,
    /// runs after context compaction (auto or manual)
    PostCompaction,
}

/// a user-configured lifecycle hook
#[derive(Debug, Clone)]
pub struct LifecycleHook {
    /// which tools this applies to: "*" for all, "edit|write" for specific tools
    pub tool_match: String,
    /// shell command to run
    pub command: String,
    /// timeout (default 30s)
    pub timeout: Duration,
    /// if true, failure blocks the operation and feeds output back to the model
    pub blocking: bool,
}

impl LifecycleHook {
    /// whether this hook matches a given tool name
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        if self.tool_match == "*" {
            return true;
        }
        self.tool_match
            .split('|')
            .any(|pattern| pattern.trim().eq_ignore_ascii_case(tool_name))
    }
}

/// result of running a lifecycle hook
#[derive(Debug, Clone)]
pub struct HookResult {
    pub success: bool,
    pub output: String,
    pub command: String,
    /// whether this hook's failure should block the operation
    pub blocking: bool,
}

/// collection of lifecycle hooks by point
#[derive(Debug, Clone, Default)]
pub struct LifecycleHooks {
    hooks: std::collections::HashMap<HookPoint, Vec<LifecycleHook>>,
}

impl LifecycleHooks {
    pub fn is_empty(&self) -> bool {
        self.hooks.values().all(|v| v.is_empty())
    }

    /// get hooks for a specific point
    pub fn for_point(&self, point: HookPoint) -> &[LifecycleHook] {
        self.hooks.get(&point).map_or(&[], |v| v.as_slice())
    }

    /// replace all hooks for a point
    pub fn set(&mut self, point: HookPoint, hooks: Vec<LifecycleHook>) {
        self.hooks.insert(point, hooks);
    }

    /// add a single hook for a point
    pub fn push(&mut self, point: HookPoint, hook: LifecycleHook) {
        self.hooks.entry(point).or_default().push(hook);
    }

    /// run all hooks at a point (not filtered by tool name)
    pub async fn run_all(
        &self,
        point: HookPoint,
        cwd: Option<&std::path::Path>,
    ) -> Vec<HookResult> {
        let mut results = Vec::new();
        for hook in self.for_point(point) {
            results.push(run_hook(hook, cwd).await);
        }
        results
    }

    /// run all matching hooks for a tool at a given lifecycle point
    ///
    /// returns results for hooks that produced output or failed.
    /// hooks run sequentially (order matters for linters, etc.)
    pub async fn run_for_tool(
        &self,
        point: HookPoint,
        tool_name: &str,
        cwd: Option<&std::path::Path>,
    ) -> Vec<HookResult> {
        let mut results = Vec::new();
        for hook in self.for_point(point) {
            if !hook.matches_tool(tool_name) {
                continue;
            }
            results.push(run_hook(hook, cwd).await);
        }
        results
    }
}

/// execute a single hook command
async fn run_hook(hook: &LifecycleHook, cwd: Option<&std::path::Path>) -> HookResult {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&hook.command);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let result = tokio::time::timeout(hook.timeout, cmd.output()).await;
    let blocking = hook.blocking;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = if stderr.is_empty() {
                stdout.into_owned()
            } else if stdout.is_empty() {
                stderr.into_owned()
            } else {
                format!("{stdout}\n{stderr}")
            };

            HookResult {
                success: output.status.success(),
                output: combined,
                command: hook.command.clone(),
                blocking,
            }
        }
        Ok(Err(e)) => HookResult {
            success: false,
            output: format!("failed to run hook: {e}"),
            command: hook.command.clone(),
            blocking,
        },
        Err(_) => HookResult {
            success: false,
            output: format!("hook timed out after {}s", hook.timeout.as_secs()),
            command: hook.command.clone(),
            blocking,
        },
    }
}

/// format hook results into text suitable for injection into the conversation
pub fn format_hook_results(results: &[HookResult], point: HookPoint) -> Option<String> {
    let failures: Vec<&HookResult> = results.iter().filter(|r| !r.success).collect();
    if failures.is_empty() {
        return None;
    }

    let label = match point {
        HookPoint::PreSession => "pre-session hook",
        HookPoint::PreToolUse => "pre-tool hook",
        HookPoint::PostToolUse => "post-tool hook",
        HookPoint::Stop => "stop hook",
        HookPoint::PostCompaction => "post-compaction hook",
    };

    let mut out = String::new();
    for r in &failures {
        out.push_str(&format!("[{label} failed: `{}`]\n", r.command));
        if !r.output.is_empty() {
            out.push_str(&r.output);
            if !r.output.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    Some(out)
}

/// whether any result is a blocking failure
pub fn has_blocking_failure(results: &[HookResult]) -> bool {
    results.iter().any(|r| !r.success && r.blocking)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(tool_match: &str, command: &str) -> LifecycleHook {
        LifecycleHook {
            tool_match: tool_match.into(),
            command: command.into(),
            timeout: Duration::from_secs(5),
            blocking: false,
        }
    }

    fn blocking_hook(tool_match: &str, command: &str) -> LifecycleHook {
        LifecycleHook {
            blocking: true,
            ..hook(tool_match, command)
        }
    }

    fn post_hooks(hooks: Vec<LifecycleHook>) -> LifecycleHooks {
        let mut lh = LifecycleHooks::default();
        lh.set(HookPoint::PostToolUse, hooks);
        lh
    }

    #[test]
    fn push_accumulates_hooks() {
        let mut hooks = LifecycleHooks::default();
        hooks.push(HookPoint::PreToolUse, hook("*", "echo a"));
        hooks.push(HookPoint::PreToolUse, hook("edit", "echo b"));
        assert_eq!(hooks.for_point(HookPoint::PreToolUse).len(), 2);
        assert!(hooks.for_point(HookPoint::PostToolUse).is_empty());
    }

    #[test]
    fn matches_wildcard() {
        let h = hook("*", "echo ok");
        assert!(h.matches_tool("edit"));
        assert!(h.matches_tool("bash"));
    }

    #[test]
    fn matches_pipe_separated() {
        let h = hook("edit|write", "echo ok");
        assert!(h.matches_tool("edit"));
        assert!(h.matches_tool("write"));
        assert!(!h.matches_tool("bash"));
    }

    #[test]
    fn matches_case_insensitive() {
        let h = hook("Edit", "echo ok");
        assert!(h.matches_tool("edit"));
        assert!(h.matches_tool("EDIT"));
    }

    #[test]
    fn matches_with_whitespace() {
        let h = hook(" edit | write ", "echo ok");
        assert!(h.matches_tool("edit"));
        assert!(h.matches_tool("write"));
    }

    #[test]
    fn lifecycle_hooks_empty_and_not() {
        assert!(LifecycleHooks::default().is_empty());
        assert!(!post_hooks(vec![hook("*", "echo ok")]).is_empty());
    }

    #[test]
    fn for_point_returns_correct_hooks() {
        let h = hook("*", "echo ok");
        let mut hooks = LifecycleHooks::default();
        hooks.set(HookPoint::PreToolUse, vec![h.clone()]);
        hooks.set(HookPoint::PostToolUse, vec![h.clone(), h.clone()]);
        assert_eq!(hooks.for_point(HookPoint::PreSession).len(), 0);
        assert_eq!(hooks.for_point(HookPoint::PreToolUse).len(), 1);
        assert_eq!(hooks.for_point(HookPoint::PostToolUse).len(), 2);
        assert_eq!(hooks.for_point(HookPoint::Stop).len(), 0);
    }

    #[tokio::test]
    async fn run_successful_hook() {
        let hooks = post_hooks(vec![hook("*", "echo hello")]);
        let results = hooks
            .run_for_tool(HookPoint::PostToolUse, "edit", None)
            .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
        assert!(!results[0].blocking);
        assert!(results[0].output.contains("hello"));
    }

    #[tokio::test]
    async fn run_failing_blocking_hook() {
        let hooks = post_hooks(vec![blocking_hook("*", "exit 1")]);
        let results = hooks
            .run_for_tool(HookPoint::PostToolUse, "edit", None)
            .await;
        assert!(!results[0].success);
        assert!(results[0].blocking);
        assert!(has_blocking_failure(&results));
    }

    #[tokio::test]
    async fn non_blocking_failure_does_not_block() {
        let hooks = post_hooks(vec![hook("*", "exit 1")]);
        let results = hooks
            .run_for_tool(HookPoint::PostToolUse, "edit", None)
            .await;
        assert!(!results[0].success);
        assert!(!has_blocking_failure(&results));
    }

    #[tokio::test]
    async fn hook_timeout() {
        let hooks = post_hooks(vec![LifecycleHook {
            timeout: Duration::from_millis(100),
            ..hook("*", "sleep 10")
        }]);
        let results = hooks
            .run_for_tool(HookPoint::PostToolUse, "edit", None)
            .await;
        assert!(!results[0].success);
        assert!(results[0].output.contains("timed out"));
    }

    #[tokio::test]
    async fn hook_skips_non_matching_tools() {
        let hooks = post_hooks(vec![hook("edit|write", "echo matched")]);

        let results = hooks
            .run_for_tool(HookPoint::PostToolUse, "bash", None)
            .await;
        assert!(results.is_empty());

        let results = hooks
            .run_for_tool(HookPoint::PostToolUse, "edit", None)
            .await;
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn run_stop_hooks() {
        let mut hooks = LifecycleHooks::default();
        hooks.set(
            HookPoint::Stop,
            vec![blocking_hook("*", "echo stop check passed")],
        );
        let results = hooks.run_all(HookPoint::Stop, None).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[test]
    fn format_hook_results_none_on_success() {
        let results = vec![HookResult {
            success: true,
            output: "all good".into(),
            command: "echo ok".into(),
            blocking: false,
        }];
        assert!(format_hook_results(&results, HookPoint::PostToolUse).is_none());
    }

    #[test]
    fn format_hook_results_shows_failures() {
        let results = vec![
            HookResult {
                success: true,
                output: "fine".into(),
                command: "echo ok".into(),
                blocking: false,
            },
            HookResult {
                success: false,
                output: "error: unused variable".into(),
                command: "cargo clippy".into(),
                blocking: true,
            },
        ];
        let formatted = format_hook_results(&results, HookPoint::PostToolUse).unwrap();
        assert!(formatted.contains("post-tool hook failed"));
        assert!(formatted.contains("cargo clippy"));
        assert!(formatted.contains("unused variable"));
    }

    #[tokio::test]
    async fn hook_captures_stderr() {
        let hooks = post_hooks(vec![hook("*", "echo error >&2")]);
        let results = hooks
            .run_for_tool(HookPoint::PostToolUse, "edit", None)
            .await;
        assert!(results[0].output.contains("error"));
    }

    #[test]
    fn post_compaction_hooks_in_lifecycle() {
        let h = hook("*", "echo compacted");
        let mut hooks = LifecycleHooks::default();
        hooks.set(HookPoint::PostCompaction, vec![h]);
        assert!(!hooks.is_empty());
        assert_eq!(hooks.for_point(HookPoint::PostCompaction).len(), 1);
    }

    #[tokio::test]
    async fn run_post_compaction_hooks() {
        let mut hooks = LifecycleHooks::default();
        hooks.set(
            HookPoint::PostCompaction,
            vec![hook("*", "echo rules preserved")],
        );
        let results = hooks.run_all(HookPoint::PostCompaction, None).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
        assert!(results[0].output.contains("rules preserved"));
    }

    #[test]
    fn format_post_compaction_hook_failure() {
        let results = vec![HookResult {
            success: false,
            output: "failed to save state".into(),
            command: "save-context.sh".into(),
            blocking: false,
        }];
        let formatted = format_hook_results(&results, HookPoint::PostCompaction).unwrap();
        assert!(formatted.contains("post-compaction hook failed"));
        assert!(formatted.contains("save-context.sh"));
    }

    #[tokio::test]
    async fn hook_with_cwd() {
        let hooks = post_hooks(vec![hook("*", "pwd")]);
        let results = hooks
            .run_for_tool(
                HookPoint::PostToolUse,
                "edit",
                Some(std::path::Path::new("/tmp")),
            )
            .await;
        assert!(results[0].success);
        assert!(results[0].output.contains("tmp"));
    }
}
