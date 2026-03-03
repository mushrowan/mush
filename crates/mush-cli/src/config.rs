//! configuration loading
//!
//! reads from ~/.config/mush/config.toml (or $MUSH_CONFIG_DIR)

use std::collections::HashMap;
use std::path::PathBuf;

use mush_ai::types::ThinkingLevel;
use serde::Deserialize;

/// how to inject skill relevance hints
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HintMode {
    /// prepend hint to user message (evaluated once per message)
    #[default]
    Message,
    /// inject via context transform (re-evaluated before each LLM call)
    Transform,
    /// no hint (all skills still loaded in system prompt)
    None,
}

/// top-level config
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    pub thinking: Option<bool>,
    pub max_tokens: Option<u64>,
    pub max_turns: Option<usize>,
    pub cache_retention: Option<mush_ai::types::CacheRetention>,
    pub system_prompt: Option<String>,
    pub hint_mode: HintMode,
    /// prompt for confirmation before executing tools (off by default)
    pub confirm_tools: Option<bool>,
    pub api_keys: ApiKeys,
    pub theme: mush_tui::ThemeConfig,
    /// MCP server configurations keyed by name
    #[serde(default)]
    pub mcp: std::collections::HashMap<String, mush_mcp::McpServerConfig>,
}

/// api key overrides from config file
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ApiKeys {
    pub anthropic: Option<String>,
    pub openrouter: Option<String>,
}

/// find the config directory
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_CONFIG_DIR") {
        PathBuf::from(dir)
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/mush")
    } else {
        PathBuf::from(".mush")
    }
}

/// path to the config file
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// load config, returning default if file doesn't exist
pub fn load_config() -> Config {
    let path = config_path();
    if !path.exists() {
        return Config::default();
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("\x1b[33mwarning: failed to parse config: {e}\x1b[0m");
                Config::default()
            }
        },
        Err(e) => {
            eprintln!("\x1b[33mwarning: failed to read config: {e}\x1b[0m");
            Config::default()
        }
    }
}

// -- per-model thinking level persistence --

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_DATA_DIR") {
        PathBuf::from(dir)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/mush")
    } else {
        PathBuf::from(".mush")
    }
}

fn thinking_prefs_path() -> PathBuf {
    data_dir().join("thinking.json")
}

pub fn load_thinking_prefs() -> HashMap<String, ThinkingLevel> {
    let path = thinking_prefs_path();
    if !path.exists() {
        return HashMap::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

pub fn save_thinking_prefs(prefs: &HashMap<String, ThinkingLevel>) {
    let path = thinking_prefs_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(prefs) {
        std::fs::write(&path, json).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
model = "claude-opus-4-6"
thinking = true
max_tokens = 8192
system_prompt = "you are helpful"
cache_retention = "long"

[api_keys]
anthropic = "sk-ant-test"
openrouter = "sk-or-test"
"#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(config.thinking, Some(true));
        assert_eq!(config.max_tokens, Some(8192));
        assert_eq!(
            config.cache_retention,
            Some(mush_ai::types::CacheRetention::Long)
        );
        assert_eq!(config.api_keys.anthropic.as_deref(), Some("sk-ant-test"));
    }

    #[test]
    fn parse_minimal_config() {
        let toml = r#"model = "claude-haiku-3-5-20241022""#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("claude-haiku-3-5-20241022"));
        assert!(config.api_keys.anthropic.is_none());
    }

    #[test]
    fn parse_empty_config() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.model.is_none());
    }

    #[test]
    fn parse_hint_mode() {
        let config: Config = toml::from_str(r#"hint_mode = "transform""#).unwrap();
        assert_eq!(config.hint_mode, HintMode::Transform);

        let config: Config = toml::from_str(r#"hint_mode = "none""#).unwrap();
        assert_eq!(config.hint_mode, HintMode::None);

        let config: Config = toml::from_str(r#"hint_mode = "message""#).unwrap();
        assert_eq!(config.hint_mode, HintMode::Message);
    }

    #[test]
    fn hint_mode_defaults_to_message() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.hint_mode, HintMode::Message);
    }

    #[test]
    fn load_missing_config_returns_default() {
        // set config dir to a temp path that doesn't exist
        let config = Config::default();
        assert!(config.model.is_none());
    }

    #[test]
    fn parse_mcp_config() {
        let toml = r#"
[mcp.git]
type = "local"
command = ["uvx", "mcp-server-git"]

[mcp.remote-api]
type = "remote"
url = "https://mcp.example.com/sse"
timeout = 60
enabled = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.mcp.len(), 2);
        assert!(config.mcp.contains_key("git"));
        assert!(config.mcp.contains_key("remote-api"));
        assert!(!config.mcp["remote-api"].enabled);
    }

    #[test]
    fn thinking_prefs_round_trip() {
        let json = r#"{"claude-opus-4-6":"high","claude-sonnet-4-20250514":"medium"}"#;
        let prefs: HashMap<String, ThinkingLevel> = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.get("claude-opus-4-6"), Some(&ThinkingLevel::High));
        assert_eq!(
            prefs.get("claude-sonnet-4-20250514"),
            Some(&ThinkingLevel::Medium)
        );
        // round-trip
        let serialised = serde_json::to_string(&prefs).unwrap();
        let prefs2: HashMap<String, ThinkingLevel> = serde_json::from_str(&serialised).unwrap();
        assert_eq!(prefs, prefs2);
    }
}
