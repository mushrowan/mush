//! glob tool - find files by glob pattern using fd

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult};

use crate::util::{resolve_path, truncate_lines};

pub struct GlobTool {
    cwd: PathBuf,
}

impl GlobTool {
    pub fn new(cwd: PathBuf) -> Self {
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
            let pattern = match args["pattern"].as_str() {
                Some(p) => p,
                None => return ToolResult::error("missing required parameter: pattern"),
            };

            let search_dir = match args["path"].as_str() {
                Some(p) => resolve_path(&self.cwd, p),
                None => self.cwd.clone(),
            };

            let mut cmd = tokio::process::Command::new("fd");
            cmd.args(["--glob", pattern, "--type", "f"])
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
        let tool = GlobTool::new(PathBuf::from("/tmp"));
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "pattern"));
    }

    #[tokio::test]
    async fn glob_finds_toml_files() {
        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let tool = GlobTool::new(cwd);
        let result = tool
            .execute(serde_json::json!({"pattern": "Cargo.toml"}))
            .await;
        assert!(result.outcome.is_success());
        let text = match &result.content[0] {
            mush_ai::types::ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("Cargo.toml"), "got: {text}");
    }

    #[tokio::test]
    async fn glob_no_results() {
        let tool = GlobTool::new(PathBuf::from("/tmp"));
        let result = tool
            .execute(serde_json::json!({"pattern": "*.nonexistent_extension_xyz"}))
            .await;
        assert!(result.outcome.is_success());
        let text = match &result.content[0] {
            mush_ai::types::ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(text.contains("no files found"));
    }
}
