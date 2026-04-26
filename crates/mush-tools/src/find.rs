//! find tool - file search using fd

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{resolve_path, truncate_lines};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FindType {
    File,
    Directory,
}

impl FindType {
    fn as_fd_arg(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    pattern: String,
    path: Option<String>,
    #[serde(rename = "type")]
    type_filter: Option<FindType>,
}

pub struct FindTool {
    cwd: Arc<Path>,
}

impl FindTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self { cwd }
    }
}

#[async_trait::async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn label(&self) -> &str {
        "Find"
    }
    fn description(&self) -> &str {
        "Search for files and directories by name pattern using fd (regex). Respects .gitignore. \
         Returns matching paths relative to the search directory. \
         For glob patterns like '**/*.rs', use the glob tool instead."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "regex pattern to match file/directory names"
                },
                "path": {
                    "type": "string",
                    "description": "directory to search in (defaults to cwd)"
                },
                "type": {
                    "type": "string",
                    "enum": ["file", "directory"],
                    "description": "restrict results to files or directories"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<FindArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let search_path = args
            .path
            .as_deref()
            .map(|path| resolve_path(&self.cwd, path))
            .unwrap_or_else(|| self.cwd.to_path_buf());

        run_fd(&self.cwd, &args.pattern, &search_path, args.type_filter).await
    }
}

async fn run_fd(
    cwd: &std::path::Path,
    pattern: &str,
    search_path: &std::path::Path,
    type_filter: Option<FindType>,
) -> ToolResult {
    let mut cmd = tokio::process::Command::new("fd");
    cmd.arg("--color=never")
        .arg(pattern)
        .arg(search_path)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(type_filter) = type_filter {
        cmd.arg("--type").arg(type_filter.as_fd_arg());
    }

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => return ToolResult::error(format!("failed to run fd: {e}")),
    };

    // fd exits 0 for both success-with-matches and success-with-no-matches.
    // any non-zero exit (1 = generic error, 2 = arg parsing error) is a
    // real failure and must surface so the agent can correct its inputs
    // rather than misread a silent "no files found"
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let detail = if detail.is_empty() {
            format!("exit status {}", output.status)
        } else {
            detail.to_string()
        };
        return ToolResult::error(format!("fd error: {detail}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.is_empty() {
        return ToolResult::text("no files found");
    }

    let lines: Vec<&str> = stdout.lines().collect();
    ToolResult::text(truncate_lines(&lines, "files"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn find_files_by_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hi\n").unwrap();

        let result = run_fd(dir.path(), r".*\.rs$", dir.path(), Some(FindType::File)).await;
        let text = crate::util::extract_text(&result);
        assert!(text.contains("main.rs"));
        assert!(text.contains("lib.rs"));
        assert!(!text.contains("notes.txt"));
    }

    #[tokio::test]
    async fn find_substring_pattern_matches_filename() {
        // pattern "zx" should find "zx.py" via substring/regex match
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("zx.py"), "").unwrap();
        std::fs::write(dir.path().join("normal.txt"), "").unwrap();

        let result = run_fd(dir.path(), "zx", dir.path(), None).await;
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("zx.py"),
            "pattern 'zx' must match 'zx.py' (got: {text})"
        );
        assert!(!text.contains("normal.txt"));
    }

    #[tokio::test]
    async fn find_no_results() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();

        let result = run_fd(dir.path(), r".*\.py$", dir.path(), None).await;
        let text = crate::util::extract_text(&result);
        assert_eq!(text, "no files found");
    }

    #[tokio::test]
    async fn find_invalid_regex_returns_error_not_silent() {
        // an invalid regex pattern should surface as a clear error so the
        // agent can correct the pattern. previously this was silenced as
        // "no files found" because we mistakenly treated fd exit code 1
        // as a no-results signal (it is actually a generic-error signal;
        // fd uses exit 0 for both success-with-matches and success-with-
        // no-matches)
        let dir = tempfile::TempDir::new().unwrap();
        let result = run_fd(dir.path(), "[", dir.path(), None).await;
        assert!(
            result.outcome.is_error(),
            "invalid regex must produce a tool error",
        );
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("regex"),
            "error should mention the regex parse failure: {text}",
        );
    }

    #[tokio::test]
    async fn find_nonexistent_search_path_returns_error() {
        // fd reports "search path is not a directory" via exit code 1.
        // we must surface this rather than silently claim no matches
        let cwd = tempfile::TempDir::new().unwrap();
        let bogus = cwd.path().join("does-not-exist");
        let result = run_fd(cwd.path(), "anything", &bogus, None).await;
        assert!(
            result.outcome.is_error(),
            "nonexistent path must produce a tool error",
        );
    }
}
