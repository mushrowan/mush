use std::collections::HashMap;
use std::path::PathBuf;

use mush_ai::types::{EffortLevel, ThinkingLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::output::Style;

fn deserialise_thinking<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<ThinkingLevel>, D::Error> {
    Option::<ThinkingConfigWire>::deserialize(d).map(|opt| {
        opt.map(|raw| match raw {
            ThinkingConfigWire::Bool(true) => ThinkingLevel::High,
            ThinkingConfigWire::Bool(false) => ThinkingLevel::Off,
            ThinkingConfigWire::Level(l) => l,
        })
    })
}

/// TODO: this bool-or-enum legacy form generates an `anyOf` in the
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
enum ThinkingConfigWire {
    Bool(bool),
    Level(ThinkingLevel),
}

pub use mush_tui::StatusBarConfig;
pub use mush_tui::TerminalPolicy;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct KernelConfig {
    pub python: Option<std::path::PathBuf>,
    pub managed: bool,
    pub extra_packages: Vec<String>,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            python: None,
            managed: true,
            extra_packages: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[schemars(extend("x-nixcfg-name" = "mush", "x-nixcfg-config-format" = "toml"))]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    #[serde(default, deserialize_with = "deserialise_thinking")]
    #[schemars(with = "Option<ThinkingConfigWire>")]
    pub thinking: Option<ThinkingLevel>,
    pub effort: Option<EffortLevel>,
    pub max_tokens: Option<u64>,
    pub max_turns: Option<usize>,
    pub cache_retention: Option<mush_ai::types::CacheRetention>,
    pub debug_cache: bool,
    pub system_prompt: Option<String>,
    pub log_filter: Option<String>,
    pub thinking_display: mush_tui::ThinkingDisplay,
    #[serde(default)]
    pub force_budget_thinking: bool,
    #[serde(default)]
    pub auto_compact: bool,
    #[serde(default)]
    pub auto_fork_compact: bool,
    #[serde(default)]
    pub rlm: bool,
    pub rlm_max_depth: Option<u32>,
    #[serde(default)]
    pub kernel: KernelConfig,
    pub confirm_tools: bool,
    pub show_cost: bool,
    pub show_usage_lines: bool,
    pub show_token_counters: bool,
    #[serde(default)]
    pub status_bar: StatusBarConfig,
    #[serde(default = "default_cache_timer")]
    pub cache_timer: bool,
    pub terminal: TerminalPolicy,
    pub api_keys: ApiKeys,
    pub theme: mush_tui::ThemeConfig,
    #[serde(default)]
    pub mcp: std::collections::HashMap<String, mush_mcp::McpServerConfig>,
    #[serde(default)]
    pub dynamic_mcp: bool,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub repo_map: RepoMapConfig,
    #[serde(default)]
    pub lsp: LspConfig,
    #[serde(default)]
    pub model_tiers: HashMap<String, String>,
    #[serde(default)]
    pub favourite_models: Vec<String>,
    pub compaction_model: Option<String>,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub keys: mush_tui::KeysConfig,
    #[serde(default)]
    #[schemars(with = "Vec<String>")]
    pub forbidden_chars: Vec<char>,
    #[serde(default)]
    pub forbidden_char_classes: Vec<mush_tools::forbidden_chars::CharClass>,
    #[serde(default = "default_max_image_mb")]
    pub max_image_mb: u32,
    #[serde(default)]
    pub web_search: mush_tools::web_search::WebSearchConfig,
}

fn default_max_image_mb() -> u32 {
    5
}

fn default_cache_timer() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: None,
            thinking: None,
            effort: None,
            max_tokens: None,
            max_turns: None,
            cache_retention: None,
            debug_cache: false,
            system_prompt: None,
            log_filter: None,
            thinking_display: mush_tui::ThinkingDisplay::default(),
            force_budget_thinking: false,
            auto_compact: false,
            auto_fork_compact: false,
            rlm: false,
            rlm_max_depth: None,
            kernel: KernelConfig::default(),
            confirm_tools: false,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar: StatusBarConfig::default(),
            cache_timer: default_cache_timer(),
            terminal: TerminalPolicy::default(),
            api_keys: ApiKeys::default(),
            theme: mush_tui::ThemeConfig::default(),
            mcp: std::collections::HashMap::default(),
            dynamic_mcp: false,
            hooks: HooksConfig::default(),
            repo_map: RepoMapConfig::default(),
            lsp: LspConfig::default(),
            model_tiers: HashMap::default(),
            favourite_models: Vec::new(),
            compaction_model: None,
            settings: Settings::default(),
            keys: mush_tui::KeysConfig::default(),
            forbidden_chars: Vec::new(),
            forbidden_char_classes: Vec::new(),
            max_image_mb: default_max_image_mb(),
            web_search: mush_tools::web_search::WebSearchConfig::default(),
        }
    }
}

pub type SettingsScope = mush_tui::settings::SettingsScope;

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub scope: SettingsScope,
    pub anthropic_betas: mush_ai::types::AnthropicBetas,
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct HooksConfig {
    pub pre_session: Vec<HookEntry>,
    pub pre_tool_use: Vec<HookEntry>,
    pub post_tool_use: Vec<HookEntry>,
    pub stop: Vec<HookEntry>,
    pub post_compaction: Vec<HookEntry>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HookEntry {
    #[serde(default = "default_match")]
    pub r#match: String,
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub blocking: bool,
}

fn default_match() -> String {
    "*".into()
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct LspConfig {
    pub diagnostics: bool,
    #[serde(default)]
    pub servers: HashMap<String, LspServerEntry>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone, PartialEq, Eq)]
pub struct LspServerEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct RepoMapConfig {
    pub enabled: bool,
    pub budget: usize,
}

impl Default for RepoMapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            budget: 8192,
        }
    }
}

impl Config {
    pub fn lifecycle_hooks(&self) -> mush_agent::LifecycleHooks {
        fn convert(entries: &[HookEntry]) -> Vec<mush_agent::LifecycleHook> {
            entries
                .iter()
                .map(|e| mush_agent::LifecycleHook {
                    tool_match: e.r#match.clone(),
                    command: e.command.clone(),
                    timeout: std::time::Duration::from_secs(e.timeout),
                    blocking: e.blocking,
                })
                .collect()
        }

        let mut hooks = mush_agent::LifecycleHooks::default();
        hooks.set(
            mush_agent::HookPoint::PreSession,
            convert(&self.hooks.pre_session),
        );
        hooks.set(
            mush_agent::HookPoint::PreToolUse,
            convert(&self.hooks.pre_tool_use),
        );
        hooks.set(
            mush_agent::HookPoint::PostToolUse,
            convert(&self.hooks.post_tool_use),
        );
        hooks.set(mush_agent::HookPoint::Stop, convert(&self.hooks.stop));
        hooks.set(
            mush_agent::HookPoint::PostCompaction,
            convert(&self.hooks.post_compaction),
        );
        hooks
    }
}

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ApiKeys {
    pub anthropic: Option<mush_ai::types::ApiKey>,
    pub openrouter: Option<mush_ai::types::ApiKey>,
    pub openai: Option<mush_ai::types::ApiKey>,
    #[serde(flatten)]
    pub other: HashMap<String, mush_ai::types::ApiKey>,
}

impl ApiKeys {
    #[must_use]
    pub fn to_map(&self) -> HashMap<String, mush_ai::types::ApiKey> {
        let mut map = HashMap::new();
        if let Some(key) = &self.anthropic {
            map.insert("anthropic".into(), key.clone());
        }
        if let Some(key) = &self.openrouter {
            map.insert("openrouter".into(), key.clone());
        }
        if let Some(key) = &self.openai {
            map.insert("openai".into(), key.clone());
        }
        for (k, v) in &self.other {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        map
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&mush_ai::types::ApiKey> {
        match name {
            "anthropic" => self.anthropic.as_ref(),
            "openrouter" => self.openrouter.as_ref(),
            "openai" => self.openai.as_ref(),
            _ => self.other.get(name),
        }
    }
}

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

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load_config() -> Config {
    let path = config_path();
    let mut config = if !path.exists() {
        Config::default()
    } else {
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<Config>(&content) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!(
                        "{}",
                        Style::Yellow.stderr(format_args!("warning: failed to parse config: {e}"))
                    );
                    Config::default()
                }
            },
            Err(e) => {
                eprintln!(
                    "{}",
                    Style::Yellow.stderr(format_args!("warning: failed to read config: {e}"))
                );
                Config::default()
            }
        }
    };

    let global = config_dir().join("settings.toml");
    if let Some(persisted) = try_load_persisted_settings(&global) {
        config.settings.anthropic_betas = persisted.anthropic_betas;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let repo = cwd.join(".mush").join("settings.toml");
        if let Some(persisted) = try_load_persisted_settings(&repo) {
            config.settings.anthropic_betas = persisted.anthropic_betas;
        }
    }

    config
}

fn try_load_persisted_settings(
    path: &std::path::Path,
) -> Option<mush_tui::settings::PersistedSettings> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!(
                "{}",
                Style::Yellow.stderr(format_args!(
                    "warning: failed to parse {}: {e}",
                    path.display()
                ))
            );
            None
        }
    }
}

fn thinking_prefs_path() -> PathBuf {
    mush_session::data_dir().join("thinking.json")
}

pub fn load_thinking_prefs() -> mush_tui::ThinkingPrefs {
    let path = thinking_prefs_path();
    if !path.exists() {
        return mush_tui::ThinkingPrefs::default();
    }
    let mut prefs = match std::fs::read_to_string(&path) {
        Ok(content) => {
            serde_json::from_str::<mush_tui::ThinkingPrefs>(&content).unwrap_or_default()
        }
        Err(_) => mush_tui::ThinkingPrefs::default(),
    };
    prefs.prune_missing_dirs();
    prefs
}

pub fn save_thinking_prefs(prefs: &mush_tui::ThinkingPrefs) {
    let path = thinking_prefs_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(prefs) {
        let _ = mush_ai::private_io::write_atomic(&path, json);
    }
}

fn last_model_path() -> PathBuf {
    mush_session::data_dir().join("last-model.json")
}

pub fn load_last_models() -> mush_tui::LastModels {
    let path = last_model_path();
    if !path.exists() {
        return mush_tui::LastModels::default();
    }
    let mut last = match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<mush_tui::LastModels>(&content).unwrap_or_default(),
        Err(_) => mush_tui::LastModels::default(),
    };
    last.prune_missing_dirs();
    last
}

pub fn save_last_models(last: &mush_tui::LastModels) {
    let path = last_model_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(last) {
        let _ = mush_ai::private_io::write_atomic(&path, json);
    }
}

fn favourite_models_path() -> PathBuf {
    mush_session::data_dir().join("favourite-models.json")
}

pub fn load_favourite_models() -> Vec<String> {
    load_favourite_models_from(&favourite_models_path())
}

pub(crate) fn load_favourite_models_from(path: &std::path::Path) -> Vec<String> {
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

pub fn save_favourite_models(models: &[String]) {
    save_favourite_models_to(&favourite_models_path(), models);
}

pub(crate) fn save_favourite_models_to(path: &std::path::Path, models: &[String]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(models) {
        let _ = mush_ai::private_io::write_atomic(path, json);
    }
}

#[must_use]
pub fn resolve_favourites(from_config: &[String], from_disk: Vec<String>) -> (Vec<String>, bool) {
    if from_config.is_empty() {
        (from_disk, false)
    } else {
        (from_config.to_vec(), true)
    }
}

fn recent_models_path() -> PathBuf {
    mush_session::data_dir().join("recent-models.json")
}

pub fn load_recent_models() -> Vec<String> {
    load_recent_models_from(&recent_models_path())
}

pub(crate) fn load_recent_models_from(path: &std::path::Path) -> Vec<String> {
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

pub fn save_recent_models(models: &[String]) {
    save_recent_models_to(&recent_models_path(), models);
}

pub(crate) fn save_recent_models_to(path: &std::path::Path, models: &[String]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(models) {
        let _ = mush_ai::private_io::write_atomic(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
model = "claude-opus-4-7"
thinking = true
max_tokens = 8192
system_prompt = "you are helpful"
cache_retention = "long"
debug_cache = true

[api_keys]
anthropic = "sk-ant-test"
openrouter = "sk-or-test"
openai = "sk-openai-test"
groq = "sk-groq-test"
xai = "sk-xai-test"
"#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(config.thinking, Some(ThinkingLevel::High));
        assert_eq!(config.max_tokens, Some(8192));
        assert_eq!(
            config.cache_retention,
            Some(mush_ai::types::CacheRetention::Long)
        );
        assert!(config.debug_cache);
        assert_eq!(
            config.api_keys.anthropic.as_ref().map(|k| k.expose()),
            Some("sk-ant-test")
        );
        assert_eq!(
            config.api_keys.openai.as_ref().map(|k| k.expose()),
            Some("sk-openai-test")
        );
        assert_eq!(
            config
                .api_keys
                .get("groq")
                .map(mush_ai::types::ApiKey::expose),
            Some("sk-groq-test")
        );
        assert_eq!(
            config
                .api_keys
                .get("xai")
                .map(mush_ai::types::ApiKey::expose),
            Some("sk-xai-test")
        );
        assert!(config.api_keys.get("unknown-provider").is_none());
        let map = config.api_keys.to_map();
        assert_eq!(map.get("groq").map(|k| k.expose()), Some("sk-groq-test"));
        assert_eq!(
            map.get("anthropic").map(|k| k.expose()),
            Some("sk-ant-test")
        );
    }

    #[test]
    fn api_keys_debug_output_redacts_secrets() {
        let toml = r#"
[api_keys]
anthropic = "sk-ant-VERY-SECRET-DO-NOT-LEAK-abc123"
xai = "sk-xai-ALSO-SECRET-xyz789"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let debug = format!("{:?}", config.api_keys);
        assert!(
            !debug.contains("VERY-SECRET-DO-NOT-LEAK-abc123"),
            "anthropic secret leaked in Debug: {debug}"
        );
        assert!(
            !debug.contains("ALSO-SECRET-xyz789"),
            "xai secret leaked in Debug: {debug}"
        );
    }

    #[test]
    fn settings_default_matches_anthropic_betas_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.settings.scope, SettingsScope::Session);
        assert_eq!(
            config.settings.anthropic_betas,
            mush_ai::types::AnthropicBetas::default()
        );
    }

    #[test]
    fn a_removed_beta_key_still_loads() {
        let toml = r#"
[settings.anthropic_betas]
redact_thinking = true
server_side_fallback = true
fallbacks = true
advisor = false
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(!config.settings.anthropic_betas.advisor);
    }

    #[test]
    fn settings_parses_scope_and_betas() {
        let toml = r#"
[settings]
scope = "global"

[settings.anthropic_betas]
context_1m = false
thinking_token_count = true
advisor = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.settings.scope, SettingsScope::Global);
        assert!(!config.settings.anthropic_betas.context_1m);
        assert!(config.settings.anthropic_betas.thinking_token_count);
        assert!(config.settings.anthropic_betas.advisor);
        assert!(config.settings.anthropic_betas.effort);
        assert!(config.settings.anthropic_betas.context_management);
        assert!(config.settings.anthropic_betas.advanced_tool_use);
    }

    #[test]
    fn settings_scope_variants_parse() {
        for (s, expected) in [
            ("global", SettingsScope::Global),
            ("disabled", SettingsScope::Disabled),
            ("repo", SettingsScope::Repo),
            ("session", SettingsScope::Session),
        ] {
            let toml = format!("[settings]\nscope = {s:?}\n");
            let config: Config = toml::from_str(&toml).unwrap();
            assert_eq!(config.settings.scope, expected, "scope={s}");
        }
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
    fn parse_terminal_policy() {
        let config: Config = toml::from_str(
            r#"
[terminal]
keyboard_enhancement = "disabled"
image_probe = "disabled"
"#,
        )
        .unwrap();
        assert_eq!(
            config.terminal.keyboard_enhancement,
            mush_tui::KeyboardEnhancementMode::Disabled
        );
        assert_eq!(
            config.terminal.image_probe,
            mush_tui::ImageProbeMode::Disabled
        );
    }

    #[test]
    fn parse_mouse_scroll_lines() {
        let config: Config = toml::from_str(
            r#"
[terminal]
mouse_scroll_lines = 5
"#,
        )
        .unwrap();
        assert_eq!(config.terminal.mouse_scroll_lines, 5);
    }

    #[test]
    fn terminal_policy_defaults_match_tui_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.terminal, TerminalPolicy::default());
    }

    #[test]
    fn thinking_config_accepts_level_string() {
        let config: Config = toml::from_str(r#"thinking = "medium""#).unwrap();
        assert_eq!(config.thinking, Some(ThinkingLevel::Medium));
    }

    #[test]
    fn thinking_config_accepts_bool_false() {
        let config: Config = toml::from_str("thinking = false").unwrap();
        assert_eq!(config.thinking, Some(ThinkingLevel::Off));
    }

    #[test]
    fn thinking_config_defaults_to_none() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.thinking, None);
    }

    #[test]
    fn auto_compact_defaults_to_false() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.auto_compact);
    }

    #[test]
    fn auto_compact_can_be_enabled() {
        let config: Config = toml::from_str("auto_compact = true").unwrap();
        assert!(config.auto_compact);
    }

    #[test]
    fn auto_fork_compact_defaults_to_false() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.auto_fork_compact);
    }

    #[test]
    fn auto_fork_compact_can_be_enabled() {
        let config: Config = toml::from_str("auto_fork_compact = true").unwrap();
        assert!(config.auto_fork_compact);
    }

    #[test]
    fn cache_timer_defaults_to_true() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.cache_timer);
    }

    #[test]
    fn cache_timer_can_be_disabled() {
        let config: Config = toml::from_str("cache_timer = false").unwrap();
        assert!(!config.cache_timer);
    }

    #[test]
    fn cache_timer_default_matches_struct_default() {
        let from_empty: Config = toml::from_str("").unwrap();
        let from_default = Config::default();
        assert_eq!(from_empty.cache_timer, from_default.cache_timer);
    }

    #[test]
    fn flat_keys_config_is_rejected() {
        let err = toml::from_str::<Config>(
            r#"
            [keys]
            edit_steering = "alt+k"
            "#,
        )
        .expect_err("flat keys should be rejected");

        assert!(
            err.to_string()
                .contains("old flat [keys] entry `edit_steering`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn scoped_keys_config_is_accepted() {
        let config = toml::from_str::<Config>(
            r#"
            [keys.insert]
            edit_steering = "alt+k"
            "#,
        )
        .expect("scoped keys should be accepted");

        assert!(config.keys.insert.raw.contains_key("edit_steering"));
    }

    #[test]
    fn load_missing_config_returns_default() {
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
    fn parse_lifecycle_hooks() {
        let toml = r#"
[[hooks.post_tool_use]]
match = "edit|write"
command = "cargo clippy --message-format=short 2>&1 | head -20"
timeout = 15
blocking = false

[[hooks.post_tool_use]]
match = "*"
command = "echo done"

[[hooks.stop]]
command = "cargo test 2>&1 | tail -10"
blocking = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.hooks.post_tool_use.len(), 2);
        assert_eq!(config.hooks.post_tool_use[0].r#match, "edit|write");
        assert_eq!(config.hooks.post_tool_use[0].timeout, 15);
        assert!(!config.hooks.post_tool_use[0].blocking);
        assert_eq!(config.hooks.post_tool_use[1].r#match, "*");
        assert_eq!(config.hooks.post_tool_use[1].timeout, 30);
        assert_eq!(config.hooks.stop.len(), 1);
        assert!(config.hooks.stop[0].blocking);

        let lifecycle = config.lifecycle_hooks();
        assert_eq!(
            lifecycle
                .for_point(mush_agent::HookPoint::PostToolUse)
                .len(),
            2
        );
        assert_eq!(lifecycle.for_point(mush_agent::HookPoint::Stop).len(), 1);
        assert!(
            lifecycle
                .for_point(mush_agent::HookPoint::PreToolUse)
                .is_empty()
        );
        assert!(lifecycle.for_point(mush_agent::HookPoint::Stop)[0].blocking);
    }

    #[test]
    fn hooks_default_to_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.hooks.pre_tool_use.is_empty());
        assert!(config.hooks.post_tool_use.is_empty());
        assert!(config.hooks.stop.is_empty());
        assert!(config.hooks.post_compaction.is_empty());
        assert!(config.lifecycle_hooks().is_empty());
    }

    #[test]
    fn repo_map_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.repo_map.enabled);
        assert_eq!(config.repo_map.budget, 8192);
    }

    #[test]
    fn repo_map_custom_budget() {
        let config: Config = toml::from_str(
            r#"
[repo_map]
budget = 32768
"#,
        )
        .unwrap();
        assert!(config.repo_map.enabled);
        assert_eq!(config.repo_map.budget, 32768);
    }

    #[test]
    fn repo_map_disabled() {
        let config: Config = toml::from_str(
            r#"
[repo_map]
enabled = false
"#,
        )
        .unwrap();
        assert!(!config.repo_map.enabled);
    }

    #[test]
    fn parse_post_compaction_hooks() {
        let toml = r#"
[[hooks.post_compaction]]
command = "cat .mush/rules.md"
blocking = false

[[hooks.post_compaction]]
command = "echo critical rules here"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.hooks.post_compaction.len(), 2);
        assert_eq!(
            config.hooks.post_compaction[0].command,
            "cat .mush/rules.md"
        );

        let lifecycle = config.lifecycle_hooks();
        assert_eq!(
            lifecycle
                .for_point(mush_agent::HookPoint::PostCompaction)
                .len(),
            2
        );
        assert!(!lifecycle.is_empty());
    }

    #[test]
    fn parse_lsp_config_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.lsp.diagnostics);
        assert!(config.lsp.servers.is_empty());
    }

    #[test]
    fn parse_lsp_config_with_servers() {
        let toml = r#"
[lsp]
diagnostics = true

[lsp.servers.rust]
command = "rust-analyzer"

[lsp.servers.python]
command = "pyright-langserver"
args = ["--stdio"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.lsp.diagnostics);
        assert_eq!(config.lsp.servers.len(), 2);
        assert_eq!(config.lsp.servers["rust"].command, "rust-analyzer");
        assert_eq!(config.lsp.servers["python"].args, vec!["--stdio"]);
    }

    #[test]
    fn parse_model_tiers() {
        let toml = r#"
[model_tiers]
fast = "claude-haiku-3-5-20241022"
default = "claude-sonnet-4-20250514"
strong = "claude-opus-4-7"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model_tiers.len(), 3);
        assert_eq!(config.model_tiers["fast"], "claude-haiku-3-5-20241022");
        assert_eq!(config.model_tiers["strong"], "claude-opus-4-7");
    }

    #[test]
    fn model_tiers_default_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.model_tiers.is_empty());
    }

    #[test]
    fn compaction_model_defaults_to_none() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.compaction_model.is_none());
    }

    #[test]
    fn compaction_model_can_be_set() {
        let config: Config = toml::from_str(r#"compaction_model = "openai/gpt-5-nano""#).unwrap();
        assert_eq!(
            config.compaction_model.as_deref(),
            Some("openai/gpt-5-nano")
        );
    }

    #[test]
    fn favourite_models_default_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.favourite_models.is_empty());
    }

    #[test]
    fn favourite_models_parses_from_config() {
        let config: Config =
            toml::from_str(r#"favourite_models = ["claude-opus-4-7", "openai/gpt-5"]"#).unwrap();
        assert_eq!(
            config.favourite_models,
            vec!["claude-opus-4-7".to_string(), "openai/gpt-5".to_string()]
        );
    }

    #[test]
    fn favourite_models_round_trip_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("favourite-models.json");
        let favs = vec!["a".to_string(), "b".to_string()];
        save_favourite_models_to(&path, &favs);
        let loaded = load_favourite_models_from(&path);
        assert_eq!(loaded, favs);
    }

    #[test]
    fn load_favourite_models_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("favourite-models.json");
        let loaded = load_favourite_models_from(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn recent_models_round_trip_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("recent-models.json");
        let recent = vec!["claude-opus-4-7".to_string(), "gpt-5".to_string()];
        save_recent_models_to(&path, &recent);
        assert_eq!(load_recent_models_from(&path), recent);
    }

    #[test]
    fn load_recent_models_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("recent-models.json");
        assert!(load_recent_models_from(&path).is_empty());
    }

    #[test]
    fn resolve_favourites_prefers_config_over_disk() {
        let from_config = vec!["declared".to_string()];
        let from_disk = vec!["imperative".to_string()];
        let (effective, locked) = resolve_favourites(&from_config, from_disk.clone());
        assert_eq!(effective, from_config);
        assert!(locked, "non-empty config should lock imperative edits");
    }

    #[test]
    fn resolve_favourites_falls_back_to_disk_when_config_empty() {
        let from_disk = vec!["imperative".to_string()];
        let (effective, locked) = resolve_favourites(&[], from_disk.clone());
        assert_eq!(effective, from_disk);
        assert!(!locked, "empty config lets imperative edits through");
    }
}
