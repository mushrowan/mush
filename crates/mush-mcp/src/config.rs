//! MCP server configuration types

use serde::Deserialize;
use std::collections::HashMap;

/// configuration for an MCP server
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    #[serde(flatten)]
    pub server_type: McpServerType,
    /// whether the server is enabled (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// timeout in seconds for requests (default: 30)
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// environment variables to set (local servers only)
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

/// how to connect to the MCP server
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerType {
    /// local server spawned as a subprocess (stdio transport)
    Local {
        /// command and arguments to run the server
        command: Vec<String>,
    },
    /// remote server accessed over HTTP
    Remote {
        /// URL of the MCP server
        url: String,
        /// optional HTTP headers
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_disabled_config() {
        let toml = r#"
            type = "local"
            command = ["npx", "some-server"]
            enabled = false
        "#;
        let config: McpServerConfig = toml::from_str(toml).unwrap();
        assert!(!config.enabled);
    }
}
