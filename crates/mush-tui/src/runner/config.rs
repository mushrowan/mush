use mush_ai::types::{Message, Model, StreamOptions, ThinkingLevel};
use mush_session::ConversationState;

/// callback that returns a relevance hint for a user message.
/// used to nudge the model toward the most relevant skills.
/// wrapped in Arc so it can be shared with context transform closures.
pub type PromptEnricher = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// how to inject skill relevance hints into the conversation
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
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

/// callback to update session title
pub type TitleUpdater = std::sync::Arc<dyn Fn(String) + Send + Sync>;

/// callback to persist session state and selected model
pub type SessionSaver = std::sync::Arc<dyn Fn(&ConversationState, &str) + Send + Sync>;

/// configuration for the TUI runner (owned, 'static-friendly)
pub struct TuiConfig {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub options: StreamOptions,
    pub max_turns: usize,
    /// initial conversation history (for session resume)
    pub initial_messages: Vec<Message>,
    /// colour theme
    pub theme: crate::theme::Theme,
    /// optional callback to auto-inject context (e.g. skills) per user message
    pub prompt_enricher: Option<PromptEnricher>,
    /// where to inject skill relevance hints
    pub hint_mode: HintMode,
    /// path to config file for hot-reload
    pub config_path: Option<std::path::PathBuf>,
    /// per-provider api keys from config
    pub provider_api_keys: std::collections::HashMap<String, String>,
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
    /// show dollar cost in status bar (off by default, toggle with /cost)
    pub show_cost: bool,
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
}
