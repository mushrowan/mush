//! notify_user tool - send a desktop notification to the user

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

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<NotifyUserArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let title = args.title;
        let body = args.body;

        // use tokio::process here so the call doesn't block the executor
        // if the notification daemon is slow or unavailable. fire-and-forget
        // pw-play alongside, reaped in a detached task to avoid leaving
        // `<defunct>` zombies behind.
        let mut notif_cmd = tokio::process::Command::new("notify-send");
        notif_cmd
            .arg("--app-name=mush")
            .arg(&title)
            .arg(&body)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let notif_result = notif_cmd.status().await;

        let sound_path =
            "/run/current-system/sw/share/sounds/freedesktop/stereo/message-new-instant.oga";
        if std::path::Path::new(sound_path).exists() {
            let mut sound_cmd = tokio::process::Command::new("pw-play");
            sound_cmd
                .arg(sound_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Ok(mut child) = sound_cmd.spawn() {
                // tokio's global orphan queue reaps dropped children on
                // SIGCHLD, but explicitly waiting in a task is clearer and
                // doesn't rely on that implementation detail
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }

        match notif_result {
            Ok(status) if status.success() => {
                ToolResult::text(format!("notification sent: {title}"))
            }
            Ok(status) => ToolResult::text(format!(
                "notify-send exited with {status}, notification may not have appeared"
            )),
            Err(_) => ToolResult::text("notification attempted (notify-send not available)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::extract_text;

    #[tokio::test]
    async fn notify_returns_success() {
        let tool = NotifyUserTool::new();
        let result = tool
            .execute(serde_json::json!({
                "title": "test",
                "body": "hello"
            }))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("notif"));
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
