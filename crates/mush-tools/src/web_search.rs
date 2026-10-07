use std::sync::Arc;

use mush_agent::tool::{AgentTool, ToolResult, parse_tool_args};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const DEFAULT_NUM_RESULTS: u32 = 8;
const TIMEOUT_SECS: u64 = 25;
const EXA_ID: &str = "exa";
const SEARX_ID: &str = "searx";
const NO_RESULTS: &str = "no results found. try a different query.";

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

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct WebSearchConfig {
    pub default_provider: Option<String>,
    pub searx: Option<SearxConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearxConfig {
    pub base_url: String,
    #[serde(default)]
    pub auth: Option<SearchAuth>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SearchAuth {
    Basic { username: String, password: String },
    Bearer { token: String },
    Header { name: String, value: String },
}

#[async_trait::async_trait]
trait SearchProvider: Send + Sync {
    fn id(&self) -> &'static str;
    async fn search(
        &self,
        query: &str,
        num_results: u32,
        search_type: SearchType,
    ) -> Result<String, String>;
}

struct ExaProvider {
    client: mush_ai::http::HttpClient,
}

#[async_trait::async_trait]
impl SearchProvider for ExaProvider {
    fn id(&self) -> &'static str {
        EXA_ID
    }

    async fn search(
        &self,
        query: &str,
        num_results: u32,
        search_type: SearchType,
    ) -> Result<String, String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {
                    "query": query,
                    "type": search_type.as_str(),
                    "numResults": num_results,
                    "livecrawl": "fallback",
                }
            }
        });

        let response = self
            .client
            .post(EXA_MCP_URL)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("search request failed: {e}"))?;
        let text = success_text(response, "search").await?;

        if let Some(result) = parse_sse_response(&text) {
            return result;
        }

        serde_json::from_str::<McpResponse>(&text)
            .ok()
            .and_then(McpResponse::outcome)
            .unwrap_or_else(|| Ok(NO_RESULTS.into()))
    }
}

struct SearxProvider {
    client: mush_ai::http::HttpClient,
    base_url: String,
    auth: Option<SearchAuth>,
}

impl SearxProvider {
    fn new(client: mush_ai::http::HttpClient, config: SearxConfig) -> Self {
        Self {
            client,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            auth: config.auth,
        }
    }
}

#[async_trait::async_trait]
impl SearchProvider for SearxProvider {
    fn id(&self) -> &'static str {
        SEARX_ID
    }

    async fn search(
        &self,
        query: &str,
        num_results: u32,
        _search_type: SearchType,
    ) -> Result<String, String> {
        let url = format!(
            "{}/search?q={}&format=json",
            self.base_url,
            urlencoding::encode(query)
        );
        let mut req = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS));
        if let Some((name, value)) = auth_header(self.auth.as_ref()) {
            req = req.header(name.as_str(), value);
        }

        let response = req
            .send()
            .await
            .map_err(|e| format!("searx request failed: {e}"))?;
        let text = success_text(response, "searx").await?;
        let parsed: SearxResponse =
            serde_json::from_str(&text).map_err(|e| format!("failed to parse searx json: {e}"))?;
        Ok(format_searx_results(&parsed, num_results))
    }
}

async fn success_text(response: reqwest::Response, service: &str) -> Result<String, String> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("{service} error ({status}): {body}"));
    }
    response
        .text()
        .await
        .map_err(|e| format!("failed to read response: {e}"))
}

fn auth_header(auth: Option<&SearchAuth>) -> Option<(String, String)> {
    use base64::Engine;
    match auth? {
        SearchAuth::Basic { username, password } => {
            let raw = format!("{username}:{password}");
            let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
            Some(("authorization".into(), format!("Basic {encoded}")))
        }
        SearchAuth::Bearer { token } => Some(("authorization".into(), format!("Bearer {token}"))),
        SearchAuth::Header { name, value } => Some((name.clone(), value.clone())),
    }
}

#[derive(Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Deserialize)]
struct SearxResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

fn format_searx_results(resp: &SearxResponse, num_results: u32) -> String {
    if resp.results.is_empty() {
        return NO_RESULTS.into();
    }
    resp.results
        .iter()
        .take(num_results as usize)
        .map(|r| {
            let content = r.content.trim();
            if content.is_empty() {
                format!("{}\n{}", r.title, r.url)
            } else {
                format!("{}\n{}\n{}", r.title, r.url, content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default = "default_num_results")]
    num_results: u32,
    #[serde(rename = "type", default)]
    search_type: SearchType,
    #[serde(default)]
    provider: Option<String>,
}

const fn default_num_results() -> u32 {
    DEFAULT_NUM_RESULTS
}

pub struct WebSearchTool {
    providers: Vec<Arc<dyn SearchProvider>>,
    default_id: String,
}

impl WebSearchTool {
    pub fn new(client: mush_ai::http::HttpClient) -> Self {
        Self::with_config(client, &WebSearchConfig::default())
    }

    pub fn with_config(client: mush_ai::http::HttpClient, config: &WebSearchConfig) -> Self {
        let mut providers: Vec<Arc<dyn SearchProvider>> = vec![Arc::new(ExaProvider {
            client: client.clone(),
        })];
        if let Some(searx) = &config.searx {
            providers.push(Arc::new(SearxProvider::new(client, searx.clone())));
        }
        let default_id = config
            .default_provider
            .as_deref()
            .filter(|id| providers.iter().any(|p| p.id() == *id))
            .unwrap_or(EXA_ID)
            .to_string();
        Self {
            providers,
            default_id,
        }
    }

    fn provider_ids(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.id()).collect()
    }

    fn resolve(&self, requested: Option<&str>) -> Result<&Arc<dyn SearchProvider>, String> {
        let id = requested.unwrap_or(&self.default_id);
        self.providers.iter().find(|p| p.id() == id).ok_or_else(|| {
            format!(
                "unknown search provider '{id}'. available: {}",
                self.provider_ids().join(", ")
            )
        })
    }
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
        "Search the web for up-to-date information. Returns content from the most relevant \
         websites. Use this for current events, recent documentation, API references, and \
         anything beyond the knowledge cutoff. The current year is 2026."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut props = serde_json::json!({
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
        });
        if self.providers.len() > 1
            && let serde_json::Value::Object(ref mut map) = props
        {
            map.insert(
                "provider".into(),
                serde_json::json!({
                    "type": "string",
                    "enum": self.provider_ids(),
                    "description": format!(
                        "which search backend to use (default: {}). omit to use the default",
                        self.default_id
                    )
                }),
            );
        }
        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": ["query"]
        })
    }

    fn output_limit(&self) -> mush_agent::tool::OutputLimit {
        mush_agent::tool::OutputLimit::Head
    }

    async fn execute(&self, args: serde_json::Value) -> ToolResult {
        let args = match parse_tool_args::<WebSearchArgs>(args) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let provider = match self.resolve(args.provider.as_deref()) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        match provider
            .search(&args.query, args.num_results, args.search_type)
            .await
        {
            Ok(text) => ToolResult::text(text),
            Err(e) => ToolResult::error(e),
        }
    }
}

#[derive(Deserialize)]
struct McpResponse {
    result: Option<McpResult>,
    error: Option<McpError>,
}

impl McpResponse {
    fn outcome(self) -> Option<Result<String, String>> {
        if let Some(err) = self.error {
            return Some(Err(format!("exa error: {}", err.message)));
        }
        let content = self.result?.content.into_iter().next()?;
        Some(Ok(content.text))
    }
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

fn parse_sse_response(text: &str) -> Option<Result<String, String>> {
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let outcome = serde_json::from_str::<McpResponse>(data)
            .ok()
            .and_then(McpResponse::outcome);
        if outcome.is_some() {
            return outcome;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_with_searx() -> WebSearchTool {
        WebSearchTool::with_config(
            mush_ai::http::client(),
            &WebSearchConfig {
                default_provider: None,
                searx: Some(SearxConfig {
                    base_url: "https://sear.example.com/".into(),
                    auth: None,
                }),
            },
        )
    }

    #[test]
    fn schema_has_required_query() {
        let tool = WebSearchTool::new(mush_ai::http::client());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "query"));
    }

    #[test]
    fn exa_only_does_not_advertise_provider() {
        let tool = WebSearchTool::new(mush_ai::http::client());
        let schema = tool.parameters_schema();
        assert!(
            schema["properties"]["provider"].is_null(),
            "provider must not be advertised when only one backend exists"
        );
    }

    #[test]
    fn multiple_providers_advertise_provider_enum() {
        let tool = tool_with_searx();
        let schema = tool.parameters_schema();
        let providers = schema["properties"]["provider"]["enum"]
            .as_array()
            .expect("provider enum present with >1 backend");
        assert!(providers.iter().any(|v| v == "exa"));
        assert!(providers.iter().any(|v| v == "searx"));
    }

    #[test]
    fn default_is_exa_when_unset() {
        let tool = tool_with_searx();
        assert_eq!(tool.default_id, "exa");
        assert!(tool.resolve(None).is_ok());
    }

    #[test]
    fn default_provider_override_is_honoured() {
        let tool = WebSearchTool::with_config(
            mush_ai::http::client(),
            &WebSearchConfig {
                default_provider: Some("searx".into()),
                searx: Some(SearxConfig {
                    base_url: "https://sear.example.com".into(),
                    auth: None,
                }),
            },
        );
        assert_eq!(tool.default_id, "searx");
    }

    #[test]
    fn invalid_default_provider_falls_back_to_exa() {
        let tool = WebSearchTool::with_config(
            mush_ai::http::client(),
            &WebSearchConfig {
                default_provider: Some("nonsense".into()),
                searx: None,
            },
        );
        assert_eq!(tool.default_id, "exa");
    }

    #[test]
    fn resolve_rejects_unknown_provider() {
        let tool = tool_with_searx();
        let err = tool.resolve(Some("bing")).err().unwrap();
        assert!(err.contains("unknown search provider"), "got: {err}");
    }

    #[test]
    fn auth_header_encodes_basic_and_bearer() {
        assert_eq!(auth_header(None), None);
        let (name, value) = auth_header(Some(&SearchAuth::Bearer {
            token: "tok".into(),
        }))
        .unwrap();
        assert_eq!(name, "authorization");
        assert_eq!(value, "Bearer tok");
        let (_, basic) = auth_header(Some(&SearchAuth::Basic {
            username: "u".into(),
            password: "p".into(),
        }))
        .unwrap();
        assert_eq!(basic, "Basic dTpw");
        let (name, value) = auth_header(Some(&SearchAuth::Header {
            name: "x-api-key".into(),
            value: "secret".into(),
        }))
        .unwrap();
        assert_eq!(name, "x-api-key");
        assert_eq!(value, "secret");
    }

    #[test]
    fn format_searx_results_joins_entries() {
        let resp = SearxResponse {
            results: vec![
                SearxResult {
                    title: "First".into(),
                    url: "https://a.example".into(),
                    content: "body a".into(),
                },
                SearxResult {
                    title: "Second".into(),
                    url: "https://b.example".into(),
                    content: String::new(),
                },
            ],
        };
        let out = format_searx_results(&resp, 8);
        assert!(out.contains("First\nhttps://a.example\nbody a"));
        assert!(out.contains("Second\nhttps://b.example"));
    }

    #[test]
    fn format_searx_results_respects_num_results() {
        let resp = SearxResponse {
            results: (0..5)
                .map(|i| SearxResult {
                    title: format!("t{i}"),
                    url: format!("https://{i}.example"),
                    content: String::new(),
                })
                .collect(),
        };
        let out = format_searx_results(&resp, 2);
        assert!(out.contains("t0"));
        assert!(out.contains("t1"));
        assert!(!out.contains("t2"), "should cap at num_results");
    }

    #[test]
    fn format_searx_results_empty_is_friendly() {
        let resp = SearxResponse { results: vec![] };
        assert!(format_searx_results(&resp, 8).contains("no results"));
    }

    #[test]
    fn parse_sse_with_content() {
        let sse = r#"data: {"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"search results here"}]}}"#;
        let result = parse_sse_response(sse).unwrap();
        assert_eq!(result.unwrap(), "search results here");
    }

    #[test]
    fn parse_sse_with_error() {
        let sse = r#"data: {"jsonrpc":"2.0","error":{"code":-1,"message":"rate limited"}}"#;
        let result = parse_sse_response(sse).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn parse_sse_no_data_lines() {
        let text = "some random text\nno data here";
        assert!(parse_sse_response(text).is_none());
    }
}
