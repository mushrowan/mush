use std::sync::Arc;

use rmcp::model::Tool as McpToolDef;
use tokio::sync::RwLock;

use crate::config::{McpServerConfig, McpServerType};
use crate::connection::{McpConnection, McpError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Connected { tools: usize },
    Failed { error: String },
    Disabled,
}

impl ServerState {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connected { .. } => "connected",
            Self::Failed { .. } => "failed",
            Self::Disabled => "disabled",
        }
    }
}

pub struct McpServer {
    name: String,
    config: McpServerConfig,
    connection: RwLock<Option<Arc<McpConnection>>>,
    tools: RwLock<Vec<McpToolDef>>,
    state: RwLock<ServerState>,
}

impl McpServer {
    pub async fn connect(name: &str, config: &McpServerConfig) -> Self {
        let server = Self {
            name: name.to_string(),
            config: config.clone(),
            connection: RwLock::new(None),
            tools: RwLock::new(Vec::new()),
            state: RwLock::new(ServerState::Disabled),
        };
        if config.enabled {
            server.dial().await;
        }
        server
    }

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

    pub fn transport(&self) -> String {
        match &self.config.server_type {
            McpServerType::Local { command } => command.join(" "),
            McpServerType::Remote { url, .. } => url.clone(),
        }
    }

    pub async fn tools(&self) -> Vec<McpToolDef> {
        self.tools.read().await.clone()
    }

    pub(crate) async fn read_tools<R>(&self, f: impl FnOnce(&[McpToolDef]) -> R) -> R {
        f(&self.tools.read().await)
    }

    pub async fn status(&self) -> McpServerStatus {
        McpServerStatus {
            name: self.name.clone(),
            transport: self.transport(),
            state: self.state().await,
        }
    }

    pub async fn call_tool(
        &self,
        tool: String,
        args: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let connection = self.connection.read().await.clone();
        match connection {
            Some(connection) => connection.call_tool(tool, args).await,
            None => Err(McpError::NotConnected(self.name.clone())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: String,
    pub state: ServerState,
}

#[cfg(test)]
impl McpServer {
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
            connection: RwLock::new(None),
            tools: RwLock::new(tools),
            state: RwLock::new(ServerState::Connected { tools: count }),
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
