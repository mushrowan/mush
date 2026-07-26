//! MCP server connection management

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, ServerResult, Tool as McpToolDef,
};
use rmcp::service::{PeerRequestOptions, RoleClient, RunningService};
use rmcp::transport::ConfigureCommandExt;
use tokio::process::Command;

use crate::config::{McpServerConfig, McpServerType, build_http_config};
use crate::tool::McpTool;

/// error type for MCP operations
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed: {0}")]
    Connection(String),
    #[error("tool call failed: {0}")]
    ToolCall(String),
    #[error("server disabled")]
    Disabled,
    #[error("timed out after {}s", .0.as_secs())]
    Timeout(Duration),
}

const DIM: &str = "2";
const YELLOW: &str = "33";

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
fn report_status(text: &str, ansi: &str) {
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

/// manages connections to multiple MCP servers
pub struct McpManager {
    connections: HashMap<String, Arc<McpConnection>>,
}

impl McpManager {
    /// connect to all configured MCP servers
    #[tracing::instrument(name = "mcp_connect_all", skip_all)]
    pub async fn connect_all(
        configs: &HashMap<String, McpServerConfig>,
    ) -> (Self, mush_agent::tool::ToolRegistry) {
        let mut connections = HashMap::new();
        let mut tools = mush_agent::tool::ToolRegistry::new();

        for (name, config) in configs {
            if !config.enabled {
                continue;
            }

            match McpConnection::connect(name, config).await {
                Ok(conn) => {
                    let conn = Arc::new(conn);
                    for mcp_tool in &conn.tools {
                        tools.register_shared(std::sync::Arc::new(McpTool::new(
                            name,
                            mcp_tool.clone(),
                            Arc::clone(&conn),
                        )));
                    }
                    connections.insert(name.clone(), conn);
                }
                Err(McpError::Disabled) => {}
                Err(e) => {
                    tracing::warn!(server = %name, error = %e, "MCP server connection failed");
                    report_status(&format!("mcp: {name} failed: {e}"), YELLOW);
                }
            }
        }

        (Self { connections }, tools)
    }

    /// get a connection by name
    pub fn get(&self, name: &str) -> Option<&Arc<McpConnection>> {
        self.connections.get(name)
    }

    /// list connected server names
    pub fn connected_servers(&self) -> Vec<&str> {
        self.connections.keys().map(|s| s.as_str()).collect()
    }

    /// get all connections as (name, connection) pairs for dynamic tool building
    pub fn connections(&self) -> Vec<(String, Arc<McpConnection>)> {
        self.connections
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// one local server keyed as `name`, running `command`
    fn local(name: &str, command: &[&str], timeout: u64) -> HashMap<String, McpServerConfig> {
        HashMap::from([(
            name.to_string(),
            McpServerConfig {
                server_type: McpServerType::Local {
                    command: command.iter().map(|s| (*s).to_string()).collect(),
                },
                enabled: true,
                timeout,
                environment: HashMap::new(),
            },
        )])
    }

    #[tokio::test]
    async fn connect_all_with_empty_config() {
        let (manager, tools) = McpManager::connect_all(&HashMap::new()).await;
        assert!(manager.connected_servers().is_empty());
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn disabled_server_skipped() {
        let mut configs = local("test", &["nonexistent"], 30);
        configs.get_mut("test").unwrap().enabled = false;

        let (manager, tools) = McpManager::connect_all(&configs).await;
        assert!(manager.connected_servers().is_empty());
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn invalid_command_reports_error() {
        let configs = local("bad", &["__nonexistent_binary_xyz__"], 5);
        let (manager, _tools) = McpManager::connect_all(&configs).await;
        assert!(manager.connected_servers().is_empty());
    }

    /// a server that spawns but never speaks must not wedge startup
    /// a server that spawns but never speaks must not wedge startup
    #[tokio::test]
    async fn silent_server_gives_up_after_the_configured_timeout() {
        let configs = local("hang", &["sleep", "60"], 1);

        let connected =
            tokio::time::timeout(Duration::from_secs(20), McpManager::connect_all(&configs)).await;

        let Ok((manager, tools)) = connected else {
            panic!("connect_all ignored the 1s timeout and hung");
        };
        assert!(manager.connected_servers().is_empty());
        assert!(tools.is_empty());
    }

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
