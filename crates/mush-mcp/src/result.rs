//! shared MCP result conversion

use mush_agent::tool::ToolResult;
use rmcp::model::CallToolResult;

/// convert an MCP `CallToolResult` into an agent `ToolResult`
pub fn convert_call_result(result: CallToolResult) -> ToolResult {
    let is_error = result.is_error.unwrap_or(false);
    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");

    if is_error {
        ToolResult::error(text)
    } else if text.is_empty() {
        ToolResult::text("(no output)")
    } else {
        ToolResult::text(text)
    }
}
