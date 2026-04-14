//! web search tool - search the web using Exa AI

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use serde::Deserialize;

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const DEFAULT_NUM_RESULTS: u32 = 8;
const TIMEOUT_SECS: u64 = 25;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchType {
    #[default]
    Auto,
    Fast,
}

impl SearchType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_num_results")]
    num_results: u32,
    #[serde(rename = "type", default)]
    search_type: SearchType,
}

const fn default_num_results() -> u32 {
    DEFAULT_NUM_RESULTS
}

pub struct WebSearchTool {
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[derive(Deserialize)]
struct McpResponse {
    result: Option<McpResult>,
    error: Option<McpError>,
}

#[derive(Deserialize)]
struct McpResult {
    content: Vec<McpContent>,
}

#[derive(Deserialize)]
struct McpContent {
    text: String,
}

#[derive(Deserialize)]
struct McpError {
    message: String,
}

#[async_trait::async_trait]
impl AgentTool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn label(&self) -> &str {
        "Web Search"
    }
    fn description(&self) -> &str {
        "Search the web using Exa AI for up-to-date information. Returns content from the most \
         relevant websites. Use this for current events, recent documentation, API references, \
         and anything beyond the knowledge cutoff. The current year is 2026."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "search query. use the current year (2026) when searching for recent info"
                },
                "num_results": {
                    "type": "integer",
                    "description": "number of results to return (default: 8)"
                },
                "type": {
                    "type": "string",
                    "enum": ["auto", "fast"],
                    "description": "search type: auto (balanced, default), fast (quick)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<WebSearchArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {
                    "query": args.query,
                    "type": args.search_type.as_str(),
                    "numResults": args.num_results,
                    "livecrawl": "fallback",
                }
            }
        });

        let response = match self
            .client
            .post(EXA_MCP_URL)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .body(body.to_string())
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("search request failed: {e}")),
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return ToolResult::error(format!("search error ({status}): {body}"));
        }

        let text = match response.text().await {
            Ok(t) => t,
            Err(e) => return ToolResult::error(format!("failed to read response: {e}")),
        };

        if let Some(result) = parse_sse_response(&text) {
            return result;
        }

        let Ok(resp) = serde_json::from_str::<McpResponse>(&text) else {
            return ToolResult::text("no results found. try a different query.");
        };
        if let Some(err) = resp.error {
            ToolResult::error(format!("exa error: {}", err.message))
        } else if let Some(result) = resp.result
            && let Some(content) = result.content.first()
        {
            ToolResult::text(&content.text)
        } else {
            ToolResult::text("no results found. try a different query.")
        }
    }
}

fn parse_sse_response(text: &str) -> Option<ToolResult> {
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(resp) = serde_json::from_str::<McpResponse>(data) else {
            continue;
        };
        if let Some(err) = resp.error {
            return Some(ToolResult::error(format!("exa error: {}", err.message)));
        }
        if let Some(result) = resp.result
            && let Some(content) = result.content.first()
        {
            return Some(ToolResult::text(&content.text));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_query() {
        let tool = WebSearchTool::new(reqwest::Client::new());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "query"));
    }

    #[test]
    fn parse_sse_with_content() {
        let sse = r#"data: {"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"search results here"}]}}"#;
        let result = parse_sse_response(sse).unwrap();
        assert!(result.outcome.is_success());
    }

    #[test]
    fn parse_sse_with_error() {
        let sse = r#"data: {"jsonrpc":"2.0","error":{"code":-1,"message":"rate limited"}}"#;
        let result = parse_sse_response(sse).unwrap();
        assert!(result.outcome.is_error());
    }

    #[test]
    fn parse_sse_no_data_lines() {
        let text = "some random text\nno data here";
        assert!(parse_sse_response(text).is_none());
    }
}
