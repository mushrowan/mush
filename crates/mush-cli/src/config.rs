//! configuration loading
//!
//! reads from ~/.config/mush/config.toml (or $MUSH_CONFIG_DIR)

use std::path::PathBuf;

use serde::Deserialize;

/// top-level config
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    pub thinking: Option<bool>,
    pub max_tokens: Option<u64>,
    pub max_turns: Option<usize>,
    pub system_prompt: Option<String>,
    pub api_keys: ApiKeys,
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

/// load config, returning default if file doesn't exist
pub fn load_config() -> Config {
    let path = config_dir().join("config.toml");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
model = "claude-sonnet-4-20250514"
thinking = true
max_tokens = 8192
system_prompt = "you are helpful"

[api_keys]
anthropic = "sk-ant-test"
openrouter = "sk-or-test"
"#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(config.thinking, Some(true));
        assert_eq!(config.max_tokens, Some(8192));
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
    fn load_missing_config_returns_default() {
        // set config dir to a temp path that doesn't exist
        let config = Config::default();
        assert!(config.model.is_none());
    }
}
