//! write tool - creates or overwrites files, auto-creating parent dirs

use std::path::{Path, PathBuf};

use mush_agent::tool::{AgentTool, ToolResult};

pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn label(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. \
         Automatically creates parent directories."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let Some(path_str) = args["path"].as_str() else {
                return ToolResult::error("missing required parameter: path");
            };
            let Some(content) = args["content"].as_str() else {
                return ToolResult::error("missing required parameter: content");
            };

            let path = if Path::new(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                self.cwd.join(path_str)
            };
            let content = content.to_string();

            tokio::task::spawn_blocking(move || write_file(&path, &content))
                .await
                .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
        })
    }
}

fn write_file(path: &Path, content: &str) -> ToolResult {
    // create parent dirs
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return ToolResult::error(format!("failed to create directories: {e}"));
    }

    match std::fs::write(path, content) {
        Ok(()) => ToolResult::text(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path.display()
        )),
        Err(e) => ToolResult::error(format!("failed to write file: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn write_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");

        let result = write_file(&path, "hello world");
        assert!(result.outcome.is_success());
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/deep.txt");

        let result = write_file(&path, "nested");
        assert!(result.outcome.is_success());
        assert_eq!(fs::read_to_string(&path).unwrap(), "nested");
    }

    #[test]
    fn write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        fs::write(&path, "old content").unwrap();

        let result = write_file(&path, "new content");
        assert!(result.outcome.is_success());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new content");
    }
}
