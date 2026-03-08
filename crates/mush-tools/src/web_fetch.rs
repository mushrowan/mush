//! web fetch tool - fetch URL content and convert to markdown/text

use mush_agent::tool::{AgentTool, ToolResult};

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024; // 5MB
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_CHARS: usize = 50_000;

pub struct WebFetchTool;

impl Default for WebFetchTool {
    fn default() -> Self {
        Self
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn label(&self) -> &str {
        "Web Fetch"
    }
    fn description(&self) -> &str {
        "Fetch content from a URL and return it as markdown, plain text, or raw HTML. \
         Supports web pages, API endpoints, and documentation. HTML pages are converted \
         to readable markdown by default, preserving headings, links, lists, and code blocks."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch (must start with http:// or https://)"
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "text", "html"],
                    "description": "output format: markdown (default, converts HTML), text (strips tags), html (raw)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "timeout in seconds (default 30, max 120)"
                }
            },
            "required": ["url"]
        })
    }

    fn execute(
        &self,
        args: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
        Box::pin(async move {
            let url = match args["url"].as_str() {
                Some(u) => u,
                None => return ToolResult::error("missing required parameter: url"),
            };

            if !url.starts_with("http://") && !url.starts_with("https://") {
                return ToolResult::error("URL must start with http:// or https://");
            }

            let format = args["format"].as_str().unwrap_or("markdown");
            let timeout_secs = args["timeout"]
                .as_u64()
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .min(MAX_TIMEOUT_SECS);

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());

            let response = match client
                .get(url)
                .header("user-agent", "Mozilla/5.0 (compatible; mush/0.1)")
                .header(
                    "accept",
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .header("accept-language", "en-US,en;q=0.9")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) if e.is_timeout() => return ToolResult::error("request timed out"),
                Err(e) => return ToolResult::error(format!("fetch failed: {e}")),
            };

            if !response.status().is_success() {
                return ToolResult::error(format!(
                    "HTTP {}: {}",
                    response.status(),
                    response.status().canonical_reason().unwrap_or("error")
                ));
            }

            // check size
            if let Some(len) = response.content_length()
                && len as usize > MAX_RESPONSE_SIZE
            {
                return ToolResult::error(format!(
                    "response too large ({len} bytes, max {MAX_RESPONSE_SIZE})"
                ));
            }

            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let bytes = match response.bytes().await {
                Ok(b) if b.len() > MAX_RESPONSE_SIZE => {
                    return ToolResult::error("response too large");
                }
                Ok(b) => b,
                Err(e) => return ToolResult::error(format!("failed to read body: {e}")),
            };

            let body = String::from_utf8_lossy(&bytes);
            let is_html = content_type.contains("text/html") || content_type.contains("xhtml");

            let output = match format {
                "markdown" if is_html => htmd::convert(&body).unwrap_or_else(|_| body.to_string()),
                "text" if is_html => strip_html_tags(&body),
                _ => body.to_string(),
            };

            // truncate if needed (find char boundary to avoid panic)
            let output = if output.len() > MAX_OUTPUT_CHARS {
                let end = output.floor_char_boundary(MAX_OUTPUT_CHARS);
                let truncated = &output[..end];
                format!(
                    "{truncated}\n\n... (truncated, {} total chars)",
                    output.len()
                )
            } else {
                output
            };

            ToolResult::text(output)
        })
    }
}

/// strip HTML tags, keeping just text content
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut last_was_space = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            c if c.is_whitespace() => {
                if !last_was_space {
                    result.push(' ');
                    last_was_space = true;
                }
            }
            c => {
                result.push(c);
                last_was_space = false;
            }
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_required_url() {
        let tool = WebFetchTool::new();
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "url"));
    }

    #[test]
    fn strip_simple_html() {
        let html = "<p>hello <b>world</b></p>";
        assert_eq!(strip_html_tags(html), "hello world");
    }

    #[test]
    fn strip_collapses_whitespace() {
        let html = "<p>hello   \n\n  world</p>";
        assert_eq!(strip_html_tags(html), "hello world");
    }

    #[test]
    fn strip_empty() {
        assert_eq!(strip_html_tags(""), "");
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let tool = WebFetchTool::new();
        let result = tool
            .execute(serde_json::json!({"url": "ftp://example.com"}))
            .await;
        assert!(result.outcome.is_error());
    }
}
