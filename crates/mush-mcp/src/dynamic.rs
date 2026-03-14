//! dynamic MCP tool loading
//!
//! meta-tools that let the agent discover and call MCP tools on demand
//! instead of loading all schemas into context at startup.
//!
//! - `mcp_list_tools`: list tool names and descriptions
//! - `mcp_get_schemas`: get full json schemas for selected tools
//! - `mcp_call_tool`: call an MCP tool by name

use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use rmcp::model::Tool as McpToolDef;
use serde::Deserialize;

use crate::connection::McpConnection;

/// lightweight tool info for listing (no schema)
#[derive(Debug, Clone)]
struct ToolInfo {
    server: String,
    full_name: String,
    tool_name: String,
    description: String,
}

/// shared state for all dynamic meta-tools
pub struct McpToolIndex {
    tools: Vec<ToolInfo>,
    definitions: Vec<(String, McpToolDef)>,
    connections: Vec<(String, Arc<McpConnection>)>,
}

impl McpToolIndex {
    /// build from connected MCP servers
    pub fn new(connections: &[(String, Arc<McpConnection>)]) -> Self {
        let mut tools = Vec::new();
        let mut definitions = Vec::new();
        let mut conns = Vec::new();

        for (server_name, conn) in connections {
            conns.push((server_name.clone(), Arc::clone(conn)));
            for tool_def in &conn.tools {
                let tool_name = tool_def.name.to_string();
                let full_name = format!("{server_name}_{tool_name}");
                let description = tool_def
                    .description
                    .as_deref()
                    .unwrap_or("MCP tool")
                    .to_string();
                tools.push(ToolInfo {
                    server: server_name.clone(),
                    full_name: full_name.clone(),
                    tool_name,
                    description,
                });
                definitions.push((full_name, tool_def.clone()));
            }
        }

        Self {
            tools,
            definitions,
            connections: conns,
        }
    }

    /// export tool names and descriptions for semantic indexing
    pub fn tool_descriptions(&self) -> Vec<(String, String)> {
        self.tools
            .iter()
            .map(|t| (t.full_name.clone(), t.description.clone()))
            .collect()
    }

    fn find_definition(&self, name: &str) -> Option<&McpToolDef> {
        let query = name.to_lowercase();
        self.definitions
            .iter()
            .find(|(n, _)| n.to_lowercase() == query)
            .map(|(_, d)| d)
    }

    fn find_connection(&self, tool_name: &str) -> Option<(&str, &str, &Arc<McpConnection>)> {
        let query = tool_name.to_lowercase();
        self.tools
            .iter()
            .find(|t| t.full_name.to_lowercase() == query)
            .and_then(|t| {
                self.connections
                    .iter()
                    .find(|(s, _)| *s == t.server)
                    .map(|(_, conn)| (t.server.as_str(), t.tool_name.as_str(), conn))
            })
    }
}

type SharedIndex = Arc<McpToolIndex>;

// -- mcp_list_tools --

pub struct McpListToolsTool {
    index: SharedIndex,
}

impl McpListToolsTool {
    pub fn new(index: SharedIndex) -> Self {
        Self { index }
    }
}

impl AgentTool for McpListToolsTool {
    fn name(&self) -> &str {
        "mcp_list_tools"
    }
    fn label(&self) -> &str {
        "MCP Tools"
    }
    fn description(&self) -> &str {
        "List available MCP tools with names and descriptions. \
         Use mcp_get_schemas to see full parameter schemas, \
         then mcp_call_tool to invoke them."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "optional: filter by server name"
                }
            }
        })
    }
    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let filter = args
                .get("server")
                .and_then(|v| v.as_str())
                .map(|s| s.to_lowercase());

            let tools: Vec<_> = self
                .index
                .tools
                .iter()
                .filter(|t| {
                    filter
                        .as_ref()
                        .is_none_or(|f| t.server.to_lowercase() == *f)
                })
                .collect();

            if tools.is_empty() {
                return ToolResult::text("no MCP tools available");
            }

            let listing: String = tools
                .iter()
                .map(|t| format!("- {} ({}): {}", t.full_name, t.server, t.description))
                .collect::<Vec<_>>()
                .join("\n");

            ToolResult::text(format!("{} MCP tools:\n{listing}", tools.len()))
        })
    }
}

// -- mcp_get_schemas --

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetSchemasArgs {
    names: Vec<String>,
}

pub struct McpGetSchemasTool {
    index: SharedIndex,
}

impl McpGetSchemasTool {
    pub fn new(index: SharedIndex) -> Self {
        Self { index }
    }
}

impl AgentTool for McpGetSchemasTool {
    fn name(&self) -> &str {
        "mcp_get_schemas"
    }
    fn label(&self) -> &str {
        "MCP Schemas"
    }
    fn description(&self) -> &str {
        "Get full parameter schemas for specific MCP tools by name. \
         Use mcp_list_tools first to see available tools."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "tool names to get schemas for (from mcp_list_tools)"
                }
            },
            "required": ["names"]
        })
    }
    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<GetSchemasArgs>(args) {
                Ok(a) => a,
                Err(e) => return e,
            };

            let mut output = Vec::new();
            let mut not_found = Vec::new();

            for name in &args.names {
                match self.index.find_definition(name) {
                    Some(def) => {
                        let schema = serde_json::to_value(&def.input_schema)
                            .unwrap_or_else(|_| serde_json::json!({}));
                        let desc = def.description.as_deref().unwrap_or("");
                        output.push(format!(
                            "## {}\n{desc}\n```json\n{}\n```",
                            name,
                            serde_json::to_string_pretty(&schema).unwrap_or_default()
                        ));
                    }
                    None => not_found.push(name.as_str()),
                }
            }

            if !not_found.is_empty() {
                output.push(format!("not found: {}", not_found.join(", ")));
            }

            ToolResult::text(output.join("\n\n"))
        })
    }
}

// -- mcp_call_tool --

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallToolArgs {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

pub struct McpCallToolTool {
    index: SharedIndex,
}

impl McpCallToolTool {
    pub fn new(index: SharedIndex) -> Self {
        Self { index }
    }
}

impl AgentTool for McpCallToolTool {
    fn name(&self) -> &str {
        "mcp_call_tool"
    }
    fn label(&self) -> &str {
        "MCP Call"
    }
    fn description(&self) -> &str {
        "Call an MCP tool by name with arguments. \
         Use mcp_list_tools to discover tools and mcp_get_schemas to see their parameters."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "full tool name (from mcp_list_tools)"
                },
                "arguments": {
                    "type": "object",
                    "description": "tool arguments (see mcp_get_schemas for schema)"
                }
            },
            "required": ["name"]
        })
    }
    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let args = match parse_tool_args::<CallToolArgs>(args) {
                Ok(a) => a,
                Err(e) => return e,
            };

            let Some((server, tool_name, conn)) = self.index.find_connection(&args.name) else {
                let available: Vec<_> = self
                    .index
                    .tools
                    .iter()
                    .map(|t| t.full_name.as_str())
                    .collect();
                return ToolResult::error(format!(
                    "tool '{}' not found. available: {}",
                    args.name,
                    if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available.join(", ")
                    }
                ));
            };

            match conn.call_tool(tool_name.to_string(), args.arguments).await {
                Ok(result) => crate::result::convert_call_result(result),
                Err(e) => ToolResult::error(format!("MCP call {tool_name} on {server}: {e}")),
            }
        })
    }
}

/// create the three dynamic MCP meta-tools
pub fn dynamic_mcp_tools(connections: &[(String, Arc<McpConnection>)]) -> Vec<Arc<dyn AgentTool>> {
    let index: SharedIndex = Arc::new(McpToolIndex::new(connections));
    vec![
        Arc::new(McpListToolsTool::new(Arc::clone(&index))),
        Arc::new(McpGetSchemasTool::new(Arc::clone(&index))),
        Arc::new(McpCallToolTool::new(index)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_index() -> SharedIndex {
        Arc::new(McpToolIndex::new(&[]))
    }

    #[tokio::test]
    async fn list_tools_empty() {
        let tool = McpListToolsTool::new(empty_index());
        let result = tool.execute(serde_json::json!({})).await;
        let text = extract_text(&result);
        assert_eq!(text, "no MCP tools available");
    }

    #[tokio::test]
    async fn get_schemas_not_found() {
        let tool = McpGetSchemasTool::new(empty_index());
        let result = tool
            .execute(serde_json::json!({"names": ["nonexistent"]}))
            .await;
        let text = extract_text(&result);
        assert!(text.contains("not found: nonexistent"));
    }

    #[tokio::test]
    async fn call_tool_not_found() {
        let tool = McpCallToolTool::new(empty_index());
        let result = tool.execute(serde_json::json!({"name": "missing"})).await;
        assert!(result.outcome.is_error());
        assert!(extract_text(&result).contains("not found"));
    }

    fn extract_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|p| match p {
                mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
