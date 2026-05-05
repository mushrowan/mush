//! write tool - creates or overwrites files, auto-creating parent dirs

use std::path::Path;
use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::util::resolve_path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    /// per-call escape hatch when the configured `forbidden_chars`
    /// would otherwise block the write. defaults to false.
    #[serde(default)]
    allow_forbidden_chars: bool,
}

pub struct WriteTool {
    cwd: Arc<Path>,
    forbidden_chars: crate::forbidden_chars::ForbiddenChars,
}

impl WriteTool {
    pub fn new(cwd: Arc<Path>) -> Self {
        Self {
            cwd,
            forbidden_chars: crate::forbidden_chars::ForbiddenChars::empty(),
        }
    }

    /// install a forbidden-chars validator. content containing any of
    /// the configured chars will be rejected unless the model passes
    /// `allow_forbidden_chars: true` on the call
    pub fn with_forbidden_chars(
        mut self,
        forbidden_chars: crate::forbidden_chars::ForbiddenChars,
    ) -> Self {
        self.forbidden_chars = forbidden_chars;
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
        let mut schema = serde_json::json!({
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
        });
        // only advertise the override knob when the validator is
        // actually configured. keeps schemas minimal for the common
        // case (no forbidden chars set)
        if !self.forbidden_chars.is_empty() {
            schema["properties"]["allow_forbidden_chars"] = serde_json::json!({
                "type": "boolean",
                "description": "bypass the configured forbidden_chars guard for this call. \
                                defaults to false. only set when the forbidden char is genuinely \
                                required (e.g. quoting a verbatim source) - prefer fixing the \
                                content first"
            });
        }
        schema
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<WriteArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        if !args.allow_forbidden_chars
            && let Err(err) = self.forbidden_chars.check(&args.content)
        {
            return ToolResult::error(err.to_string());
        }

        let path = resolve_path(&self.cwd, &args.path);
        let content = args.content;

        tokio::task::spawn_blocking(move || write_file(&path, &content))
            .await
            .unwrap_or_else(|e| ToolResult::error(format!("task join error: {e}")))
    }
}

fn write_file(path: &Path, content: &str) -> ToolResult {
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
    use crate::forbidden_chars::ForbiddenChars;
    use crate::util::extract_text;
    use std::fs;

    fn write_args(path: &Path, content: &str) -> serde_json::Value {
        serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": content,
        })
    }

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

    #[tokio::test]
    async fn write_rejects_forbidden_chars_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.txt");
        let tool =
            WriteTool::new(Arc::from(dir.path())).with_forbidden_chars(ForbiddenChars::new(['—']));

        let result = tool.execute(write_args(&path, "no — allowed")).await;
        assert!(
            !result.outcome.is_success(),
            "write should fail when content contains forbidden char"
        );
        let msg = extract_text(&result);
        assert!(
            msg.contains("'—'") && msg.contains("allow_forbidden_chars"),
            "error should name char and bypass flag: {msg}"
        );
        assert!(
            !path.exists(),
            "file should not be created when validation fails"
        );
    }

    #[tokio::test]
    async fn write_passes_when_no_forbidden_chars_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.txt");
        let tool =
            WriteTool::new(Arc::from(dir.path())).with_forbidden_chars(ForbiddenChars::new(['—']));

        let result = tool.execute(write_args(&path, "plain hyphen-minus")).await;
        assert!(result.outcome.is_success(), "write should succeed");
        assert_eq!(fs::read_to_string(&path).unwrap(), "plain hyphen-minus");
    }

    #[tokio::test]
    async fn write_allow_forbidden_chars_bypasses_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("override.txt");
        let tool =
            WriteTool::new(Arc::from(dir.path())).with_forbidden_chars(ForbiddenChars::new(['—']));

        let mut args = write_args(&path, "with — em dash");
        args["allow_forbidden_chars"] = serde_json::Value::Bool(true);
        let result = tool.execute(args).await;
        assert!(
            result.outcome.is_success(),
            "explicit allow_forbidden_chars should bypass validator: {:?}",
            extract_text(&result)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "with — em dash");
    }

    #[test]
    fn write_schema_omits_allow_forbidden_chars_when_validator_empty() {
        let tool = WriteTool::new(Arc::from(std::env::temp_dir().as_path()));
        let schema = tool.parameters_schema();
        assert!(
            schema["properties"]["allow_forbidden_chars"].is_null(),
            "schema should not advertise allow_forbidden_chars when no chars are configured"
        );
    }

    #[test]
    fn write_schema_advertises_allow_forbidden_chars_when_configured() {
        let tool = WriteTool::new(Arc::from(std::env::temp_dir().as_path()))
            .with_forbidden_chars(ForbiddenChars::new(['—']));
        let schema = tool.parameters_schema();
        assert_eq!(
            schema["properties"]["allow_forbidden_chars"]["type"], "boolean",
            "schema should advertise allow_forbidden_chars when validator is configured"
        );
    }
}
