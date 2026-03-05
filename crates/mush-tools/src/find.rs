//! find tool - file search using fd

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult};

use crate::util::{resolve_path, truncate_lines};

pub struct FindTool {
    cwd: PathBuf,
}

impl FindTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

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

            let type_filter = args["type"].as_str();

            run_fd(&self.cwd, pattern, &search_path, type_filter).await
        })
    }
}

async fn run_fd(
    cwd: &std::path::Path,
    pattern: &str,
    search_path: &std::path::Path,
    type_filter: Option<&str>,
) -> ToolResult {
    let mut cmd = tokio::process::Command::new("fd");
    cmd.arg("--color=never")
        .arg(pattern)
        .arg(search_path)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(t) = type_filter {
        cmd.arg("--type").arg(t);
    }

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => return ToolResult::error(format!("failed to run fd: {e}")),
    };

    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return ToolResult::error(format!("fd error: {stderr}"));
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
    use crate::util::extract_text;
    use std::fs;

    #[tokio::test]
    async fn find_files_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "").unwrap();
        fs::write(dir.path().join("lib.rs"), "").unwrap();
        fs::write(dir.path().join("readme.md"), "").unwrap();

        let result = run_fd(
            dir.path(),
            r"\.rs$",
            dir.path(),
            Some("file"),
        )
        .await;

        let text = extract_text(&result);
        assert!(text.contains("main.rs"));
        assert!(text.contains("lib.rs"));
        assert!(!text.contains("readme.md"));
    }

    #[tokio::test]
    async fn find_no_results() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "").unwrap();

        let result = run_fd(
            dir.path(),
            "nonexistent_xyz",
            dir.path(),
            None,
        )
        .await;

        let text = extract_text(&result);
        assert!(text.contains("no files found"));
    }
}
