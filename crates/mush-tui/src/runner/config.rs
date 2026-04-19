use mush_ai::types::{Message, Model, StreamOptions, ThinkingLevel};
use mush_session::ConversationState;

/// callback that returns a relevance hint for a user message.
/// used to nudge the model toward the most relevant skills.
/// wrapped in Arc so it can be shared with context transform closures.
pub type PromptEnricher = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// how to inject skill relevance hints into the conversation
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
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

/// callback to persist per-model thinking level
pub type ThinkingPrefsSaver =
    std::sync::Arc<dyn Fn(&std::collections::HashMap<String, ThinkingLevel>) + Send + Sync>;

/// callback to persist last selected model id
pub type LastModelSaver = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// callback to persist the imperative favourites list. only called when
/// favourites aren't locked by config.toml
pub type FavouriteModelsSaver = std::sync::Arc<dyn Fn(&[String]) + Send + Sync>;

/// callback to update session title
pub type TitleUpdater = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// snapshot of all pane conversations for session persistence
pub struct SessionSnapshot {
    /// session id for this snapshot
    pub session_id: mush_ai::types::SessionId,
    /// primary pane conversation
    pub primary: ConversationState,
    /// primary pane model id
    pub model_id: String,
    /// additional panes (empty for single-pane sessions)
    pub panes: Vec<PaneSnapshot>,
}

/// snapshot of a single additional pane
pub struct PaneSnapshot {
    pub pane_id: mush_ai::types::PaneId,
    pub label: Option<String>,
    pub model_id: String,
    pub conversation: ConversationState,
}

/// callback to persist session state (all panes)
pub type SessionSaver = std::sync::Arc<dyn Fn(SessionSnapshot) + Send + Sync>;

/// configuration for the TUI runner (owned, 'static-friendly)
pub struct TuiConfig {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub options: StreamOptions,
    pub max_turns: usize,
    /// initial conversation history (for session resume)
    pub initial_messages: Vec<Message>,
    /// additional panes to restore (for multi-pane session resume)
    pub initial_panes: Vec<PaneSnapshot>,
    /// colour theme
    pub theme: crate::theme::Theme,
    /// optional callback to auto-inject context (e.g. skills) per user message
    pub prompt_enricher: Option<PromptEnricher>,
    /// where to inject skill relevance hints
    pub hint_mode: HintMode,
    /// path to config file for hot-reload
    pub config_path: Option<std::path::PathBuf>,
    /// per-provider api keys from config
    pub provider_api_keys: std::collections::HashMap<String, mush_ai::ApiKey>,
    /// per-model thinking level prefs (loaded from disk at startup)
    pub thinking_prefs: std::collections::HashMap<String, ThinkingLevel>,
    /// callback to save thinking prefs when they change
    pub save_thinking_prefs: Option<ThinkingPrefsSaver>,
    /// callback to persist last selected model id
    pub save_last_model: Option<LastModelSaver>,
    /// callback to auto-save session after each agent turn
    pub save_session: Option<SessionSaver>,
    /// callback to update session title (called with LLM-generated title)
    pub update_title: Option<TitleUpdater>,
    /// prompt for confirmation before executing tools (off by default)
    pub confirm_tools: bool,
    /// automatically compact conversation when approaching context limit (off by default)
    pub auto_compact: bool,
    /// fork the session tree before auto-compacting (preserves uncompacted original)
    pub auto_fork_compact: bool,
    /// show dollar cost in status bar (off by default, toggle with /cost)
    pub show_cost: bool,
    /// render per-message usage lines (off by default, same info is in status bar)
    pub show_usage_lines: bool,
    /// show ↑/↓/R/W token counter segment in status bar (off by default)
    pub show_token_counters: bool,
    /// per-segment visibility toggles for the status bar
    pub status_bar_config: crate::app_state::StatusBarConfig,
    /// emit system messages when cache reads are observed
    pub debug_cache: bool,
    /// show cache warmth countdown in status bar and send desktop notifications
    pub cache_timer: bool,
    /// how to display thinking text (hidden, collapse, expanded)
    pub thinking_display: crate::app::ThinkingDisplay,
    /// shared live tool output (updated by bash sink, read by TUI)
    pub tool_output_live: Option<std::sync::Arc<std::sync::Mutex<Option<String>>>>,
    /// callback to get recent log entries (returns last N lines)
    pub log_buffer: Option<std::sync::Arc<dyn Fn(usize) -> Vec<String> + Send + Sync>>,
    /// multi-pane file isolation mode
    pub isolation_mode: crate::file_tracker::IsolationMode,
    /// terminal setup policy and overrides
    pub terminal_policy: crate::terminal_policy::TerminalPolicy,
    /// user-configured lifecycle hooks
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    /// working directory for hook commands
    pub cwd: std::path::PathBuf,
    /// dynamic system prompt context (e.g. live repo map updates)
    pub dynamic_system_context: Option<mush_agent::DynamicContext>,
    pub file_rules: Option<mush_agent::FileRuleCallback>,
    /// LSP diagnostic injection after file-modifying tools
    pub lsp_diagnostics: Option<mush_agent::DiagnosticCallback>,
    /// pre-built agent card for /card command and IPC
    pub agent_card: Option<mush_agent::AgentCard>,
    /// model tier aliases: "fast" → "claude-3-5-haiku-...", etc.
    pub model_tiers: std::collections::HashMap<String, String>,
    /// separate model + options for compaction (None = use active model)
    pub compaction_model: Option<(Model, StreamOptions)>,
    /// shared http client for usage polling and other http calls
    pub http_client: Option<reqwest::Client>,
    /// current session id (updated by /new)
    pub session_id: mush_ai::types::SessionId,
    /// active runtime settings (scope + anthropic betas)
    pub settings: crate::settings::ScopedSettings,
    /// lines scrolled per j/k keystroke in scroll mode
    pub scroll_lines: u16,
    /// effective favourite models list (config + disk resolved at startup).
    /// cycled through with alt+m / alt+shift+m, ★-marked in the picker
    pub favourite_models: Vec<String>,
    /// whether the favourites list is pinned by config.toml. when true the
    /// picker's ctrl+f toggle is rejected with a toast
    pub favourites_locked: bool,
    /// callback to persist favourites after an imperative toggle
    pub save_favourite_models: Option<FavouriteModelsSaver>,
    /// resolved keybind map applied to every new App
    pub keymap: crate::keybinds::KeyMap,
}
