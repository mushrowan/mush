//! glob tool - find files by glob pattern using fd

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{clean_regex_pattern, inject_note, resolve_path, truncate_lines};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

pub struct GlobTool {
    cwd: Arc<Path>,
}

impl GlobTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self { cwd }
    }
}

#[async_trait::async_trait]
impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn label(&self) -> &str {
        "Glob"
    }
    fn description(&self) -> &str {
        "Fast file pattern matching using glob syntax. Respects .gitignore. \
         Returns matching file paths sorted by modification time (newest first). \
         The pattern is a raw glob; do not wrap it in quotes (e.g. pass `**/*.rs`, not `\"**/*.rs\"`). \
         Use this when you need to find files by name patterns like **/*.rs or src/**/*.ts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "glob pattern to match files (e.g. **/*.rs, src/**/*.ts, *.toml). raw glob, no quote-wrapping"
                },
                "path": {
                    "type": "string",
                    "description": "directory to search in (defaults to cwd)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        // ranked path matches; head keeps the most relevant entries
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<GlobArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let cleaned = clean_regex_pattern(&args.pattern);

        let search_dir = args
            .path
            .as_deref()
            .map(|path| resolve_path(&self.cwd, path))
            .unwrap_or_else(|| self.cwd.to_path_buf());

        let mut cmd = tokio::process::Command::new("fd");
        cmd.args(["--glob", &cleaned.pattern, "--type", "f"])
            .arg("--color=never")
            .current_dir(&search_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                return ToolResult::error(format!("failed to run fd: {e}"));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();

        let result = if lines.is_empty() {
            ToolResult::text("no files found")
        } else {
            ToolResult::text(truncate_lines(&lines, "files"))
        };
        inject_note(result, cleaned.note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_pattern() {
        let tool = GlobTool::new(Path::new("/tmp").into());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "pattern"));
    }

    #[tokio::test]
    async fn glob_finds_toml_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/config.toml"), "x = 1").unwrap();
        std::fs::write(dir.path().join("readme.md"), "hi").unwrap();

        let tool = GlobTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({ "pattern": "**/*.toml" }))
            .await;
        let text = crate::util::extract_text(&result);
        assert!(text.contains("Cargo.toml"));
        assert!(text.contains("nested/config.toml"));
        assert!(!text.contains("readme.md"));
    }

    #[tokio::test]
    async fn glob_no_results() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();

        let tool = GlobTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({ "pattern": "**/*.rs" }))
            .await;
        let text = crate::util::extract_text(&result);
        assert_eq!(text, "no files found");
    }

    #[tokio::test]
    async fn glob_strips_trailing_unmatched_quote_and_notes_it() {
        // mirror the find/grep behaviour: a pattern with a stray
        // trailing `"` should still find files (after cleaning) and
        // come back with a note explaining what was changed
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();

        let tool = GlobTool::new(dir.path().into());
        let result = tool
            .execute(serde_json::json!({ "pattern": "**/*.toml\"" }))
            .await;
        let text = crate::util::extract_text(&result);
        assert!(
            text.contains("Cargo.toml"),
            "must run cleaned pattern and find the file (got: {text})"
        );
        assert!(
            text.contains("trailing unmatched"),
            "must surface a note about the strip (got: {text})"
        );
    }
}
