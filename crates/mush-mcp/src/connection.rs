use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, ServerResult, Tool as McpToolDef,
};
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};
use tokio::process::Command;

use crate::config::{McpServerConfig, McpServerType, build_http_config};

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed: {0}")]
    Connection(String),
    #[error("tool call failed: {0}")]
    ToolCall(String),
    #[error("server disabled")]
    Disabled,
    #[error("server {0} is not connected")]
    NotConnected(String),
    #[error("timed out after {}s", .0.as_secs())]
    Timeout(Duration),
}

pub(crate) const DIM: &str = "2";
pub(crate) const YELLOW: &str = "33";

fn status_line(text: &str, ansi: &str, colour: bool) -> String {
    if colour {
        format!("\x1b[{ansi}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub(crate) fn report_status(text: &str, ansi: &str) {
    eprintln!(
        "{}",
        status_line(
            text,
            ansi,
            anstream::AutoStream::auto(std::io::stderr()).current_choice()
                == anstream::ColorChoice::AlwaysAnsi,
        )
    );
}

pub struct McpConnection {
    pub name: String,
    pub service: RunningService<RoleClient, ()>,
    pub tools: Vec<McpToolDef>,
    timeout: Option<Duration>,
}

impl McpConnection {
    #[tracing::instrument(name = "mcp_connect", skip_all, fields(server = %name))]
    pub async fn connect(name: &str, config: &McpServerConfig) -> Result<Self, McpError> {
        if !config.enabled {
            return Err(McpError::Disabled);
        }

        match config.request_timeout() {
            Some(deadline) => tokio::time::timeout(deadline, Self::handshake(name, config))
                .await
                .map_err(|_| McpError::Timeout(deadline))?,
            None => Self::handshake(name, config).await,
        }
    }

    async fn handshake(name: &str, config: &McpServerConfig) -> Result<Self, McpError> {
        let service = match &config.server_type {
            McpServerType::Local { command } => {
                let (cmd, args) = command
                    .split_first()
                    .ok_or_else(|| McpError::Connection("empty command".into()))?;

                let mut command = Command::new(cmd);
                command
                    .args(args)
                    .envs(&config.environment)
                    .kill_on_drop(true);

                let transport = rmcp::transport::TokioChildProcess::new(command)
                    .map_err(|e| McpError::Connection(e.to_string()))?;

                ().serve(transport)
                    .await
                    .map_err(|e| McpError::Connection(e.to_string()))?
            }
            McpServerType::Remote { url, headers } => {
                let transport = rmcp::transport::StreamableHttpClientTransport::from_config(
                    build_http_config(url, headers),
                );

                ().serve(transport)
                    .await
                    .map_err(|e| McpError::Connection(e.to_string()))?
            }
        };

        let tools_result = service
            .list_tools(Default::default())
            .await
            .map_err(|e| McpError::Connection(format!("failed to list tools: {e}")))?;

        let tools = tools_result.tools;
        report_status(
            &format!("mcp: {name} connected ({} tools)", tools.len()),
            DIM,
        );

        Ok(Self {
            name: name.to_string(),
            service,
            tools,
            timeout: config.request_timeout(),
        })
    }

    #[tracing::instrument(name = "mcp_call", skip_all, fields(tool = %name))]
    pub async fn call_tool(
        &self,
        name: String,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let mut params = CallToolRequestParams::new(name);
        if let serde_json::Value::Object(obj) = args {
            params = params.with_arguments(obj);
        }

        let options = match self.timeout {
            Some(deadline) => {
                PeerRequestOptions::with_timeout(deadline).reset_timeout_on_progress()
            }
            None => PeerRequestOptions::no_options(),
        };

        let response = self
            .service
            .send_request_with_option(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                options,
            )
            .await
            .map_err(|e| McpError::ToolCall(e.to_string()))?
            .await_response()
            .await
            .map_err(|e| McpError::ToolCall(e.to_string()))?;

        match response {
            ServerResult::CallToolResult(result) => Ok(result),
            other => Err(McpError::ToolCall(format!(
                "expected a tool result, got {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_plain_when_stderr_is_redirected() {
        let line = status_line("mcp: git connected", DIM, false);
        assert_eq!(line, "mcp: git connected");
        assert!(
            !line.contains('\x1b'),
            "escapes leaked into a pipe: {line:?}"
        );
    }

    #[test]
    fn status_is_styled_for_a_terminal() {
        assert_eq!(
            status_line("mcp: git failed", YELLOW, true),
            "\x1b[33mmcp: git failed\x1b[0m"
        );
    }
}
