use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

use crate::catalogue::McpCatalogue;

type SharedCatalogue = Arc<McpCatalogue>;

pub struct McpListToolsTool {
    catalogue: SharedCatalogue,
}

impl McpListToolsTool {
    pub fn new(catalogue: SharedCatalogue) -> Self {
        Self { catalogue }
    }
}

#[async_trait::async_trait]
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
    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }
    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let filter = args.get("server").and_then(|v| v.as_str());
        let tools = self.catalogue.tools(filter).await;

        if tools.is_empty() {
            return ToolResult::text("no MCP tools available");
        }

        let listing: String = tools
            .iter()
            .map(|t| format!("- {} ({}): {}", t.full_name, t.server, t.description))
            .collect::<Vec<_>>()
            .join("\n");

        ToolResult::text(format!("{} MCP tools:\n{listing}", tools.len()))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetSchemasArgs {
    names: Vec<String>,
}

pub struct McpGetSchemasTool {
    catalogue: SharedCatalogue,
}

impl McpGetSchemasTool {
    pub fn new(catalogue: SharedCatalogue) -> Self {
        Self { catalogue }
    }
}

#[async_trait::async_trait]
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
    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }
    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<GetSchemasArgs>(args) {
            Ok(a) => a,
            Err(e) => return e,
        };

        let mut output = Vec::new();
        let mut not_found = Vec::new();

        for name in &args.names {
            match self.catalogue.schema(name).await {
                Some(schema) => output.push(format!(
                    "## {name}\n```json\n{}\n```",
                    serde_json::to_string_pretty(&schema).unwrap_or_default()
                )),
                None => not_found.push(name.as_str()),
            }
        }

        if !not_found.is_empty() {
            output.push(format!("not found: {}", not_found.join(", ")));
        }

        ToolResult::text(output.join("\n\n"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallToolArgs {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

pub struct McpCallToolTool {
    catalogue: SharedCatalogue,
}

impl McpCallToolTool {
    pub fn new(catalogue: SharedCatalogue) -> Self {
        Self { catalogue }
    }
}

#[async_trait::async_trait]
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
    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }
    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<CallToolArgs>(args) {
            Ok(a) => a,
            Err(e) => return e,
        };

        let Some((server, tool_name)) = self.catalogue.resolve(&args.name).await else {
            let available: Vec<_> = self
                .catalogue
                .tools(None)
                .await
                .into_iter()
                .map(|t| t.full_name)
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

        match server.call_tool(tool_name.clone(), args.arguments).await {
            Ok(result) => crate::result::convert_call_result(result),
            Err(e) => ToolResult::error(format!("MCP call {tool_name} on {}: {e}", server.name())),
        }
    }
}

#[must_use]
pub fn dynamic_mcp_tools(catalogue: McpCatalogue) -> Vec<Arc<dyn AgentTool>> {
    let catalogue: SharedCatalogue = Arc::new(catalogue);
    vec![
        Arc::new(McpListToolsTool::new(Arc::clone(&catalogue))),
        Arc::new(McpGetSchemasTool::new(Arc::clone(&catalogue))),
        Arc::new(McpCallToolTool::new(catalogue)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::McpServer;
    use rmcp::model::Tool as McpToolDef;

    fn empty() -> SharedCatalogue {
        Arc::new(McpCatalogue::new(Vec::new()))
    }

    fn stocked() -> SharedCatalogue {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "quality": { "type": "string" } }
        });
        Arc::new(McpCatalogue::new(vec![Arc::new(McpServer::with_tools(
            "sweethome3d",
            vec![McpToolDef::new(
                "render_photo",
                "ray-traced render",
                Arc::new(schema.as_object().unwrap().clone()),
            )],
        ))]))
    }

    #[tokio::test]
    async fn list_tools_empty() {
        let tool = McpListToolsTool::new(empty());
        let result = tool.execute(serde_json::json!({})).await;
        assert_eq!(extract_text(&result), "no MCP tools available");
    }

    #[tokio::test]
    async fn list_tools_names_each_tool_with_its_server() {
        let tool = McpListToolsTool::new(stocked());
        let text = extract_text(&tool.execute(serde_json::json!({})).await);

        assert!(text.contains("sweethome3d_render_photo"), "got {text}");
        assert!(text.contains("ray-traced render"), "got {text}");
    }

    #[tokio::test]
    async fn get_schemas_not_found() {
        let tool = McpGetSchemasTool::new(empty());
        let result = tool
            .execute(serde_json::json!({"names": ["nonexistent"]}))
            .await;
        assert!(extract_text(&result).contains("not found: nonexistent"));
    }

    #[tokio::test]
    async fn get_schemas_returns_the_input_schema() {
        let tool = McpGetSchemasTool::new(stocked());
        let text = extract_text(
            &tool
                .execute(serde_json::json!({"names": ["sweethome3d_render_photo"]}))
                .await,
        );

        assert!(text.contains("quality"), "got {text}");
    }

    #[tokio::test]
    async fn call_tool_not_found() {
        let tool = McpCallToolTool::new(empty());
        let result = tool.execute(serde_json::json!({"name": "missing"})).await;
        assert!(result.outcome.is_error());
        assert!(extract_text(&result).contains("not found"));
    }

    #[tokio::test]
    async fn call_tool_lists_what_is_available_when_the_name_is_wrong() {
        let tool = McpCallToolTool::new(stocked());
        let result = tool.execute(serde_json::json!({"name": "typo"})).await;

        assert!(result.outcome.is_error());
        assert!(
            extract_text(&result).contains("sweethome3d_render_photo"),
            "the error should point at real tool names"
        );
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
