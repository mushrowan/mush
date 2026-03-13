//! bridge between MCP tools and the AgentTool trait

use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult};
use rmcp::model::Tool as McpToolDef;

use crate::connection::McpConnection;

/// wraps an MCP tool as an AgentTool for the agent loop
pub struct McpTool {
    /// server_name:tool_name
    full_name: String,
    /// just the tool name (for display)
    tool_name: String,
    /// server name
    server_name: String,
    /// tool definition from the MCP server
    definition: McpToolDef,
    /// connection to the MCP server
    connection: Arc<McpConnection>,
}

impl McpTool {
    pub fn new(server_name: &str, definition: McpToolDef, connection: Arc<McpConnection>) -> Self {
        let tool_name = definition.name.to_string();
        let full_name = format!("{server_name}_{tool_name}");
        Self {
            full_name,
            tool_name,
            server_name: server_name.to_string(),
            definition,
            connection,
        }
    }
}

impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn label(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        self.definition.description.as_deref().unwrap_or("MCP tool")
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // convert the MCP input schema to a json value
        serde_json::to_value(&self.definition.input_schema).unwrap_or_else(|_| {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            match self
                .connection
                .call_tool(self.tool_name.clone(), args)
                .await
            {
                Ok(result) => crate::result::convert_call_result(result),
                Err(e) => ToolResult::error(format!(
                    "MCP tool {} on {}: {e}",
                    self.tool_name, self.server_name
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_tool_naming() {
        let name = "server";
        let tool_name = "my_tool";
        let full = format!("{name}_{tool_name}");
        assert_eq!(full, "server_my_tool");
    }
}
