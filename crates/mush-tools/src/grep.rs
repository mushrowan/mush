//! grep tool - pattern search using ripgrep (rg)

use std::path::PathBuf;
use std::process::Stdio;

use mush_agent::tool::{AgentTool, ToolResult};

const MAX_RESULTS: usize = 200;

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
        "Search file contents using ripgrep. Respects .gitignore. \
         Returns matching lines with file paths and line numbers."
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
                .map(|p| {
                    let path = std::path::Path::new(p);
                    if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        self.cwd.join(p)
                    }
                })
                .unwrap_or_else(|| self.cwd.clone());

            let include = args["include"].as_str();

            run_rg(&self.cwd, pattern, &search_path, include).await
        })
    }
}

async fn run_rg(
    cwd: &PathBuf,
    pattern: &str,
    search_path: &std::path::Path,
    include: Option<&str>,
) -> ToolResult {
    let mut cmd = tokio::process::Command::new("rg");
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        .arg("--max-count=50") // per-file limit
        .arg(pattern)
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

    // truncate to MAX_RESULTS lines
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() > MAX_RESULTS {
        let truncated: String = lines[..MAX_RESULTS].join("\n");
        ToolResult::text(format!(
            "{truncated}\n\n[{} more matches. Narrow your search pattern.]",
            lines.len() - MAX_RESULTS
        ))
    } else {
        ToolResult::text(format!("{} matches\n\n{stdout}", lines.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn grep_finds_pattern() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}",
        )
        .unwrap();

        let result = run_rg(&dir.path().to_path_buf(), "println", dir.path(), None).await;

        assert!(!result.is_error);
        let text = extract_text(&result);
        assert!(text.contains("println"));
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let result = run_rg(
            &dir.path().to_path_buf(),
            "nonexistent_pattern_xyz",
            dir.path(),
            None,
        )
        .await;

        let text = extract_text(&result);
        assert!(text.contains("no matches"));
    }

    #[tokio::test]
    async fn grep_with_include_glob() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("code.rs"), "fn hello()").unwrap();
        fs::write(dir.path().join("data.txt"), "fn hello()").unwrap();

        let result = run_rg(&dir.path().to_path_buf(), "hello", dir.path(), Some("*.rs")).await;

        let text = extract_text(&result);
        assert!(text.contains("code.rs"));
        assert!(!text.contains("data.txt"));
    }

    fn extract_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
