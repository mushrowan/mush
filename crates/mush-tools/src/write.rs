use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::{resolve_path, run_blocking};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    #[serde(alias = "file_path")]
    path: String,
    content: String,
}

pub struct WriteTool {
    cwd: Arc<Path>,
    ledger: crate::read_ledger::ReadLedger,
}

impl WriteTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            cwd,
            ledger: crate::read_ledger::ReadLedger::shared(),
        }
    }

    #[must_use]
    pub fn with_read_ledger(mut self, ledger: crate::read_ledger::ReadLedger) -> Self {
        self.ledger = ledger;
        self
    }
}

#[async_trait::async_trait]
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

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<WriteArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let path = resolve_path(&self.cwd, &args.path);
        let content = args.content;
        let ledger = self.ledger.clone();

        crate::file_mutation_queue::with_file_mutation_queue(&path, async {
            let path = path.clone();
            run_blocking(move || {
                if let Err(stale) = ledger.check(&path) {
                    return ToolResult::error(stale.to_string());
                }
                let result = write_file(&path, &content);
                if result.outcome.is_success() {
                    ledger.record(&path);
                }
                result
            })
            .await
        })
        .await
    }
}

fn write_file(path: &Path, content: &str) -> ToolResult {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return ToolResult::error(format!("failed to create directories: {e}"));
    }

    match mush_ai::private_io::write_atomic(path, content) {
        Ok(()) => ToolResult::text(format!(
            "Successfully wrote {} bytes to {}\n\npreview:\n{}",
            content.len(),
            path.display(),
            content
        )),
        Err(e) => ToolResult::error(format!("failed to write file: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

    #[test]
    fn write_result_includes_content_preview() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preview.txt");

        let result = write_file(&path, "first line\nsecond line");
        let text = crate::util::extract_text(&result);

        assert!(text.contains("preview:"), "got: {text}");
        assert!(text.contains("first line"), "got: {text}");
        assert!(text.contains("second line"), "got: {text}");
    }

    #[test]
    fn a_write_accepts_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(dir.path().into());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.execute(serde_json::json!({
                "file_path": "aliased.txt",
                "content": "hi",
            })));

        assert!(result.outcome.is_success(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("aliased.txt")).unwrap(),
            "hi"
        );
    }
}
