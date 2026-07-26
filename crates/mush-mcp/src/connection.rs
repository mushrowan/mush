//! MCP server connection management

use std::io::IsTerminal;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, ServerResult, Tool as McpToolDef,
};
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::ConfigureCommandExt;
use tokio::process::Command;

use crate::config::{McpServerConfig, McpServerType, build_http_config};

/// error type for MCP operations
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

/// wrap a startup status line in an ansi code, or leave it alone
///
/// startup status goes to stderr before the tui takes over, so it has to
/// survive being piped to a file without spraying escape bytes.
fn status_line(text: &str, ansi: &str, colour: bool) -> String {
    if colour {
        format!("\x1b[{ansi}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// report startup status on stderr, styled only for a real terminal
pub(crate) fn report_status(text: &str, ansi: &str) {
    eprintln!(
        "{}",
        status_line(text, ansi, std::io::stderr().is_terminal())
    );
}

/// a live connection to an MCP server
pub struct McpConnection {
    pub name: String,
    pub service: RunningService<RoleClient, ()>,
    pub tools: Vec<McpToolDef>,
    /// per-request deadline, `None` to wait indefinitely
    timeout: Option<Duration>,
}

impl McpConnection {
    /// connect to an MCP server
    ///
    /// the configured timeout bounds the whole handshake, otherwise a server
    /// that spawns but never speaks would wedge startup forever.
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

                let args_owned: Vec<String> = args.to_vec();
                let env = config.environment.clone();

                let transport = rmcp::transport::TokioChildProcess::new(
                    Command::new(cmd).configure(move |c| {
                        for arg in &args_owned {
                            c.arg(arg);
                        }
                        for (k, v) in &env {
                            c.env(k, v);
                        }
                        // a timed-out handshake drops the transport, which must
                        // take the child with it rather than orphan it
                        c.kill_on_drop(true);
                    }),
                )
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

        // list available tools
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

    /// call a tool on this server
    ///
    /// uses the cancellable request path so an expired deadline also tells
    /// the server to stop working, rather than just abandoning the response.
    /// progress notifications extend the deadline, which matters for tools
    /// that stream updates while rendering.
    #[tracing::instrument(name = "mcp_call", skip_all, fields(tool = %name))]
    pub async fn call_tool(
        &self,
        name: String,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let mut params = CallToolRequestParams::new(name);
        if let Some(obj) = args.as_object().cloned() {
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
