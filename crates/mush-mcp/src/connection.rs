//! MCP server connection management

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, Tool as McpToolDef};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::ConfigureCommandExt;
use tokio::process::Command;

use crate::config::{McpServerConfig, McpServerType};
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
    #[error("remote transport not yet supported")]
    RemoteNotSupported,
}

/// a live connection to an MCP server
pub struct McpConnection {
    pub name: String,
    pub service: RunningService<RoleClient, ()>,
    pub tools: Vec<McpToolDef>,
}

impl McpConnection {
    /// connect to an MCP server
    pub async fn connect(name: &str, config: &McpServerConfig) -> Result<Self, McpError> {
        if !config.enabled {
            return Err(McpError::Disabled);
        }

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
                    }),
                )
                .map_err(|e| McpError::Connection(e.to_string()))?;

                ().serve(transport)
                    .await
                    .map_err(|e| McpError::Connection(e.to_string()))?
            }
            McpServerType::Remote { url, .. } => {
                let transport =
                    rmcp::transport::StreamableHttpClientTransport::from_uri(url.as_str());

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
        eprintln!(
            "\x1b[2mmcp: {name} connected ({} tools)\x1b[0m",
            tools.len()
        );

        Ok(Self {
            name: name.to_string(),
            service,
            tools,
        })
    }

    /// call a tool on this server
    pub async fn call_tool(
        &self,
        name: String,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let mut params = CallToolRequestParams::new(name);
        if let Some(obj) = args.as_object().cloned() {
            params = params.with_arguments(obj);
        }
        self.service
            .call_tool(params)
            .await
            .map_err(|e| McpError::ToolCall(e.to_string()))
    }
}

/// manages connections to multiple MCP servers
pub struct McpManager {
    connections: HashMap<String, Arc<McpConnection>>,
}

impl McpManager {
    /// connect to all configured MCP servers
    pub async fn connect_all(
        configs: &HashMap<String, McpServerConfig>,
    ) -> (Self, Vec<Box<dyn mush_agent::tool::AgentTool>>) {
        let mut connections = HashMap::new();
        let mut tools: Vec<Box<dyn mush_agent::tool::AgentTool>> = Vec::new();

        for (name, config) in configs {
            if !config.enabled {
                continue;
            }

            match McpConnection::connect(name, config).await {
                Ok(conn) => {
                    let conn = Arc::new(conn);
                    for mcp_tool in &conn.tools {
                        tools.push(Box::new(McpTool::new(
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
                    eprintln!("\x1b[33mmcp: {name} failed: {e}\x1b[0m");
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_all_with_empty_config() {
        let configs = HashMap::new();
        let (manager, tools) = McpManager::connect_all(&configs).await;
        assert!(manager.connected_servers().is_empty());
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn disabled_server_skipped() {
        let mut configs = HashMap::new();
        configs.insert(
            "test".into(),
            McpServerConfig {
                server_type: McpServerType::Local {
                    command: vec!["nonexistent".into()],
                },
                enabled: false,
                timeout: 30,
                environment: HashMap::new(),
            },
        );
        let (manager, tools) = McpManager::connect_all(&configs).await;
        assert!(manager.connected_servers().is_empty());
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn invalid_command_reports_error() {
        let mut configs = HashMap::new();
        configs.insert(
            "bad".into(),
            McpServerConfig {
                server_type: McpServerType::Local {
                    command: vec!["__nonexistent_binary_xyz__".into()],
                },
                enabled: true,
                timeout: 5,
                environment: HashMap::new(),
            },
        );
        let (manager, _tools) = McpManager::connect_all(&configs).await;
        // should have failed gracefully
        assert!(manager.connected_servers().is_empty());
    }
}
