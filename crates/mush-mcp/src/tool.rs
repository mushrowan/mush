//! bridge between MCP tools and the AgentTool trait

use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult};
use rmcp::model::Tool as McpToolDef;

use crate::server::McpServer;

/// wraps an MCP tool as an AgentTool for the agent loop
pub struct McpTool {
    /// server_name:tool_name
    full_name: String,
    /// just the tool name (for display)
    tool_name: String,
    /// tool definition from the MCP server
    definition: McpToolDef,
    /// handle to the server, which survives reconnects
    server: Arc<McpServer>,
}

impl McpTool {
    pub fn new(definition: McpToolDef, server: Arc<McpServer>) -> Self {
        let tool_name = definition.name.to_string();
        let full_name = format!("{}_{tool_name}", server.name());
        Self {
            full_name,
            tool_name,
            definition,
            server,
        }
    }
}

#[async_trait::async_trait]
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

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        // unknown shape; head keeps any structured prefix (titles,
        // status, the first list entries) which is usually where the
        // signal lives
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        match self.server.call_tool(self.tool_name.clone(), args).await {
            Ok(result) => crate::result::convert_call_result(result),
            Err(e) => ToolResult::error(format!(
                "MCP tool {} on {}: {e}",
                self.tool_name,
                self.server.name()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(name: &'static str) -> McpToolDef {
        McpToolDef::new(
            name,
            "does a thing",
            Arc::new(
                serde_json::json!({ "type": "object" })
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
    }

    #[tokio::test]
    async fn tool_is_named_for_its_server() {
        let server = Arc::new(McpServer::with_tools("git", vec![definition("commit")]));
        let tool = McpTool::new(definition("commit"), server);

        assert_eq!(tool.name(), "git_commit");
        assert_eq!(tool.label(), "commit");
    }

    #[tokio::test]
    async fn calling_a_dead_server_reports_the_server_name() {
        let server = Arc::new(McpServer::with_tools("git", vec![definition("commit")]));
        let tool = McpTool::new(definition("commit"), server);

        let result = tool.execute(serde_json::json!({})).await;

        assert!(result.outcome.is_error());
    }
}
