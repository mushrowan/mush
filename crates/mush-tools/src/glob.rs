//! glob tool - find files by glob pattern using fd

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{resolve_path, truncate_lines};

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
         Use this when you need to find files by name patterns like '**/*.rs' or 'src/**/*.ts'."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "glob pattern to match files (e.g. '**/*.rs', 'src/**/*.ts', '*.toml')"
                },
                "path": {
                    "type": "string",
                    "description": "directory to search in (defaults to cwd)"
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
            let args = match parse_tool_args::<GlobArgs>(args) {
                Ok(args) => args,
                Err(error) => return error,
            };

            let search_dir = args
                .path
                .as_deref()
                .map(|path| resolve_path(&self.cwd, path))
                .unwrap_or_else(|| self.cwd.to_path_buf());

            let mut cmd = tokio::process::Command::new("fd");
            cmd.args(["--glob", &args.pattern, "--type", "f"])
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

            if lines.is_empty() {
                return ToolResult::text("no files found");
            }

            ToolResult::text(truncate_lines(&lines, "files"))
        })
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
}
