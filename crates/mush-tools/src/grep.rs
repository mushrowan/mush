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
                    "description": "regex pattern to search for",
                    "type": "string"
                },
                "path": {
                    "description": "directory or file to search in (defaults to cwd)",
                    "type": "string"
                },
                "include": {
                    "description": "glob pattern for files to include (e.g. '*.rs')",
                    "type": "string"
                },
                "mode": {
                    "description": "search mode: 'regex' (default) or 'literal' for fixed string matching",
                    "type": "string",
                    "enum": ["regex", "literal"]
                },
                "case_sensitive": {
                    "description": "whether the search is case sensitive (default true)",
                    "type": "boolean"
                },
                "whole_word": {
                    "description": "only match whole words (default false)",
                    "type": "boolean"
                },
                "context_before": {
                    "description": "lines of context to show before each match (default 0)",
                    "type": "integer"
                },
                "context_after": {
                    "description": "lines of context to show after each match (default 0)",
                    "type": "integer"
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
            let pattern = args["pattern"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let path = args["path"].as_str().map(|s| s.to_string());
            let include = args["include"].as_str().map(|s| s.to_string());
            let mode = args["mode"].as_str().unwrap_or("regex");
            let case_sensitive = args["case_sensitive"].as_bool().unwrap_or(true);
            let whole_word = args["whole_word"].as_bool().unwrap_or(false);
            let context_before = args["context_before"].as_u64().unwrap_or(0);
            let context_after = args["context_after"].as_u64().unwrap_or(0);

            if pattern.is_empty() {
                return ToolResult::error("pattern is required");
            }

            let search_path = path
                .as_deref()
                .map(|p| resolve_path(&self.cwd, p))
                .unwrap_or_else(|| self.cwd.clone());

            run_rg(
                &self.cwd,
                &pattern,
                &search_path,
                include.as_deref(),
                mode,
                case_sensitive,
                whole_word,
                context_before,
                context_after,
            )
            .await
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_rg(
    cwd: &std::path::Path,
    pattern: &str,
    search_path: &std::path::Path,
    include: Option<&str>,
    mode: &str,
    case_sensitive: bool,
    whole_word: bool,
    context_before: u64,
    context_after: u64,
) -> ToolResult {
    // strip newlines from pattern - LLMs sometimes include literal newlines
    // which rg rejects with "literal \n is not allowed"
    let pattern: String = pattern
        .chars()
        .filter(|&c| c != '\n' && c != '\r')
        .collect();

    if pattern.is_empty() {
        return ToolResult::error("pattern is empty (after stripping newlines)");
    }

    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--no-heading")
        .arg("--line-number")
        .arg("--color=never")
        .arg("--with-filename")
        .arg("--max-count=50");

    if mode == "literal" {
        cmd.arg("--fixed-strings");
    }

    if !case_sensitive {
        cmd.arg("--ignore-case");
    }

    if whole_word {
        cmd.arg("--word-regexp");
    }

    if context_before > 0 {
        cmd.arg(format!("-B{context_before}"));
    }

    if context_after > 0 {
        cmd.arg(format!("-A{context_after}"));
    }

    if let Some(glob) = include {
        cmd.arg("--glob").arg(glob);
    }

    cmd.arg("--").arg(&pattern).arg(search_path);
    cmd.current_dir(cwd);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => return ToolResult::error(format!("failed to run rg: {e}")),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // rg exit 1 = no matches, exit 2 = error
    if output.status.code() == Some(2) {
        return ToolResult::error(format!("rg error: {stderr}"));
    }

    if stdout.is_empty() {
        return ToolResult::text("No matches found.");
    }

    let lines: Vec<&str> = stdout.lines().collect();
    ToolResult::text(truncate_lines(&lines, "matches"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[tokio::test]
    async fn grep_finds_pattern() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello world\nfoo bar\nhello again").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("hello world"));
        assert!(text.contains("hello again"));
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "nonexistent",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("No matches"));
    }

    #[tokio::test]
    async fn grep_with_include() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("test.txt"), "fn other()").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "fn",
                "path": dir.path().to_str().unwrap(),
                "include": "*.rs"
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("main"));
        assert!(!text.contains("other"));
    }

    #[tokio::test]
    async fn grep_literal_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello.world\nhelloXworld").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello.world",
                "path": dir.path().to_str().unwrap(),
                "mode": "literal"
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("hello.world"));
        // in literal mode, . is not a regex metachar, so helloXworld should not match
        assert!(!text.contains("helloXworld"));
    }

    #[tokio::test]
    async fn grep_case_insensitive() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "Hello World\nhello world\nHELLO").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "case_sensitive": false
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("Hello World"));
        assert!(text.contains("hello world"));
        assert!(text.contains("HELLO"));
    }

    #[tokio::test]
    async fn grep_whole_word() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "cat\ncatch\nthe cat sat").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "cat",
                "path": dir.path().to_str().unwrap(),
                "whole_word": true
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("cat"));
        assert!(text.contains("the cat sat"));
        assert!(!text.contains("catch"));
    }

    #[tokio::test]
    async fn grep_context_lines() {
        let dir = temp_dir();
        fs::write(
            dir.path().join("test.txt"),
            "line1\nline2\nMATCH\nline4\nline5",
        )
        .unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "MATCH",
                "path": dir.path().to_str().unwrap(),
                "context_before": 1,
                "context_after": 1
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("line2"));
        assert!(text.contains("MATCH"));
        assert!(text.contains("line4"));
    }

    #[tokio::test]
    async fn grep_strips_newlines_from_pattern() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "helloworld").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello\nworld",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        assert!(result.outcome.is_success());
        let text = extract_text(&result);
        assert!(text.contains("helloworld"));
    }

    #[tokio::test]
    async fn grep_empty_after_stripping_newlines() {
        let dir = temp_dir();
        fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "\n\n",
                "path": dir.path().to_str().unwrap()
            }))
            .await;
        assert!(result.outcome.is_error());
        let text = extract_text(&result);
        assert!(text.contains("pattern is empty"));
    }
}
