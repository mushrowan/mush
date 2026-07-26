//! live view over the tools advertised by every connected server
//!
//! shared by the dynamic meta-tools and the `/mcp` command so name
//! resolution behaves identically for the model and for the user. reads
//! through to the servers on every call, so a reconnect that changes a
//! server's tool list is picked up without rebuilding anything.

use std::sync::Arc;

use crate::server::McpServer;

/// one tool, as addressed by the agent
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolInfo {
    pub server: String,
    /// `{server}_{tool}`, the name the agent calls
    pub full_name: String,
    /// bare name as the server knows it
    pub tool_name: String,
    pub description: String,
}

pub struct McpCatalogue {
    servers: Vec<Arc<McpServer>>,
}

impl McpCatalogue {
    #[must_use]
    pub fn new(servers: Vec<Arc<McpServer>>) -> Self {
        Self { servers }
    }

    /// every tool, optionally narrowed to one server. case-insensitive on
    /// the server name so `/mcp tools SweetHome3D` works
    pub async fn tools(&self, server: Option<&str>) -> Vec<McpToolInfo> {
        let filter = server.map(str::to_lowercase);
        let mut out = Vec::new();

        for server in &self.servers {
            if filter
                .as_ref()
                .is_some_and(|f| server.name().to_lowercase() != *f)
            {
                continue;
            }
            for tool in server.tools().await {
                let tool_name = tool.name.to_string();
                out.push(McpToolInfo {
                    server: server.name().to_string(),
                    full_name: format!("{}_{tool_name}", server.name()),
                    tool_name,
                    description: tool
                        .description
                        .as_deref()
                        .unwrap_or("MCP tool")
                        .to_string(),
                });
            }
        }

        out
    }

    /// the server owning `full_name`, plus the bare tool name to call
    pub async fn resolve(&self, full_name: &str) -> Option<(Arc<McpServer>, String)> {
        let query = full_name.to_lowercase();
        for server in &self.servers {
            for tool in server.tools().await {
                let tool_name = tool.name.to_string();
                if format!("{}_{tool_name}", server.name()).to_lowercase() == query {
                    return Some((Arc::clone(server), tool_name));
                }
            }
        }
        None
    }

    /// json schema for `full_name`
    pub async fn schema(&self, full_name: &str) -> Option<serde_json::Value> {
        let query = full_name.to_lowercase();
        for server in &self.servers {
            for tool in server.tools().await {
                if format!("{}_{}", server.name(), tool.name).to_lowercase() == query {
                    return Some(
                        serde_json::to_value(&tool.input_schema)
                            .unwrap_or_else(|_| serde_json::json!({})),
                    );
                }
            }
        }
        None
    }

    /// server names in configuration order
    #[must_use]
    pub fn server_names(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.name()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Tool as McpToolDef;

    fn tool(name: &'static str, description: &'static str) -> McpToolDef {
        McpToolDef::new(
            name,
            description,
            Arc::new(
                serde_json::json!({ "type": "object", "properties": { "room": { "type": "string" } } })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
    }

    fn catalogue() -> McpCatalogue {
        McpCatalogue::new(vec![
            Arc::new(McpServer::with_tools(
                "sweethome3d",
                vec![
                    tool("render_photo", "ray-traced render"),
                    tool("add_wall", "create a wall"),
                ],
            )),
            Arc::new(McpServer::with_tools("git", vec![tool("commit", "commit")])),
        ])
    }

    #[tokio::test]
    async fn tools_are_prefixed_with_their_server() {
        let names: Vec<_> = catalogue()
            .tools(None)
            .await
            .into_iter()
            .map(|t| t.full_name)
            .collect();

        assert_eq!(
            names,
            [
                "sweethome3d_render_photo",
                "sweethome3d_add_wall",
                "git_commit"
            ]
        );
    }

    #[tokio::test]
    async fn tools_can_be_narrowed_to_one_server_ignoring_case() {
        let tools = catalogue().tools(Some("SweetHome3D")).await;
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().all(|t| t.server == "sweethome3d"));
    }

    #[tokio::test]
    async fn unknown_server_filter_yields_nothing() {
        assert!(catalogue().tools(Some("nope")).await.is_empty());
    }

    #[tokio::test]
    async fn resolve_splits_the_server_from_the_bare_tool_name() {
        let (server, tool_name) = catalogue()
            .resolve("sweethome3d_render_photo")
            .await
            .expect("known tool resolves");

        assert_eq!(server.name(), "sweethome3d");
        assert_eq!(tool_name, "render_photo");
    }

    #[tokio::test]
    async fn resolve_rejects_an_unknown_tool() {
        assert!(catalogue().resolve("sweethome3d_nope").await.is_none());
    }

    #[tokio::test]
    async fn schema_comes_back_as_the_tools_input_schema() {
        let schema = catalogue()
            .schema("sweethome3d_render_photo")
            .await
            .expect("known tool has a schema");

        assert_eq!(schema["properties"]["room"]["type"], "string");
    }

    #[tokio::test]
    async fn schema_of_an_unknown_tool_is_absent() {
        assert!(catalogue().schema("git_nope").await.is_none());
    }

    /// a tool whose name already contains underscores must still resolve,
    /// since the split point comes from the server list, not from parsing
    #[tokio::test]
    async fn underscored_tool_names_round_trip() {
        let catalogue = McpCatalogue::new(vec![Arc::new(McpServer::with_tools(
            "sweet_home",
            vec![tool("export_plan_image", "2d plan")],
        ))]);

        let (server, tool_name) = catalogue
            .resolve("sweet_home_export_plan_image")
            .await
            .expect("resolves despite underscores");
        assert_eq!(server.name(), "sweet_home");
        assert_eq!(tool_name, "export_plan_image");
    }
}
