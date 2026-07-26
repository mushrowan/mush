//! MCP server manager
//!
//! owns one [`McpServer`] handle per configured server and hands out the
//! tool registry the agent loop consumes.

use std::collections::HashMap;
use std::sync::Arc;

use crate::catalogue::McpCatalogue;
use crate::config::McpServerConfig;
use crate::connection::{DIM, YELLOW, report_status};
use crate::server::{McpServer, McpServerStatus, ServerState};
use crate::tool::McpTool;

/// manages connections to multiple MCP servers
pub struct McpManager {
    /// sorted by name so status output and tool ordering are stable
    servers: Vec<Arc<McpServer>>,
}

impl McpManager {
    /// connect to all configured MCP servers
    ///
    /// failures are recorded on the server handle rather than dropped, so
    /// `/mcp` can explain why something is missing and `/mcp reconnect`
    /// can retry it later.
    #[tracing::instrument(name = "mcp_connect_all", skip_all)]
    pub async fn connect_all(configs: &HashMap<String, McpServerConfig>) -> Self {
        let mut names: Vec<&String> = configs.keys().collect();
        names.sort();

        let mut servers = Vec::with_capacity(names.len());
        for name in names {
            let server = Arc::new(McpServer::connect(name, &configs[name]).await);
            match server.state().await {
                ServerState::Connected { tools } => {
                    report_status(&format!("mcp: {name} connected ({tools} tools)"), DIM);
                }
                ServerState::Failed { error } => {
                    report_status(&format!("mcp: {name} failed: {error}"), YELLOW);
                }
                ServerState::Disabled => {}
            }
            servers.push(server);
        }

        Self { servers }
    }

    /// one `AgentTool` per MCP tool, for the static (non-dynamic) mode
    pub async fn tool_registry(&self) -> mush_agent::tool::ToolRegistry {
        let mut registry = mush_agent::tool::ToolRegistry::new();
        for server in &self.servers {
            for definition in server.tools().await {
                registry.register_shared(Arc::new(McpTool::new(definition, Arc::clone(server))));
            }
        }
        registry
    }

    /// live view over every server's tools
    #[must_use]
    pub fn catalogue(&self) -> McpCatalogue {
        McpCatalogue::new(self.servers.clone())
    }

    /// display-ready status for every configured server
    pub async fn statuses(&self) -> Vec<McpServerStatus> {
        let mut out = Vec::with_capacity(self.servers.len());
        for server in &self.servers {
            out.push(server.status().await);
        }
        out
    }

    /// redial one server by name, case-insensitively
    pub async fn reconnect(&self, name: &str) -> Option<ServerState> {
        let query = name.to_lowercase();
        let server = self
            .servers
            .iter()
            .find(|s| s.name().to_lowercase() == query)?;
        Some(server.reconnect().await)
    }

    /// names of servers that are currently connected
    pub async fn connected_servers(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for server in &self.servers {
            if server.state().await.is_connected() {
                out.push(server.name());
            }
        }
        out
    }

    #[must_use]
    pub fn servers(&self) -> &[Arc<McpServer>] {
        &self.servers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerType;
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
        let manager = McpManager::connect_all(&HashMap::new()).await;
        assert!(manager.connected_servers().await.is_empty());
        assert!(manager.tool_registry().await.is_empty());
    }

    #[tokio::test]
    async fn disabled_server_is_listed_but_not_connected() {
        let mut configs = local("test", &["nonexistent"], 30);
        configs.get_mut("test").unwrap().enabled = false;

        let manager = McpManager::connect_all(&configs).await;

        assert!(manager.connected_servers().await.is_empty());
        assert!(manager.tool_registry().await.is_empty());
        // still visible to `/mcp`, so a disabled server is not a mystery
        let statuses = manager.statuses().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, ServerState::Disabled);
    }

    #[tokio::test]
    async fn failed_server_keeps_its_reason_for_display() {
        let manager = McpManager::connect_all(&local("bad", &["__nope__"], 5)).await;

        assert!(manager.connected_servers().await.is_empty());
        let statuses = manager.statuses().await;
        assert_eq!(statuses[0].name, "bad");
        assert!(matches!(statuses[0].state, ServerState::Failed { .. }));
    }

    #[tokio::test]
    async fn statuses_are_sorted_by_name() {
        let mut configs = local("zeta", &["__nope__"], 1);
        configs.extend(local("alpha", &["__nope__"], 1));

        let manager = McpManager::connect_all(&configs).await;

        let names: Vec<_> = manager
            .statuses()
            .await
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[tokio::test]
    async fn reconnect_names_an_unknown_server() {
        let manager = McpManager::connect_all(&HashMap::new()).await;
        assert!(manager.reconnect("ghost").await.is_none());
    }

    #[tokio::test]
    async fn reconnect_retries_a_failed_server() {
        let manager = McpManager::connect_all(&local("bad", &["__nope__"], 1)).await;

        let state = manager
            .reconnect("BAD")
            .await
            .expect("known server, matched case-insensitively");
        assert!(matches!(state, ServerState::Failed { .. }));
    }

    /// a server that spawns but never speaks must not wedge startup
    #[tokio::test]
    async fn silent_server_gives_up_after_the_configured_timeout() {
        let configs = local("hang", &["sleep", "60"], 1);

        let connected =
            tokio::time::timeout(Duration::from_secs(20), McpManager::connect_all(&configs)).await;

        let Ok(manager) = connected else {
            panic!("connect_all ignored the 1s timeout and hung");
        };
        assert!(manager.connected_servers().await.is_empty());
    }
}
