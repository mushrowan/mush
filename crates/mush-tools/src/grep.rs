//! grep tool - pattern search using ripgrep (rg)

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult};

use crate::util::{resolve_path, truncate_lines};

pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn label(&self) -> &str {
        "Grep"
    }
    fn description(&self) -> &str {
        "Search file contents using ripgrep (rg). Respects .gitignore. \
         Returns matching lines with file paths and line numbers. \
         Use this for searching code, config, and text files by content pattern."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "directory or file to search in (defaults to cwd)"
                },
                "include": {
                    "type": "string",
                    "description": "glob pattern for files to include (e.g. '*.rs')"
                }
            },
            "required": ["pattern"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let Some(pattern) = args["pattern"].as_str() else {
                return ToolResult::error("missing required parameter: pattern");
            };

            let search_path = args["path"]
                .as_str()
                .map(|p| resolve_path(&self.cwd, p))
                .unwrap_or_else(|| self.cwd.clone());

            let include = args["include"].as_str();

            run_rg(&self.cwd, pattern, &search_path, include).await
        })
    }
}

async fn run_rg(
    cwd: &std::path::Path,
    pattern: &str,
    search_path: &std::path::Path,
    include: Option<&str>,
) -> ToolResult {
    // strip newlines from pattern - LLMs sometimes include literal newlines
    // which rg rejects with "literal \n is not allowed"
    let pattern: String = pattern.chars().filter(|&c| c != '\n' && c != '\r').collect();

    if pattern.is_empty() {
        return ToolResult::error("pattern is empty (after stripping newlines)");
    }

    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg("--max-count=50") // per-file limit
        .arg(&pattern)
        .arg(search_path)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(glob) = include {
        cmd.arg("--glob").arg(glob);
    }

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => return ToolResult::error(format!("failed to run rg: {e}")),
    };

    // rg exits 1 when no matches found, 2+ for actual errors
    if output.status.code() == Some(2) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return ToolResult::error(format!("rg error: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.is_empty() {
        return ToolResult::text("no matches found");
    }

    let lines: Vec<&str> = stdout.lines().collect();
    ToolResult::text(truncate_lines(&lines, "matches"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    #[tokio::test]
    async fn grep_finds_pattern() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}",
        )
        .unwrap();

        let result = run_rg(dir.path(), "println", dir.path(), None).await;

        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("println"));
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let result = run_rg(dir.path(), "nonexistent_pattern_xyz", dir.path(), None).await;

        let text = extract_text(&result);
        assert!(text.contains("no matches"));
    }

    #[tokio::test]
    async fn grep_strips_newlines_from_pattern() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "helloworld").unwrap();

        // pattern with embedded newlines should still match after stripping
        let result = run_rg(dir.path(), "hello\nworld", dir.path(), None).await;

        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("helloworld"));
    }

    #[tokio::test]
    async fn grep_empty_after_stripping_newlines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let result = run_rg(dir.path(), "\n\n", dir.path(), None).await;

        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("empty"));
    }

    #[tokio::test]
    async fn grep_with_include_glob() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("code.rs"), "fn hello()").unwrap();
        fs::write(dir.path().join("data.txt"), "fn hello()").unwrap();

        let result = run_rg(dir.path(), "hello", dir.path(), Some("*.rs")).await;

        let text = extract_text(&result);
        assert!(text.contains("code.rs"));
        assert!(!text.contains("data.txt"));
    }
}
