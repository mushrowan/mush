//! grep tool - pattern search using ripgrep (rg)

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{resolve_path, truncate_lines};

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GrepMode {
    #[default]
    Regex,
    Literal,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GrepOutput {
    #[default]
    Lines,
    Count,
    Files,
    Json,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    #[serde(default)]
    mode: GrepMode,
    #[serde(default = "default_true")]
    case_sensitive: bool,
    #[serde(default)]
    whole_word: bool,
    #[serde(default)]
    context_before: u64,
    #[serde(default)]
    context_after: u64,
    #[serde(default)]
    output: GrepOutput,
    max_results: Option<usize>,
    top_n: Option<usize>,
}

const fn default_true() -> bool {
    true
}

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
         Use 'count' output to get per-file match counts instead of full lines. \
         Use 'files' output to get just filenames with matches. \
         Prefer this over bash grep/rg for all file content searches."
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
                },
                "output": {
                    "description": "output format: 'lines' (default, full matching lines), 'count' (per-file match counts sorted by count desc), 'files' (just filenames with matches), 'json' (structured per-file counts as JSON array)",
                    "type": "string",
                    "enum": ["lines", "count", "files", "json"]
                },
                "max_results": {
                    "description": "max files to show in count/files output (default: all)",
                    "type": "integer"
                },
                "top_n": {
                    "description": "return only the top N files by match count (for count/json output modes). applied after sorting by count desc",
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
            let args = match parse_tool_args::<GrepArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            if args.pattern.is_empty() {
                return ToolResult::error("pattern is required");
            }

            let search_path = args
                .path
                .as_deref()
                .map(|path| resolve_path(&self.cwd, path))
                .unwrap_or_else(|| self.cwd.clone());

            run_rg(
                &self.cwd,
                &args.pattern,
                &search_path,
                args.include.as_deref(),
                args.mode,
                args.case_sensitive,
                args.whole_word,
                args.context_before,
                args.context_after,
                args.output,
                args.max_results,
                args.top_n,
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
    mode: GrepMode,
    case_sensitive: bool,
    whole_word: bool,
    context_before: u64,
    context_after: u64,
    output_mode: GrepOutput,
    max_results: Option<usize>,
    top_n: Option<usize>,
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

    match output_mode {
        GrepOutput::Count | GrepOutput::Json => {
            cmd.arg("--count")
                .arg("--color=never")
                .arg("--with-filename");
        }
        GrepOutput::Files => {
            cmd.arg("--files-with-matches").arg("--color=never");
        }
        GrepOutput::Lines => {
            cmd.arg("--no-heading")
                .arg("--line-number")
                .arg("--color=never")
                .arg("--with-filename")
                .arg("--max-count=50");
        }
    }

    if matches!(mode, GrepMode::Literal) {
        cmd.arg("--fixed-strings");
    }

    if !case_sensitive {
        cmd.arg("--ignore-case");
    }

    if whole_word {
        cmd.arg("--word-regexp");
    }

    if !matches!(output_mode, GrepOutput::Count | GrepOutput::Files) {
        if context_before > 0 {
            cmd.arg(format!("-B{context_before}"));
        }

        if context_after > 0 {
            cmd.arg(format!("-A{context_after}"));
        }
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

    match output_mode {
        GrepOutput::Count | GrepOutput::Json => {
            let mut entries: Vec<(&str, u64)> = lines
                .iter()
                .filter_map(|line| {
                    let (path, count) = line.rsplit_once(':')?;
                    Some((path, count.trim().parse().ok()?))
                })
                .collect();
            entries.sort_by_key(|e| std::cmp::Reverse(e.1));

            let total_matches: u64 = entries.iter().map(|(_, c)| c).sum();
            let total_files = entries.len();
            let effective_limit = top_n.or(max_results);
            if let Some(n) = effective_limit {
                entries.truncate(n);
            }

            if matches!(output_mode, GrepOutput::Json) {
                let items: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|(path, count)| serde_json::json!({"file": path, "count": count}))
                    .collect();
                let json = serde_json::json!({
                    "total_matches": total_matches,
                    "total_files": total_files,
                    "files": items,
                });
                ToolResult::text(json.to_string())
            } else {
                let showing = if effective_limit.is_some() && entries.len() < total_files {
                    format!(" (showing top {})", entries.len())
                } else {
                    String::new()
                };
                let mut out =
                    format!("{total_matches} matches across {total_files} files{showing}\n");
                for (path, count) in &entries {
                    out.push_str(&format!("{path}: {count}\n"));
                }
                ToolResult::text(out)
            }
        }
        GrepOutput::Files => {
            let mut file_lines = lines;
            let total = file_lines.len();
            if let Some(n) = max_results {
                file_lines.truncate(n);
            }
            let showing = if file_lines.len() < total {
                format!(" (showing {}/{})", file_lines.len(), total)
            } else {
                String::new()
            };
            ToolResult::text(format!(
                "{total} files{showing}\n\n{}",
                file_lines.join("\n")
            ))
        }
        GrepOutput::Lines => ToolResult::text(truncate_lines(&lines, "matches")),
    }
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
        fs::write(
            dir.path().join("test.txt"),
            "hello world\nfoo bar\nhello again",
        )
        .unwrap();

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
        fs::write(
            dir.path().join("test.txt"),
            "Hello World\nhello world\nHELLO",
        )
        .unwrap();

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
    async fn grep_count_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "hello\nhello\nhello").unwrap();
        fs::write(dir.path().join("b.txt"), "hello\nworld").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "output": "count"
            }))
            .await;
        let text = extract_text(&result);
        // sorted descending by count, with summary header
        assert!(text.contains("4 matches across 2 files"));
        assert!(text.contains("a.txt: 3"));
        assert!(text.contains("b.txt: 1"));
    }

    #[tokio::test]
    async fn grep_files_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("match.txt"), "hello world").unwrap();
        fs::write(dir.path().join("no.txt"), "goodbye").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "output": "files"
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("match.txt"));
        assert!(!text.contains("no.txt"));
    }

    #[tokio::test]
    async fn grep_count_with_max_results() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "x\nx\nx").unwrap();
        fs::write(dir.path().join("b.txt"), "x\nx").unwrap();
        fs::write(dir.path().join("c.txt"), "x").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "x",
                "path": dir.path().to_str().unwrap(),
                "output": "count",
                "max_results": 2
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("6 matches across 3 files (showing top 2)"));
        // should have the top 2 by count, not all 3
        assert!(text.lines().filter(|l| l.contains(": ")).count() == 2);
    }

    #[tokio::test]
    async fn grep_count_with_top_n() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "x\nx\nx\nx\nx").unwrap();
        fs::write(dir.path().join("b.txt"), "x\nx\nx").unwrap();
        fs::write(dir.path().join("c.txt"), "x\nx").unwrap();
        fs::write(dir.path().join("d.txt"), "x").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "x",
                "path": dir.path().to_str().unwrap(),
                "output": "count",
                "top_n": 2
            }))
            .await;
        let text = extract_text(&result);
        // 5+3+2+1 = 11 matches, 4 files, showing top 2
        assert!(text.contains("across 4 files"), "got: {text}");
        assert!(text.contains("showing top 2"), "got: {text}");
        assert_eq!(text.lines().filter(|l| l.contains(": ")).count(), 2);
    }

    #[tokio::test]
    async fn grep_json_with_top_n() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "y\ny\ny").unwrap();
        fs::write(dir.path().join("b.txt"), "y\ny").unwrap();
        fs::write(dir.path().join("c.txt"), "y").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "y",
                "path": dir.path().to_str().unwrap(),
                "output": "json",
                "top_n": 1
            }))
            .await;
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["total_matches"], 6);
        assert_eq!(json["total_files"], 3);
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["count"], 3);
    }

    #[tokio::test]
    async fn grep_json_mode() {
        let dir = temp_dir();
        fs::write(dir.path().join("a.txt"), "hello\nhello").unwrap();
        fs::write(dir.path().join("b.txt"), "hello").unwrap();

        let tool = GrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(serde_json::json!({
                "pattern": "hello",
                "path": dir.path().to_str().unwrap(),
                "output": "json"
            }))
            .await;
        let text = extract_text(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["total_matches"], 3);
        assert_eq!(json["total_files"], 2);
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        // sorted desc by count
        assert!(files[0]["count"].as_u64().unwrap() >= files[1]["count"].as_u64().unwrap());
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
