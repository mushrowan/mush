use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyUserArgs {
    title: String,
    body: String,
}

pub struct NotifyUserTool;

impl Default for NotifyUserTool {
    fn default() -> Self {
        Self
    }
}

impl NotifyUserTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl AgentTool for NotifyUserTool {
    fn name(&self) -> &str {
        "notify_user"
    }

    fn label(&self) -> &str {
        "NotifyUser"
    }

    fn description(&self) -> &str {
        "Send a desktop notification to the user. Use this to alert the user when a long-running task completes, when you need input, or when something important happens. The notification appears outside the terminal so the user sees it even if they've switched to another window."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "notification title (short, a few words)"
                },
                "body": {
                    "type": "string",
                    "description": "notification body (brief description)"
                }
            },
            "required": ["title", "body"]
        })
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<NotifyUserArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        execute_command(crate::notification::command(&args.title, &args.body)).await
    }
}

async fn execute_command(command: tokio::process::Command) -> ToolResult {
    match crate::notification::deliver(command, crate::notification::DELIVERY_TIMEOUT).await {
        Ok(()) => ToolResult::text("notification accepted by desktop service"),
        Err(error) => {
            tracing::warn!(%error, "desktop notification failed");
            ToolResult::error(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;

    #[tokio::test]
    async fn notify_reports_missing_helper() {
        let directory = tempfile::tempdir().unwrap();
        let result = execute_command(tokio::process::Command::new(
            directory.path().join("missing"),
        ))
        .await;
        let text = extract_text(&result);
        assert!(text.contains("could not launch notification helper"));
        assert_eq!(result.outcome, mush_ai::types::ToolOutcome::Error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_reports_helper_exit_failure() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        let result = execute_command(command).await;
        assert_eq!(result.outcome, mush_ai::types::ToolOutcome::Error);
        assert!(extract_text(&result).contains("exited with"));
    }

    #[test]
    fn schema_has_required_fields() {
        let tool = NotifyUserTool::new();
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "title"));
        assert!(required.iter().any(|v| v == "body"));
    }
}
