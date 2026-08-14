use http::{HeaderName, HeaderValue};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct McpServerConfig {
    #[serde(flatten)]
    pub server_type: McpServerType,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

impl McpServerConfig {
    #[must_use]
    pub fn request_timeout(&self) -> Option<std::time::Duration> {
        (self.timeout > 0).then(|| std::time::Duration::from_secs(self.timeout))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerType {
    Local {
        command: Vec<String>,
    },
    Remote {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    30
}

pub(crate) fn build_http_config(
    url: &str,
    headers: &HashMap<String, String>,
) -> StreamableHttpClientTransportConfig {
    let mut wire = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        match (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            (Ok(name), Ok(value)) => {
                wire.insert(name, value);
            }
            _ => tracing::warn!(header = %name, "skipping unusable MCP header"),
        }
    }

    StreamableHttpClientTransportConfig::with_uri(url).custom_headers(wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    #[test]
    fn parse_local_config() {
        let toml = r#"
            type = "local"
            command = ["uvx", "mcp-server-git"]
        "#;
        let config: McpServerConfig = toml::from_str(toml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.timeout, 30);
        match &config.server_type {
            McpServerType::Local { command } => {
                assert_eq!(command, &["uvx", "mcp-server-git"]);
            }
            _ => panic!("expected local"),
        }
    }

    #[test]
    fn parse_remote_config() {
        let toml = r#"
            type = "remote"
            url = "https://mcp.example.com/sse"
            timeout = 60
        "#;
        let config: McpServerConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.timeout, 60);
        match &config.server_type {
            McpServerType::Remote { url, .. } => {
                assert_eq!(url, "https://mcp.example.com/sse");
            }
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn remote_headers_reach_the_transport() {
        let config: McpServerConfig = toml::from_str(
            r#"
            type = "remote"
            url = "https://mcp.example.com/mcp"

            [headers]
            x-tenant = "lounge"
            authorization = "Bearer sekrit"
        "#,
        )
        .unwrap();

        let McpServerType::Remote { url, headers } = &config.server_type else {
            panic!("expected remote");
        };
        assert_eq!(headers["x-tenant"], "lounge");

        let built = build_http_config(url, headers);
        assert_eq!(
            built
                .custom_headers
                .get(&HeaderName::from_static("x-tenant")),
            Some(&HeaderValue::from_static("lounge"))
        );
        assert_eq!(
            built
                .custom_headers
                .get(&HeaderName::from_static("authorization")),
            Some(&HeaderValue::from_static("Bearer sekrit"))
        );
    }

    #[test]
    fn unusable_header_is_skipped_rather_than_failing_the_connection() {
        let config: McpServerConfig = toml::from_str(
            r#"
            type = "remote"
            url = "https://mcp.example.com/mcp"

            [headers]
            "bad header name" = "x"
            x-good = "y"
        "#,
        )
        .unwrap();

        let McpServerType::Remote { url, headers } = &config.server_type else {
            panic!("expected remote");
        };

        let built = build_http_config(url, headers);
        assert_eq!(built.custom_headers.len(), 1);
        assert!(
            built
                .custom_headers
                .contains_key(&HeaderName::from_static("x-good"))
        );
    }

    #[test]
    fn parse_disabled_config() {
        let toml = r#"
            type = "local"
            command = ["npx", "some-server"]
            enabled = false
        "#;
        let config: McpServerConfig = toml::from_str(toml).unwrap();
        assert!(!config.enabled);
    }

    #[test]
    fn zero_timeout_means_no_deadline() {
        let with_deadline: McpServerConfig = toml::from_str(
            r#"
            type = "local"
            command = ["x"]
            timeout = 45
        "#,
        )
        .unwrap();
        assert_eq!(
            with_deadline.request_timeout(),
            Some(std::time::Duration::from_secs(45))
        );

        let opted_out: McpServerConfig = toml::from_str(
            r#"
            type = "local"
            command = ["x"]
            timeout = 0
        "#,
        )
        .unwrap();
        assert_eq!(opted_out.request_timeout(), None);
    }
}
