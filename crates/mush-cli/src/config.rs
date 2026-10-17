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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct KernelConfig {
    pub python: Option<std::path::PathBuf>,
    pub managed: bool,
    pub extra_packages: Vec<String>,
    pub idle_release_minutes: u64,
}

impl KernelConfig {
    #[must_use]
    pub fn idle_release(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_release_minutes.saturating_mul(60))
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub threshold_percent: u8,
    pub below_price_cliff: mush_session::compact::PriceCliffPin,
    pub agent_callable: bool,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        let defaults = mush_session::compact::CompactionSettings::default();
        Self {
            enabled: true,
            reserve_tokens: defaults.reserve_tokens,
            keep_recent_tokens: defaults.keep_recent_tokens,
            threshold_percent: defaults.threshold_percent,
            below_price_cliff: defaults.below_price_cliff,
            agent_callable: true,
        }
    }
}

impl CompactionConfig {
    #[must_use]
    pub fn settings(&self) -> mush_session::compact::CompactionSettings {
        mush_session::compact::CompactionSettings {
            reserve_tokens: self.reserve_tokens,
            keep_recent_tokens: self.keep_recent_tokens,
            threshold_percent: self.threshold_percent,
            below_price_cliff: self.below_price_cliff,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct BranchSummaryConfig {
    pub reserve_tokens: u64,
    pub skip_prompt: bool,
}

impl Default for BranchSummaryConfig {
    fn default() -> Self {
        Self {
            reserve_tokens: mush_session::compact::CompactionSettings::default().reserve_tokens,
            skip_prompt: false,
        }
    }
}

impl BranchSummaryConfig {
    #[must_use]
    pub fn settings(&self) -> mush_session::compact::CompactionSettings {
        mush_session::compact::CompactionSettings {
            reserve_tokens: self.reserve_tokens,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AutoRefineConfig {
    pub enabled: bool,
    pub turn_interval: u32,
    pub compact: bool,
    pub cooldown_ms: u64,
}

impl Default for AutoRefineConfig {
    fn default() -> Self {
        let defaults = mush_session::refine::AutoRefineSettings::default();
        Self {
            enabled: defaults.enabled,
            turn_interval: defaults.turn_interval,
            compact: defaults.compact,
            cooldown_ms: defaults.cooldown_ms,
        }
    }
}

impl AutoRefineConfig {
    #[must_use]
    pub fn settings(&self) -> mush_session::refine::AutoRefineSettings {
        mush_session::refine::AutoRefineSettings {
            enabled: self.enabled,
            turn_interval: self.turn_interval.max(1),
            compact: self.compact,
            cooldown_ms: self.cooldown_ms,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AutonomousConfig {
    pub enabled: bool,
    pub max_continuations: Option<u32>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub gates: Vec<String>,
    pub gate_retries: u32,
    pub gate_timeout_ms: u64,
    pub subagent_keep_alive_ms: u64,
}

impl Default for AutonomousConfig {
    fn default() -> Self {
        let defaults = mush_runtime::AutonomousDefaults::default();
        Self {
            enabled: defaults.enabled,
            max_continuations: defaults.limits.max_continuations,
            max_turns: defaults.limits.max_turns,
            max_tokens: defaults.limits.max_tokens,
            timeout_ms: defaults.limits.timeout_ms,
            gates: defaults.gates.commands,
            gate_retries: defaults.gates.max_retries,
            gate_timeout_ms: defaults.gates.timeout_ms,
            subagent_keep_alive_ms: defaults.subagent_keep_alive_ms,
        }
    }
}

impl AutonomousConfig {
    #[must_use]
    pub fn settings(&self) -> mush_runtime::AutonomousDefaults {
        mush_runtime::AutonomousDefaults {
            enabled: self.enabled,
            limits: mush_runtime::Limits {
                max_continuations: self.max_continuations,
                max_turns: self.max_turns,
                max_tokens: self.max_tokens,
                timeout_ms: self.timeout_ms,
            },
            gates: mush_runtime::Gates {
                commands: self.gates.clone(),
                max_retries: self.gate_retries,
                timeout_ms: self.gate_timeout_ms,
            },
            subagent_keep_alive_ms: self.subagent_keep_alive_ms,
        }
    }
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            python: None,
            managed: true,
            extra_packages: Vec::new(),
            idle_release_minutes: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct SyncConfig {
    pub server: Option<String>,
    pub key_file: Option<PathBuf>,
    pub machine: Option<String>,
    pub kernel_state_max_mb: u64,
    pub kernel_state_max_age_days: u64,
    pub lease_ttl_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            server: None,
            key_file: None,
            machine: None,
            kernel_state_max_mb: 64,
            kernel_state_max_age_days: 14,
            lease_ttl_secs: 90,
        }
    }
}

fn expand_home(path: &std::path::Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_owned(),
    }
}

impl SyncConfig {
    #[must_use]
    pub fn settings(&self) -> Option<mush_sync::Settings> {
        let server = self
            .server
            .as_deref()
            .map(str::trim)
            .filter(|server| !server.is_empty())?;
        Some(mush_sync::Settings {
            server: server.to_owned(),
            key_file: self
                .key_file
                .as_deref()
                .map_or_else(|| config_dir().join("sync.key"), expand_home),
            machine: self.machine.clone().unwrap_or_else(mush_sync::hostname),
            data_dir: mush_session::data_dir(),
            limits: mush_sync::Limits {
                kernel_state: self.kernel_state_max_mb.saturating_mul(1 << 20),
                kernel_state_age: std::time::Duration::from_secs(
                    self.kernel_state_max_age_days.saturating_mul(24 * 60 * 60),
                ),
                ..mush_sync::Limits::default()
            },
            lease_ttl: std::time::Duration::from_secs(self.lease_ttl_secs.max(15)),
            connect_timeout: std::time::Duration::from_secs(3),
        })
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
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub branch_summary: BranchSummaryConfig,
    #[serde(default)]
    pub auto_refine: AutoRefineConfig,
    #[serde(default)]
    pub autonomous: AutonomousConfig,
    #[serde(default)]
    pub auto_fork_compact: bool,
    #[serde(default = "default_rlm")]
    pub rlm: bool,
    pub rlm_max_depth: Option<u32>,
    #[serde(default)]
    pub kernel: KernelConfig,
    pub confirm_tools: bool,
    #[serde(default = "default_feature_hints")]
    pub feature_hints: bool,
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
    pub subagent_model: Option<String>,
    #[serde(default)]
    pub preferred_providers: Vec<String>,
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
    #[serde(default)]
    pub sync: SyncConfig,
}

fn default_max_image_mb() -> u32 {
    5
}

fn default_cache_timer() -> bool {
    true
}

fn default_feature_hints() -> bool {
    true
}

fn default_rlm() -> bool {
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
            compaction: CompactionConfig::default(),
            branch_summary: BranchSummaryConfig::default(),
            auto_refine: AutoRefineConfig::default(),
            autonomous: AutonomousConfig::default(),
            auto_fork_compact: false,
            rlm: true,
            rlm_max_depth: None,
            kernel: KernelConfig::default(),
            confirm_tools: false,
            feature_hints: default_feature_hints(),
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
            subagent_model: None,
            preferred_providers: Vec::new(),
            settings: Settings::default(),
            keys: mush_tui::KeysConfig::default(),
            forbidden_chars: Vec::new(),
            forbidden_char_classes: Vec::new(),
            max_image_mb: default_max_image_mb(),
            web_search: mush_tools::web_search::WebSearchConfig::default(),
            sync: SyncConfig::default(),
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

pub use mush_ai::dirs::config_dir;

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

#[derive(Debug, thiserror::Error)]
pub enum OverrideError {
    #[error("expected KEY=VALUE, got {0:?}")]
    Shape(String),
    #[error("unknown setting {0:?}")]
    UnknownKey(String),
    #[error("config rejected the override {key:?}: {error}")]
    Invalid { key: String, error: String },
}

fn warn(message: std::fmt::Arguments<'_>) {
    eprintln!(
        "{}",
        Style::Yellow.stderr(format_args!("warning: {message}"))
    );
}

pub fn load_config() -> Config {
    load_config_with(&[]).unwrap_or_else(|error| {
        warn(format_args!("{error}"));
        Config::default()
    })
}

pub fn load_config_with(overrides: &[String]) -> Result<Config, OverrideError> {
    let path = config_path();
    let mut table = if !path.exists() {
        toml::Table::new()
    } else {
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<toml::Table>(&content) {
                Ok(table) => table,
                Err(e) => {
                    warn(format_args!("failed to parse config: {e}"));
                    toml::Table::new()
                }
            },
            Err(e) => {
                warn(format_args!("failed to read config: {e}"));
                toml::Table::new()
            }
        }
    };
    let schema = schemars::schema_for!(Config);
    for spec in overrides {
        apply_override(&mut table, schema.as_value(), spec)?;
    }
    let mut config: Config = match table.try_into() {
        Ok(config) => config,
        Err(e) if overrides.is_empty() => {
            warn(format_args!("failed to parse config: {e}"));
            Config::default()
        }
        Err(e) => {
            return Err(OverrideError::Invalid {
                key: overrides.join(" "),
                error: e.to_string(),
            });
        }
    };

    let global = config_dir().join("settings.toml");
    if let Some(persisted) = try_load_persisted_settings(&global) {
        config.settings.anthropic_betas = persisted.anthropic_betas;
        if persisted.rlm_max_depth.is_some() {
            config.rlm_max_depth = persisted.rlm_max_depth;
        }
        if let Some(pin) = persisted.compact_below_price_cliff {
            config.compaction.below_price_cliff = pin;
        }
        if let Some(providers) = persisted.preferred_providers {
            config.preferred_providers = providers;
        }
        if let Some(model) = persisted.subagent_model {
            config.subagent_model = mush_tui::settings::chosen_subagent_model(&model);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let repo = cwd.join(".mush").join("settings.toml");
        if let Some(persisted) = try_load_persisted_settings(&repo) {
            config.settings.anthropic_betas = persisted.anthropic_betas;
            if let Some(pin) = persisted.compact_below_price_cliff {
                config.compaction.below_price_cliff = pin;
            }
            if let Some(providers) = persisted.preferred_providers {
                config.preferred_providers = providers;
            }
            if let Some(model) = persisted.subagent_model {
                config.subagent_model = mush_tui::settings::chosen_subagent_model(&model);
            }
        }
    }

    Ok(config)
}

fn apply_override(
    table: &mut toml::Table,
    schema: &serde_json::Value,
    spec: &str,
) -> Result<(), OverrideError> {
    let (key, raw) = spec
        .split_once('=')
        .ok_or_else(|| OverrideError::Shape(spec.to_string()))?;
    let key = key.trim();
    let path: Vec<&str> = key.split('.').collect();
    if key.is_empty() || path.iter().any(|segment| segment.is_empty()) {
        return Err(OverrideError::Shape(spec.to_string()));
    }
    if !schema_has_path(schema, schema, &path) {
        return Err(OverrideError::UnknownKey(key.to_string()));
    }
    let value = parse_override_value(raw.trim());
    let (last, parents) = path
        .split_last()
        .ok_or_else(|| OverrideError::Shape(spec.to_string()))?;
    let mut node = table;
    for segment in parents {
        let entry = node
            .entry((*segment).to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        node = entry
            .as_table_mut()
            .ok_or_else(|| OverrideError::Shape(spec.to_string()))?;
    }
    node.insert((*last).to_string(), value);
    Ok(())
}

fn parse_override_value(raw: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut table| table.remove("v"))
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

fn schema_has_path(root: &serde_json::Value, node: &serde_json::Value, path: &[&str]) -> bool {
    let Some((head, rest)) = path.split_first() else {
        return true;
    };
    let node = resolve_ref(root, node);
    if let Some(property) = node.get("properties").and_then(|p| p.get(*head)) {
        return schema_has_path(root, property, rest);
    }
    if let Some(extra) = node.get("additionalProperties")
        && extra.is_object()
    {
        return schema_has_path(root, extra, rest);
    }
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(options) = node.get(combinator).and_then(|v| v.as_array())
            && options
                .iter()
                .any(|option| schema_has_path(root, option, path))
        {
            return true;
        }
    }
    false
}

fn resolve_ref<'a>(
    root: &'a serde_json::Value,
    node: &'a serde_json::Value,
) -> &'a serde_json::Value {
    let Some(reference) = node.get("$ref").and_then(|r| r.as_str()) else {
        return node;
    };
    let Some(pointer) = reference.strip_prefix('#') else {
        return node;
    };
    root.pointer(pointer).unwrap_or(node)
}

fn try_load_persisted_settings(
    path: &std::path::Path,
) -> Option<mush_tui::settings::PersistedSettings> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(p) => Some(p),
        Err(e) => {
            warn(format_args!("failed to parse {}: {e}", path.display()));
            None
        }
    }
}

fn last_model_path() -> PathBuf {
    mush_session::data_dir().join("last-model.json")
}

fn load_json<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_json(path: &std::path::Path, value: &impl Serialize) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(value) {
        let _ = mush_ai::private_io::write_atomic(path, json);
    }
}

pub fn load_last_models() -> mush_tui::LastModels {
    let mut last: mush_tui::LastModels = load_json(&last_model_path());
    last.prune_missing_dirs();
    last
}

pub fn save_last_models(last: &mush_tui::LastModels) {
    save_json(&last_model_path(), last);
}

pub(crate) fn load_model_list(path: &std::path::Path) -> Vec<String> {
    load_json(path)
}

pub(crate) fn save_model_list(path: &std::path::Path, models: &[String]) {
    save_json(path, &models);
}

fn favourite_models_path() -> PathBuf {
    mush_session::data_dir().join("favourite-models.json")
}

pub fn load_favourite_models() -> Vec<String> {
    load_model_list(&favourite_models_path())
}

pub fn save_favourite_models(models: &[String]) {
    save_model_list(&favourite_models_path(), models);
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
    load_model_list(&recent_models_path())
}

pub fn save_recent_models(models: &[String]) {
    save_model_list(&recent_models_path(), models);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overridden(specs: &[&str]) -> Result<Config, OverrideError> {
        let schema = schemars::schema_for!(Config);
        let mut table = toml::Table::new();
        for spec in specs {
            apply_override(&mut table, schema.as_value(), spec)?;
        }
        table
            .try_into()
            .map_err(|e: toml::de::Error| OverrideError::Invalid {
                key: specs.join(" "),
                error: e.to_string(),
            })
    }

    #[test]
    fn an_override_sets_a_flat_bool_and_a_nested_key() {
        let config =
            overridden(&["confirm_tools=true", "kernel.managed=false", "max_turns=7"]).unwrap();

        assert!(config.confirm_tools);
        assert!(!config.kernel.managed);
        assert_eq!(config.max_turns, Some(7));
    }

    #[test]
    fn an_idle_subagent_keeps_its_kernel_for_thirty_minutes_unless_configured() {
        assert_eq!(
            Config::default().kernel.idle_release(),
            std::time::Duration::from_secs(30 * 60)
        );
        let config = overridden(&["kernel.idle_release_minutes=0"]).unwrap();
        assert_eq!(config.kernel.idle_release(), std::time::Duration::ZERO);
    }

    #[test]
    fn a_bare_word_is_a_string_and_a_list_is_a_list() {
        let config = overridden(&[
            "model=claude-opus-4-7",
            "kernel.extra_packages=[\"polars\", \"rich\"]",
        ])
        .unwrap();

        assert_eq!(config.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(config.kernel.extra_packages, vec!["polars", "rich"]);
    }

    #[test]
    fn an_unknown_key_and_a_bad_shape_are_rejected() {
        assert!(matches!(
            overridden(&["confrim_tools=true"]),
            Err(OverrideError::UnknownKey(key)) if key == "confrim_tools"
        ));
        assert!(matches!(
            overridden(&["kernel.nope=1"]),
            Err(OverrideError::UnknownKey(key)) if key == "kernel.nope"
        ));
        assert!(matches!(
            overridden(&["confirm_tools"]),
            Err(OverrideError::Shape(_))
        ));
    }

    #[test]
    fn a_wrong_type_is_rejected_by_the_config() {
        assert!(matches!(
            overridden(&["confirm_tools=maybe"]),
            Err(OverrideError::Invalid { .. })
        ));
    }

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
custom = "sk-custom-test"
local = "sk-local-test"
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
                .get("custom")
                .map(mush_ai::types::ApiKey::expose),
            Some("sk-custom-test")
        );
        assert_eq!(
            config
                .api_keys
                .get("local")
                .map(mush_ai::types::ApiKey::expose),
            Some("sk-local-test")
        );
        assert!(config.api_keys.get("unknown-provider").is_none());
        let map = config.api_keys.to_map();
        assert_eq!(
            map.get("custom").map(|k| k.expose()),
            Some("sk-custom-test")
        );
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
local = "sk-local-ALSO-SECRET-xyz789"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let debug = format!("{:?}", config.api_keys);
        assert!(
            !debug.contains("VERY-SECRET-DO-NOT-LEAK-abc123"),
            "anthropic secret leaked in Debug: {debug}"
        );
        assert!(
            !debug.contains("ALSO-SECRET-xyz789"),
            "local secret leaked in Debug: {debug}"
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
    fn an_empty_config_takes_the_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.model.is_none());
        assert_eq!(config.terminal, TerminalPolicy::default());
        assert_eq!(config.thinking, None);
        assert!(!config.auto_fork_compact);
        assert!(config.cache_timer);
        assert_eq!(config.cache_timer, Config::default().cache_timer);
        assert!(config.repo_map.enabled);
        assert_eq!(config.repo_map.budget, 8192);
        assert!(!config.lsp.diagnostics);
        assert!(config.lsp.servers.is_empty());
        assert!(config.model_tiers.is_empty());
        assert!(config.compaction_model.is_none());
        assert!(config.favourite_models.is_empty());
        assert!(config.forbidden_chars.is_empty());
        assert!(config.forbidden_char_classes.is_empty());
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
    fn compaction_and_refine_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.compaction.enabled);
        assert_eq!(config.compaction.reserve_tokens, 16_384);
        assert_eq!(config.compaction.keep_recent_tokens, 20_000);
        assert!(config.compaction.agent_callable);
        assert_eq!(config.branch_summary.reserve_tokens, 16_384);
        assert!(!config.branch_summary.skip_prompt);
        assert!(config.auto_refine.enabled);
        assert_eq!(config.auto_refine.turn_interval, 25);
        assert!(config.auto_refine.compact);
        assert_eq!(config.auto_refine.cooldown_ms, 1_200_000);
    }

    #[test]
    fn auto_refine_table_overrides_and_clamps_the_interval() {
        let config: Config = toml::from_str(
            "[auto_refine]\nenabled = false\nturn_interval = 0\ncompact = false\ncooldown_ms = 5\n",
        )
        .unwrap();
        let settings = config.auto_refine.settings();
        assert!(!settings.enabled);
        assert_eq!(settings.turn_interval, 1);
        assert!(!settings.compact);
        assert_eq!(settings.cooldown_ms, 5);
    }

    #[test]
    fn compaction_table_overrides_each_knob() {
        let config: Config = toml::from_str(
            "[compaction]\nenabled = false\nreserve_tokens = 8192\nkeep_recent_tokens = 5000\nagent_callable = false\n\n[branch_summary]\nreserve_tokens = 4096\nskip_prompt = true\n",
        )
        .unwrap();
        assert!(!config.compaction.enabled);
        assert_eq!(config.compaction.settings().reserve_tokens, 8_192);
        assert_eq!(config.compaction.settings().keep_recent_tokens, 5_000);
        assert!(!config.compaction.agent_callable);
        assert_eq!(config.branch_summary.settings().reserve_tokens, 4_096);
        assert!(config.branch_summary.skip_prompt);
    }

    #[test]
    fn auto_fork_compact_can_be_enabled() {
        let config: Config = toml::from_str("auto_fork_compact = true").unwrap();
        assert!(config.auto_fork_compact);
    }

    #[test]
    fn cache_timer_can_be_disabled() {
        let config: Config = toml::from_str("cache_timer = false").unwrap();
        assert!(!config.cache_timer);
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
    fn autonomous_defaults_match_the_runtime_and_the_table_overrides_them() {
        let config: Config = toml::from_str("").unwrap();
        let defaults = config.autonomous.settings();
        assert_eq!(defaults, mush_runtime::AutonomousDefaults::default());
        assert!(!defaults.enabled);

        let config: Config = toml::from_str(
            "[autonomous]\nenabled = true\nmax_turns = 4\ngates = [\"cargo test\"]\ngate_retries = 1\nsubagent_keep_alive_ms = 0\n",
        )
        .unwrap();
        let settings = config.autonomous.settings();
        assert!(settings.enabled);
        assert_eq!(settings.limits.max_turns, Some(4));
        assert_eq!(settings.gates.commands, vec!["cargo test".to_string()]);
        assert_eq!(settings.gates.max_retries, 1);
        assert_eq!(settings.subagent_keep_alive_ms, 0);
    }

    #[test]
    fn subagent_model_defaults_to_none_and_takes_an_id() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.subagent_model.is_none());
        let config: Config = toml::from_str(r#"subagent_model = "claude-haiku-4-5""#).unwrap();
        assert_eq!(config.subagent_model.as_deref(), Some("claude-haiku-4-5"));
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
        save_model_list(&path, &favs);
        let loaded = load_model_list(&path);
        assert_eq!(loaded, favs);
    }

    #[test]
    fn load_favourite_models_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("favourite-models.json");
        let loaded = load_model_list(&path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn recent_models_round_trip_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("recent-models.json");
        let recent = vec!["claude-opus-4-7".to_string(), "gpt-5".to_string()];
        save_model_list(&path, &recent);
        assert_eq!(load_model_list(&path), recent);
    }

    #[test]
    fn load_recent_models_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("recent-models.json");
        assert!(load_model_list(&path).is_empty());
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
