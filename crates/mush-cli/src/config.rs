//! configuration loading
//!
//! reads from ~/.config/mush/config.toml (or $MUSH_CONFIG_DIR)

use std::collections::HashMap;
use std::path::PathBuf;

use mush_ai::types::ThinkingLevel;
use serde::{Deserialize, Deserializer};

/// deserialise thinking from bool (legacy) or ThinkingLevel string
fn deserialise_thinking<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<ThinkingLevel>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bool(bool),
        Level(ThinkingLevel),
    }
    Option::<Raw>::deserialize(d).map(|opt| {
        opt.map(|raw| match raw {
            Raw::Bool(true) => ThinkingLevel::High,
            Raw::Bool(false) => ThinkingLevel::Off,
            Raw::Level(l) => l,
        })
    })
}

pub use mush_tui::HintMode;
pub use mush_tui::IsolationMode;
pub use mush_tui::TerminalPolicy;

/// top-level config
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    #[serde(default, deserialize_with = "deserialise_thinking")]
    pub thinking: Option<ThinkingLevel>,
    pub max_tokens: Option<u64>,
    pub max_turns: Option<usize>,
    pub cache_retention: Option<mush_ai::types::CacheRetention>,
    pub debug_cache: bool,
    pub system_prompt: Option<String>,
    pub hint_mode: HintMode,
    /// tracing filter string (e.g. "mush=debug,warn")
    pub log_filter: Option<String>,
    /// how to display thinking text (hidden, collapse, expanded)
    pub thinking_display: mush_tui::ThinkingDisplay,
    /// automatically compact conversation when approaching context limit (off by default)
    #[serde(default)]
    pub auto_compact: bool,
    /// fork the session tree before auto-compacting (preserves uncompacted original)
    #[serde(default)]
    pub auto_fork_compact: bool,
    /// prompt for confirmation before executing tools (off by default)
    pub confirm_tools: bool,
    /// show dollar cost in status bar (off by default, toggle with /cost)
    pub show_cost: bool,
    /// show cache warmth countdown and send desktop notifications (off by default)
    pub cache_timer: bool,
    /// multi-pane file isolation mode: none (detect-and-warn), worktree, jj
    pub isolation: IsolationMode,
    /// terminal behaviour overrides
    pub terminal: TerminalPolicy,
    pub api_keys: ApiKeys,
    pub theme: mush_tui::ThemeConfig,
    /// MCP server configurations keyed by name
    #[serde(default)]
    pub mcp: std::collections::HashMap<String, mush_mcp::McpServerConfig>,
    /// use dynamic meta-tools for MCP instead of loading all schemas (default false)
    #[serde(default)]
    pub dynamic_mcp: bool,
    /// lifecycle hooks
    #[serde(default)]
    pub hooks: HooksConfig,
    /// retrieval / auto-context settings
    #[serde(default)]
    pub retrieval: RetrievalConfig,
    /// LSP integration settings
    #[serde(default)]
    pub lsp: LspConfig,
    /// model tier aliases for delegation and multi-pane
    #[serde(default)]
    pub model_tiers: HashMap<String, String>,
}

/// lifecycle hook config sections
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    pub pre_session: Vec<HookEntry>,
    pub pre_tool_use: Vec<HookEntry>,
    pub post_tool_use: Vec<HookEntry>,
    pub stop: Vec<HookEntry>,
    pub post_compaction: Vec<HookEntry>,
}

/// a single hook entry in config
#[derive(Debug, Deserialize)]
pub struct HookEntry {
    /// tool name pattern: "*" for all, "edit|write" for specific tools
    #[serde(default = "default_match")]
    pub r#match: String,
    /// shell command to run
    pub command: String,
    /// timeout in seconds (default 30)
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// whether failure blocks the operation (default false)
    #[serde(default)]
    pub blocking: bool,
}

fn default_match() -> String {
    "*".into()
}

fn default_timeout() -> u64 {
    30
}

/// which local embedding model to use for semantic search
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingModel {
    /// nomic CodeRankEmbed-137M, code-specialised, fast
    #[default]
    Coderank,
    /// google EmbeddingGemma-300M, general purpose, larger
    Gemma,
}

/// LSP integration configuration
#[derive(Debug, Default, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct LspConfig {
    /// enable auto-injecting diagnostics after file-modifying tools (default false)
    pub diagnostics: bool,
    /// custom server overrides, keyed by language name (e.g. "rust", "python")
    #[serde(default)]
    pub servers: HashMap<String, LspServerEntry>,
}

/// a custom LSP server config entry
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct LspServerEntry {
    /// command to run
    pub command: String,
    /// command line arguments
    #[serde(default)]
    pub args: Vec<String>,
}

/// how skills are loaded into context
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillLoading {
    /// all skills eagerly loaded into system prompt
    Eager,
    /// skills loaded on demand via list/describe/load tools
    #[default]
    Lazy,
}

/// retrieval tier configuration
///
/// controls which auto-context sources are active and how much
/// of the context budget they can use.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct RetrievalConfig {
    /// tier 1: tree-sitter repo map in system prompt
    pub repo_map: bool,
    /// tier 2: embedding-based skill matching per user message
    pub embeddings: bool,
    /// total token budget for all retrieval context (default 2048)
    pub context_budget: usize,
    /// how skills are loaded: "lazy" (default) or "eager"
    pub skill_loading: SkillLoading,
    /// which embedding model to use: "coderank" (default) or "gemma"
    pub embedding_model: EmbeddingModel,
    /// cosine similarity threshold for auto-loading full skill content (default 0.5)
    #[serde(default = "default_auto_load_threshold")]
    pub auto_load_threshold: f32,
}

fn default_auto_load_threshold() -> f32 {
    0.5
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            repo_map: true,
            embeddings: true,
            context_budget: 2048,
            skill_loading: SkillLoading::default(),
            embedding_model: EmbeddingModel::default(),
            auto_load_threshold: default_auto_load_threshold(),
        }
    }
}

impl Config {
    /// convert config hook entries into the agent's lifecycle hooks
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

        mush_agent::LifecycleHooks {
            pre_session: convert(&self.hooks.pre_session),
            pre_tool_use: convert(&self.hooks.pre_tool_use),
            post_tool_use: convert(&self.hooks.post_tool_use),
            stop: convert(&self.hooks.stop),
            post_compaction: convert(&self.hooks.post_compaction),
        }
    }
}

/// api key overrides from config file
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ApiKeys {
    pub anthropic: Option<String>,
    pub openrouter: Option<String>,
    pub openai: Option<String>,
}

impl ApiKeys {
    /// collect non-None keys into a provider → key map for the TUI
    #[must_use]
    pub fn to_map(&self) -> HashMap<String, String> {
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
        map
    }
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

fn thinking_prefs_path() -> PathBuf {
    mush_session::data_dir().join("thinking.json")
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
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(prefs) {
        let _ = std::fs::write(&path, json);
    }
}

fn last_model_path() -> PathBuf {
    mush_session::data_dir().join("last-model.txt")
}

pub fn load_last_model() -> Option<String> {
    let model = std::fs::read_to_string(last_model_path()).ok()?;
    let model = model.trim();
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

pub fn save_last_model(model_id: &str) {
    let path = last_model_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, model_id);
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
debug_cache = true

[api_keys]
anthropic = "sk-ant-test"
openrouter = "sk-or-test"
openai = "sk-openai-test"
"#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(config.thinking, Some(ThinkingLevel::High));
        assert_eq!(config.max_tokens, Some(8192));
        assert_eq!(
            config.cache_retention,
            Some(mush_ai::types::CacheRetention::Long)
        );
        assert!(config.debug_cache);
        assert_eq!(config.api_keys.anthropic.as_deref(), Some("sk-ant-test"));
        assert_eq!(config.api_keys.openai.as_deref(), Some("sk-openai-test"));
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
    fn parse_isolation_mode() {
        let config: Config = toml::from_str(r#"isolation = "jj""#).unwrap();
        assert_eq!(config.isolation, IsolationMode::Jj);

        let config: Config = toml::from_str(r#"isolation = "worktree""#).unwrap();
        assert_eq!(config.isolation, IsolationMode::Worktree);

        let config: Config = toml::from_str(r#"isolation = "none""#).unwrap();
        assert_eq!(config.isolation, IsolationMode::None);
    }

    #[test]
    fn isolation_defaults_to_none() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.isolation, IsolationMode::None);
    }

    #[test]
    fn parse_terminal_policy() {
        let config: Config = toml::from_str(
            r#"
[terminal]
keyboard_enhancement = "disabled"
mouse_tracking = "disabled"
image_probe = "disabled"
"#,
        )
        .unwrap();
        assert_eq!(
            config.terminal.keyboard_enhancement,
            mush_tui::KeyboardEnhancementMode::Disabled
        );
        assert_eq!(
            config.terminal.mouse_tracking,
            mush_tui::MouseTrackingMode::Disabled
        );
        assert_eq!(
            config.terminal.image_probe,
            mush_tui::ImageProbeMode::Disabled
        );
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
    fn cache_timer_defaults_to_false() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.cache_timer);
    }

    #[test]
    fn cache_timer_can_be_enabled() {
        let config: Config = toml::from_str("cache_timer = true").unwrap();
        assert!(config.cache_timer);
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
        // second hook gets defaults
        assert_eq!(config.hooks.post_tool_use[1].r#match, "*");
        assert_eq!(config.hooks.post_tool_use[1].timeout, 30);
        assert_eq!(config.hooks.stop.len(), 1);
        assert!(config.hooks.stop[0].blocking);

        // conversion to agent types
        let lifecycle = config.lifecycle_hooks();
        assert_eq!(lifecycle.post_tool_use.len(), 2);
        assert_eq!(lifecycle.stop.len(), 1);
        assert!(lifecycle.pre_tool_use.is_empty());
        assert!(lifecycle.stop[0].blocking);
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
    fn retrieval_defaults_all_enabled() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.retrieval.repo_map);
        assert!(config.retrieval.embeddings);
        assert_eq!(config.retrieval.context_budget, 2048);
    }

    #[test]
    fn retrieval_can_disable_tiers() {
        let config: Config = toml::from_str(
            r#"
[retrieval]
repo_map = false
embeddings = false
context_budget = 4096
"#,
        )
        .unwrap();
        assert!(!config.retrieval.repo_map);
        assert!(!config.retrieval.embeddings);
        assert_eq!(config.retrieval.context_budget, 4096);
    }

    #[test]
    fn retrieval_skill_loading_defaults_to_lazy() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.retrieval.skill_loading, SkillLoading::Lazy);
    }

    #[test]
    fn retrieval_skill_loading_eager() {
        let config: Config = toml::from_str(
            r#"
[retrieval]
skill_loading = "eager"
"#,
        )
        .unwrap();
        assert_eq!(config.retrieval.skill_loading, SkillLoading::Eager);
    }

    #[test]
    fn retrieval_embedding_model_defaults_to_coderank() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.retrieval.embedding_model, EmbeddingModel::Coderank);
    }

    #[test]
    fn retrieval_embedding_model_gemma() {
        let config: Config = toml::from_str(
            r#"
[retrieval]
embedding_model = "gemma"
"#,
        )
        .unwrap();
        assert_eq!(config.retrieval.embedding_model, EmbeddingModel::Gemma);
    }

    #[test]
    fn retrieval_auto_load_threshold_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!((config.retrieval.auto_load_threshold - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn retrieval_auto_load_threshold_custom() {
        let config: Config = toml::from_str(
            r#"
[retrieval]
auto_load_threshold = 0.7
"#,
        )
        .unwrap();
        assert!((config.retrieval.auto_load_threshold - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn retrieval_partial_override() {
        let config: Config = toml::from_str(
            r#"
[retrieval]
embeddings = false
"#,
        )
        .unwrap();
        assert!(config.retrieval.repo_map); // default true
        assert!(!config.retrieval.embeddings);
        assert_eq!(config.retrieval.context_budget, 2048); // default
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
        assert_eq!(config.hooks.post_compaction[0].command, "cat .mush/rules.md");

        let lifecycle = config.lifecycle_hooks();
        assert_eq!(lifecycle.post_compaction.len(), 2);
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
strong = "claude-opus-4-6"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.model_tiers.len(), 3);
        assert_eq!(config.model_tiers["fast"], "claude-haiku-3-5-20241022");
        assert_eq!(config.model_tiers["strong"], "claude-opus-4-6");
    }

    #[test]
    fn model_tiers_default_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.model_tiers.is_empty());
    }
}
