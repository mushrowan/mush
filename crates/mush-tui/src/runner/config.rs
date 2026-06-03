use mush_ai::types::{Message, Model, StreamOptions};
use mush_session::ConversationState;

/// callback to persist the imperative favourites list. only called when
/// favourites aren't locked by config.toml
pub type FavouriteModelsSaver = std::sync::Arc<dyn Fn(&[String]) + Send + Sync>;

/// callback to persist the most-recently-used model list. takes the
/// whole list (most recent first) so the writer can serialise atomically
pub type RecentModelsSaver = std::sync::Arc<dyn Fn(&[String]) + Send + Sync>;

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
    /// active system prompt at snapshot time. carrying it with the
    /// snapshot lets resumed sessions reuse the exact cached prompt
    /// instead of rebuilding from AGENTS.md (which would mismatch
    /// the provider-side prefix cache)
    pub system_prompt: Option<String>,
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

/// freshly-discovered project context returned by a `/reload` callback.
/// the runner applies these to the live `TuiConfig` and `App`
pub struct ReloadedContext {
    /// rebuilt system prompt incorporating any AGENTS.md edits
    pub system_prompt: String,
    /// re-discovered prompt templates (replaces the existing list)
    pub templates: Vec<mush_ext::PromptTemplate>,
}

/// callback fired by `/reload`. takes the project root and returns the
/// rebuilt context. supplied by the binary (mush-cli) where the
/// discovery primitives live
pub type ReloadCallback = std::sync::Arc<dyn Fn(&std::path::Path) -> ReloadedContext + Send + Sync>;

/// callback to resolve model ids at runtime
pub type ModelResolver = std::sync::Arc<dyn Fn(&str) -> Option<Model> + Send + Sync>;

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
    /// path to config file for hot-reload
    pub config_path: Option<std::path::PathBuf>,
    /// per-provider api keys from config
    pub provider_api_keys: std::collections::HashMap<String, mush_ai::ApiKey>,
    /// per-directory + per-model thinking level prefs (loaded from disk
    /// at startup, mutated as users cycle levels at runtime)
    pub thinking_prefs: super::ThinkingPrefs,
    /// callback to save thinking prefs when they change
    pub save_thinking_prefs: Option<super::ThinkingPrefsSaver>,
    /// per-directory record of the most recently used model. drives
    /// the "resume the same model in this project next time" flow
    pub last_models: super::LastModels,
    /// callback to save last-models map when it changes
    pub save_last_models: Option<super::LastModelsSaver>,
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
    /// callback to resolve model ids for runtime compaction-model changes
    pub resolve_model: Option<ModelResolver>,
    /// shared http client for usage polling and other http calls
    pub http_client: Option<mush_ai::http::HttpClient>,
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
    /// model ids in most-recently-used order (most recent first), loaded
    /// at startup. updated whenever a message is sent to a model so the
    /// favourites picker can sort by recency
    pub recent_models: Vec<String>,
    /// callback to persist the recent-models list after it changes
    pub save_recent_models: Option<RecentModelsSaver>,
    /// resolved keybind map applied to every new App
    pub keymap: crate::keybinds::KeyMap,
    /// callback fired by /reload to refresh AGENTS.md and templates.
    /// `None` means /reload reports "not supported" rather than silently
    /// becoming a no-op
    pub reload_context: Option<ReloadCallback>,
    /// validator applied to file content produced by `write` and `edit`.
    /// rejects content containing any of the configured chars unless
    /// the model passes `allow_forbidden_chars: true` on the call.
    /// empty by default (no enforcement)
    pub forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars,
}

impl TuiConfig {
    /// produce stream options for an outgoing request, overriding
    /// `options.session_id` with the canonical top-level `session_id`.
    ///
    /// `session_id` is duplicated across `TuiConfig.session_id` (the
    /// top-level field updated by /new and /resume) and the inner
    /// `options.session_id` (set once at process startup from
    /// `setup.options`). the inner copy is what becomes the
    /// `x-claude-code-session-id` header on anthropic oauth, which
    /// anthropic uses as a prompt-cache namespace. without this
    /// override every /resume sends the stale process-startup id
    /// instead of the loaded session's id, busting the entire prefix
    /// cache on the first call after resume
    #[must_use]
    pub fn stream_options(&self) -> mush_ai::types::StreamOptions {
        let mut options = self.options.clone();
        options.session_id = Some(self.session_id.clone());
        options
    }

    /// like [`Self::stream_options`] but for `compaction_model`'s
    /// stored options. the compaction call goes to the wire under the
    /// same session id as the active conversation
    #[must_use]
    pub fn compaction_options(
        &self,
    ) -> Option<(mush_ai::types::Model, mush_ai::types::StreamOptions)> {
        self.compaction_model.as_ref().map(|(model, opts)| {
            let mut options = opts.clone();
            options.session_id = Some(self.session_id.clone());
            (model.clone(), options)
        })
    }
}
