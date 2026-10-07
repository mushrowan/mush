use std::fmt::Write as _;
use std::time::Duration;

use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    PreSession,
    PreToolUse,
    PostToolUse,
    Stop,
    PostCompaction,
}

#[derive(Debug, Clone)]
pub struct LifecycleHook {
    pub tool_match: String,
    pub command: String,
    pub timeout: Duration,
    pub blocking: bool,
}

impl LifecycleHook {
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        if self.tool_match == "*" {
            return true;
        }
        self.tool_match
            .split('|')
            .any(|pattern| pattern.trim().eq_ignore_ascii_case(tool_name))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HookResult {
    pub success: bool,
    pub output: String,
    pub command: String,
    pub blocking: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LifecycleHooks {
    hooks: std::collections::HashMap<HookPoint, Vec<LifecycleHook>>,
}

impl LifecycleHooks {
    pub fn is_empty(&self) -> bool {
        self.hooks.values().all(|v| v.is_empty())
    }

    pub fn for_point(&self, point: HookPoint) -> &[LifecycleHook] {
        self.hooks.get(&point).map_or(&[], |v| v.as_slice())
    }

    pub fn set(&mut self, point: HookPoint, hooks: Vec<LifecycleHook>) {
        self.hooks.insert(point, hooks);
    }

    pub fn push(&mut self, point: HookPoint, hook: LifecycleHook) {
        self.hooks.entry(point).or_default().push(hook);
    }

    pub async fn run_all(
        &self,
        point: HookPoint,
        cwd: Option<&std::path::Path>,
    ) -> Vec<HookResult> {
        self.run_matching(point, cwd, |_| true).await
    }

    pub async fn run_for_tool(
        &self,
        point: HookPoint,
        tool_name: &str,
        cwd: Option<&std::path::Path>,
    ) -> Vec<HookResult> {
        self.run_matching(point, cwd, |hook| hook.matches_tool(tool_name))
            .await
    }

    async fn run_matching(
        &self,
        point: HookPoint,
        cwd: Option<&std::path::Path>,
        matches: impl Fn(&LifecycleHook) -> bool,
    ) -> Vec<HookResult> {
        let mut results = Vec::new();
        for hook in self.for_point(point).iter().filter(|hook| matches(hook)) {
            results.push(run_hook(hook, cwd).await);
        }
        results
    }
}

async fn run_hook(hook: &LifecycleHook, cwd: Option<&std::path::Path>) -> HookResult {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&hook.command);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let (success, output) = match tokio::time::timeout(hook.timeout, cmd.output()).await {
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
            (output.status.success(), combined)
        }
        Ok(Err(e)) => (false, format!("failed to run hook: {e}")),
        Err(_) => (
            false,
            format!("hook timed out after {}s", hook.timeout.as_secs()),
        ),
    };
    HookResult {
        success,
        output,
        command: hook.command.clone(),
        blocking: hook.blocking,
    }
}

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
        let _ = writeln!(out, "[{label} failed: `{}`]", r.command);
        if !r.output.is_empty() {
            out.push_str(&r.output);
            if !r.output.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    Some(out)
}

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
