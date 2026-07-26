//! per-server handle and status
//!
//! tools hold a handle rather than a connection directly, so reconnecting
//! swaps the live connection underneath them without touching the agent's
//! tool registry.

use std::sync::Arc;

use rmcp::model::Tool as McpToolDef;
use tokio::sync::RwLock;

use crate::config::{McpServerConfig, McpServerType};
use crate::connection::{McpConnection, McpError};

/// where a server stands right now
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Connected {
        tools: usize,
    },
    Failed {
        error: String,
    },
    /// `enabled = false` in config, never dialled
    Disabled,
}

impl ServerState {
    #[must_use]
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// short word for display
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connected { .. } => "connected",
            Self::Failed { .. } => "failed",
            Self::Disabled => "disabled",
        }
    }
}

/// a named MCP server, connected or not
pub struct McpServer {
    name: String,
    config: McpServerConfig,
    /// current connection, `None` while disconnected or failed
    connection: Arc<RwLock<Option<Arc<McpConnection>>>>,
    /// tools advertised by the current connection, refreshed on reconnect
    tools: Arc<RwLock<Vec<McpToolDef>>>,
    state: Arc<RwLock<ServerState>>,
}

impl McpServer {
    /// dial a configured server, recording failure rather than propagating
    /// it so one broken server cannot stop the others from loading
    pub async fn connect(name: &str, config: &McpServerConfig) -> Self {
        let server = Self {
            name: name.to_string(),
            config: config.clone(),
            connection: Arc::new(RwLock::new(None)),
            tools: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(ServerState::Disabled)),
        };
        if config.enabled {
            server.dial().await;
        }
        server
    }

    /// replace the live connection, keeping the same handle so tools
    /// already registered with the agent keep working
    pub async fn reconnect(&self) -> ServerState {
        if !self.config.enabled {
            return ServerState::Disabled;
        }
        self.dial().await;
        self.state().await
    }

    async fn dial(&self) {
        let state = match McpConnection::connect(&self.name, &self.config).await {
            Ok(connection) => {
                let tools = connection.tools.clone();
                let count = tools.len();
                *self.tools.write().await = tools;
                *self.connection.write().await = Some(Arc::new(connection));
                ServerState::Connected { tools: count }
            }
            Err(error) => {
                *self.connection.write().await = None;
                self.tools.write().await.clear();
                tracing::warn!(server = %self.name, %error, "MCP server connection failed");
                ServerState::Failed {
                    error: error.to_string(),
                }
            }
        };
        *self.state.write().await = state;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn state(&self) -> ServerState {
        self.state.read().await.clone()
    }

    /// how the server is reached, for display
    pub fn transport(&self) -> String {
        match &self.config.server_type {
            McpServerType::Local { command } => command.join(" "),
            McpServerType::Remote { url, .. } => url.clone(),
        }
    }

    /// tool definitions advertised by the current connection
    pub async fn tools(&self) -> Vec<McpToolDef> {
        self.tools.read().await.clone()
    }

    /// display-ready snapshot
    pub async fn status(&self) -> McpServerStatus {
        McpServerStatus {
            name: self.name.clone(),
            transport: self.transport(),
            state: self.state().await,
        }
    }

    /// call a tool on whichever connection is live now
    pub async fn call_tool(
        &self,
        tool: String,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        // clone the Arc out so the lock is not held across the call
        let connection = self.connection.read().await.clone();
        match connection {
            Some(connection) => connection.call_tool(tool, args).await,
            None => Err(McpError::NotConnected(self.name.clone())),
        }
    }
}

/// display-ready snapshot of one server
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: String,
    pub state: ServerState,
}

#[cfg(test)]
impl McpServer {
    /// a server that reports a fixed tool list without dialling anything.
    /// tool calls on it always fail, so this suits catalogue tests only
    pub(crate) fn with_tools(name: &str, tools: Vec<McpToolDef>) -> Self {
        let count = tools.len();
        Self {
            name: name.to_string(),
            config: McpServerConfig {
                server_type: McpServerType::Local {
                    command: vec!["fake".into()],
                },
                enabled: true,
                timeout: 1,
                environment: std::collections::HashMap::new(),
            },
            connection: Arc::new(RwLock::new(None)),
            tools: Arc::new(RwLock::new(tools)),
            state: Arc::new(RwLock::new(ServerState::Connected { tools: count })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(command: &[&str], enabled: bool) -> McpServerConfig {
        McpServerConfig {
            server_type: McpServerType::Local {
                command: command.iter().map(|s| (*s).to_string()).collect(),
            },
            enabled,
            timeout: 2,
            environment: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn failed_server_records_why() {
        let server = McpServer::connect("bad", &config(&["__no_such_binary__"], true)).await;

        let ServerState::Failed { error } = server.state().await else {
            panic!("expected a failure state, got {:?}", server.state().await);
        };
        assert!(!error.is_empty(), "failure should carry a reason");
        assert!(server.tools().await.is_empty());
    }

    #[tokio::test]
    async fn disabled_server_is_never_dialled() {
        let server = McpServer::connect("off", &config(&["__no_such_binary__"], false)).await;
        assert_eq!(server.state().await, ServerState::Disabled);
    }

    #[tokio::test]
    async fn calling_a_disconnected_server_names_it() {
        let server = McpServer::connect("off", &config(&["__no_such_binary__"], false)).await;

        let error = server
            .call_tool("anything".into(), serde_json::json!({}))
            .await
            .expect_err("a disconnected server cannot serve calls");
        assert!(error.to_string().contains("off"), "got {error}");
    }

    #[tokio::test]
    async fn transport_is_shown_as_the_command() {
        let server = McpServer::connect("git", &config(&["uvx", "mcp-server-git"], false)).await;
        assert_eq!(server.transport(), "uvx mcp-server-git");
    }

    #[tokio::test]
    async fn reconnect_on_a_disabled_server_stays_disabled() {
        let server = McpServer::connect("off", &config(&["sleep", "60"], false)).await;
        assert_eq!(server.reconnect().await, ServerState::Disabled);
    }
}
