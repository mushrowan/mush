//! configuration loading
//!
//! reads from ~/.config/mush/config.toml (or $MUSH_CONFIG_DIR)

use std::collections::HashMap;
use std::path::PathBuf;

use mush_ai::types::ThinkingLevel;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// deserialise thinking from bool (legacy) or ThinkingLevel string
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

/// wire format for the `thinking` field: accepts a bool (legacy) or a
/// `ThinkingLevel` string. the resulting `Option<ThinkingLevel>` in
/// [`Config`] is what the rest of the code sees.
///
/// TODO: this bool-or-enum legacy form generates an `anyOf` in the
/// JSON Schema that nixcfg maps to `types.either bool (enum [...])`.
/// the `either` form is usable but fiddly for nix users; a future
/// refactor can drop the bool form, announce a one-line breaking
/// change (`thinking = true` → `thinking = "high"`), and simplify
/// the schema to just the enum. kept in option A shape for now per
/// the migration plan
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
enum ThinkingConfigWire {
    Bool(bool),
    Level(ThinkingLevel),
}

pub use mush_tui::HintMode;
pub use mush_tui::IsolationMode;
pub use mush_tui::StatusBarConfig;
pub use mush_tui::TerminalPolicy;

/// top-level config
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[schemars(extend("x-nixcfg-name" = "mush", "x-nixcfg-config-format" = "toml"))]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    #[serde(default, deserialize_with = "deserialise_thinking")]
    #[schemars(with = "Option<ThinkingConfigWire>")]
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
    /// render per-message usage lines (off by default, same info is in the status bar)
    pub show_usage_lines: bool,
    /// show the ↑/↓/R/W token counter segment in the status bar (off by default)
    pub show_token_counters: bool,
    /// per-segment visibility toggles for the status bar
    #[serde(default)]
    pub status_bar: StatusBarConfig,
    /// show cache warmth countdown and send desktop notifications (on by default)
    #[serde(default = "default_cache_timer")]
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
    /// models the user has pinned as favourites, shown with a star in the
    /// model picker and cycleable with alt+m. declaring any value here
    /// locks imperative adds/removes (the tui picker rejects ctrl+f with a
    /// toast) so config stays the single source of truth
    #[serde(default)]
    pub favourite_models: Vec<String>,
    /// optional model to use for compaction (defaults to the active model)
    pub compaction_model: Option<String>,
    /// provider-specific settings (anthropic betas, scope, etc.)
    #[serde(default)]
    pub settings: Settings,
    /// lines scrolled per j/k keystroke in scroll mode (default 3)
    #[serde(default = "default_scroll_lines")]
    pub scroll_lines: u16,
    /// configurable keybinds: action → key combination(s)
    #[serde(default)]
    pub keys: mush_tui::KeysConfig,
}

fn default_scroll_lines() -> u16 {
    mush_tui::DEFAULT_SCROLL_LINES
}

fn default_cache_timer() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: None,
            thinking: None,
            max_tokens: None,
            max_turns: None,
            cache_retention: None,
            debug_cache: false,
            system_prompt: None,
            hint_mode: HintMode::default(),
            log_filter: None,
            thinking_display: mush_tui::ThinkingDisplay::default(),
            auto_compact: false,
            auto_fork_compact: false,
            confirm_tools: false,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar: StatusBarConfig::default(),
            cache_timer: default_cache_timer(),
            isolation: IsolationMode::default(),
            terminal: TerminalPolicy::default(),
            api_keys: ApiKeys::default(),
            theme: mush_tui::ThemeConfig::default(),
            mcp: std::collections::HashMap::default(),
            dynamic_mcp: false,
            hooks: HooksConfig::default(),
            retrieval: RetrievalConfig::default(),
            lsp: LspConfig::default(),
            model_tiers: HashMap::default(),
            favourite_models: Vec::new(),
            compaction_model: None,
            settings: Settings::default(),
            scroll_lines: default_scroll_lines(),
            keys: mush_tui::KeysConfig::default(),
        }
    }
}

/// scope for persisting settings changes made during a session
pub type SettingsScope = mush_tui::settings::SettingsScope;

/// session settings with scope control.
/// `scope` determines where runtime changes (via `/settings`) get persisted
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    /// scope for runtime changes made via `/settings`
    pub scope: SettingsScope,
    /// anthropic oauth beta flag toggles
    pub anthropic_betas: mush_ai::types::AnthropicBetas,
}

/// lifecycle hook config sections
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct HooksConfig {
    pub pre_session: Vec<HookEntry>,
    pub pre_tool_use: Vec<HookEntry>,
    pub post_tool_use: Vec<HookEntry>,
    pub stop: Vec<HookEntry>,
    pub post_compaction: Vec<HookEntry>,
}

/// a single hook entry in config
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingModel {
    /// nomic CodeRankEmbed-137M, code-specialised, fast
    #[default]
    Coderank,
    /// google EmbeddingGemma-300M, general purpose, larger
    Gemma,
}

/// LSP integration configuration
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct LspConfig {
    /// enable auto-injecting diagnostics after file-modifying tools (default false)
    pub diagnostics: bool,
    /// custom server overrides, keyed by language name (e.g. "rust", "python")
    #[serde(default)]
    pub servers: HashMap<String, LspServerEntry>,
}

/// a custom LSP server config entry
#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone, PartialEq, Eq)]
pub struct LspServerEntry {
    /// command to run
    pub command: String,
    /// command line arguments
    #[serde(default)]
    pub args: Vec<String>,
}

/// how skills are discovered and injected into context
///
/// each strategy trades off token cost vs accuracy vs latency:
/// - `prepended`: all skills in system prompt
/// - `summaries`: skill catalogue in prompt, `skill` tool on demand
/// - `embedded`: skill catalogue plus embedding hints, `skill` tool on demand
/// - `embed_inject`: auto-inject from embeddings, no tool needed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    /// all skill bodies dumped into the system prompt
    Prepended,
    /// names + descriptions listed, `skill` tool for on-demand reading
    #[default]
    Summaries,
    /// embedding similarity hints + `skill` tool
    Embedded,
    /// embedding auto-injects matched skill content, no tool needed
    EmbedInject,
}

impl ContextStrategy {
    /// whether this strategy requires the embeddings feature
    #[cfg(feature = "embeddings")]
    pub fn needs_embeddings(self) -> bool {
        matches!(self, Self::Embedded | Self::EmbedInject)
    }

    /// whether this strategy registers skill tools (list/describe/load)
    pub fn needs_skill_tools(self) -> bool {
        matches!(self, Self::Summaries | Self::Embedded)
    }
}

/// retrieval tier configuration
///
/// controls which auto-context sources are active and how much
/// of the context budget they can use.
#[derive(Debug, Deserialize, Serialize, JsonSchema, Clone, PartialEq)]
#[serde(default)]
pub struct RetrievalConfig {
    /// tier 1: tree-sitter repo map in system prompt
    pub repo_map: bool,
    /// total token budget for all retrieval context (default 2048)
    pub context_budget: usize,
    /// how skills are discovered and injected
    pub context_strategy: ContextStrategy,
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
            context_budget: 2048,
            context_strategy: ContextStrategy::default(),
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

/// api key overrides from config file.
///
/// named fields cover the common paths used throughout the codebase.
/// the flattened `other` map catches any additional provider name so
/// users can add keys for groq, deepseek, xai, cerebras, mistral,
/// together, deepinfra, etc. without a new named field each time
///
/// all key values are `ApiKey`, whose Debug/Display are redacted so
/// secrets can't accidentally leak into trace output, panic messages,
/// or captured error bodies. access the raw value via `ApiKey::expose`
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct ApiKeys {
    pub anthropic: Option<mush_ai::types::ApiKey>,
    pub openrouter: Option<mush_ai::types::ApiKey>,
    pub openai: Option<mush_ai::types::ApiKey>,
    /// any other provider keyed by short name (groq, deepseek, xai, ...)
    /// surfaces as a freeform submodule in the nix module so values can
    /// be set imperatively from nix in addition to env vars
    #[serde(flatten)]
    pub other: HashMap<String, mush_ai::types::ApiKey>,
}

impl ApiKeys {
    /// collect non-None keys into a provider → key map for the TUI
    ///
    /// returns `ApiKey` values so the caller has to explicitly call
    /// `.expose()` to reach the raw secret, keeping the redaction
    /// guarantee from the config struct through to the auth layer
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
            // named fields take priority over flattened map entries
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        map
    }

    /// look up an api key by provider name (named field first, then `other`)
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

/// load config, returning default if file doesn't exist.
/// also merges in persisted settings from `settings.toml` (global) and
/// `<cwd>/.mush/settings.toml` (repo) so /settings changes round-trip.
pub fn load_config() -> Config {
    let path = config_path();
    let mut config = if !path.exists() {
        Config::default()
    } else {
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<Config>(&content) {
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
    };

    // layer persisted settings overrides (global first, then repo)
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
                "\x1b[33mwarning: failed to parse {}: {e}\x1b[0m",
                path.display()
            );
            None
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
        Ok(content) => serde_json::from_str::<HashMap<String, ThinkingLevel>>(&content)
            .unwrap_or_default()
            .into_iter()
            .map(|(model, level)| (model, level.normalize_visible()))
            .collect(),
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

// favourite models persistence (imperative, opt-in)

fn favourite_models_path() -> PathBuf {
    mush_session::data_dir().join("favourite-models.json")
}

/// load favourites written by `/favourite` runtime toggles. returns `Vec::new`
/// if the file doesn't exist or can't be parsed
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

/// persist favourites after an imperative toggle. callers should gate on
/// the locked flag from [`resolve_favourites`] before calling
pub fn save_favourite_models(models: &[String]) {
    save_favourite_models_to(&favourite_models_path(), models);
}

pub(crate) fn save_favourite_models_to(path: &std::path::Path, models: &[String]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(models) {
        let _ = std::fs::write(path, json);
    }
}

/// pick the effective favourites list and whether imperative edits are locked.
/// config (declarative) takes precedence when non-empty and locks the picker;
/// otherwise the on-disk imperative list is live
#[must_use]
pub fn resolve_favourites(from_config: &[String], from_disk: Vec<String>) -> (Vec<String>, bool) {
    if from_config.is_empty() {
        (from_disk, false)
    } else {
        (from_config.to_vec(), true)
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
        // flattened extra providers picked up via the `other` map
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
        // unknown providers fall through to None
        assert!(config.api_keys.get("unknown-provider").is_none());
        // to_map merges named fields with `other`
        let map = config.api_keys.to_map();
        assert_eq!(map.get("groq").map(|k| k.expose()), Some("sk-groq-test"));
        assert_eq!(
            map.get("anthropic").map(|k| k.expose()),
            Some("sk-ant-test")
        );
    }

    #[test]
    fn api_keys_debug_output_redacts_secrets() {
        // secrets must never leak into Debug output (trace!, dbg!, panic
        // messages, etc). anthropic is a known field; xai-prefixed key
        // goes through the `other` HashMap to cover both code paths
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
    fn settings_parses_scope_and_betas() {
        let toml = r#"
[settings]
scope = "global"

[settings.anthropic_betas]
context_1m = false
redact_thinking = true
advisor = true
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.settings.scope, SettingsScope::Global);
        assert!(!config.settings.anthropic_betas.context_1m);
        assert!(config.settings.anthropic_betas.redact_thinking);
        assert!(config.settings.anthropic_betas.advisor);
        // unspecified fields keep their defaults
        assert!(config.settings.anthropic_betas.effort);
        assert!(config.settings.anthropic_betas.context_management);
        assert!(!config.settings.anthropic_betas.advanced_tool_use);
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
        // `#[serde(default)]` at struct level delegates to Default::default().
        // keep the manual Default impl in sync with the field-level serde default
        // so deserialising `""` matches constructing with `Config::default()`
        let from_empty: Config = toml::from_str("").unwrap();
        let from_default = Config::default();
        assert_eq!(from_empty.cache_timer, from_default.cache_timer);
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
        let json = r#"{"claude-opus-4-7":"high","claude-sonnet-4-20250514":"medium"}"#;
        let prefs: HashMap<String, ThinkingLevel> = serde_json::from_str(json).unwrap();
        assert_eq!(prefs.get("claude-opus-4-7"), Some(&ThinkingLevel::High));
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
    fn retrieval_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.retrieval.repo_map);
        assert_eq!(config.retrieval.context_budget, 2048);
        assert_eq!(
            config.retrieval.context_strategy,
            ContextStrategy::Summaries
        );
        assert_eq!(config.retrieval.embedding_model, EmbeddingModel::Coderank);
        assert!((config.retrieval.auto_load_threshold - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn retrieval_context_strategy_all_variants() {
        for (input, expected) in [
            ("prepended", ContextStrategy::Prepended),
            ("summaries", ContextStrategy::Summaries),
            ("embedded", ContextStrategy::Embedded),
            ("embed_inject", ContextStrategy::EmbedInject),
        ] {
            let toml = format!("[retrieval]\ncontext_strategy = \"{input}\"");
            let config: Config = toml::from_str(&toml).unwrap();
            assert_eq!(
                config.retrieval.context_strategy, expected,
                "failed for {input}"
            );
        }
    }

    #[test]
    #[cfg(feature = "embeddings")]
    fn context_strategy_needs_embeddings() {
        assert!(!ContextStrategy::Prepended.needs_embeddings());
        assert!(!ContextStrategy::Summaries.needs_embeddings());
        assert!(ContextStrategy::Embedded.needs_embeddings());
        assert!(ContextStrategy::EmbedInject.needs_embeddings());
    }

    #[test]
    fn context_strategy_needs_skill_tools() {
        assert!(!ContextStrategy::Prepended.needs_skill_tools());
        assert!(ContextStrategy::Summaries.needs_skill_tools());
        assert!(ContextStrategy::Embedded.needs_skill_tools());
        assert!(!ContextStrategy::EmbedInject.needs_skill_tools());
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
context_strategy = "prepended"
"#,
        )
        .unwrap();
        assert!(config.retrieval.repo_map); // default
        assert_eq!(
            config.retrieval.context_strategy,
            ContextStrategy::Prepended
        );
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
