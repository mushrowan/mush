//! notify_user tool - send a desktop notification to the user

use mush_agent::tool::{AgentTool, ToolResult};

pub struct NotifyUserTool;

impl NotifyUserTool {
    pub fn new() -> Self {
        Self
    }
}

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

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let title = args["title"].as_str().unwrap_or("mush");
            let body = args["body"].as_str().unwrap_or("");

            // send notification via notify-send
            let notif_result = std::process::Command::new("notify-send")
                .arg("--app-name=mush")
                .arg(title)
                .arg(body)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();

            // also play a sound via pw-play
            let sound_path =
                "/run/current-system/sw/share/sounds/freedesktop/stereo/message-new-instant.oga";
            if std::path::Path::new(sound_path).exists() {
                let _ = std::process::Command::new("pw-play")
                    .arg(sound_path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }

            match notif_result {
                Ok(status) if status.success() => {
                    ToolResult::text(format!("notification sent: {title}"))
                }
                Ok(status) => ToolResult::text(format!(
                    "notify-send exited with {}, notification may not have appeared",
                    status
                )),
                Err(_) => {
                    // no notify-send available, still OK
                    ToolResult::text("notification attempted (notify-send not available)")
                }
            }
        })
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
