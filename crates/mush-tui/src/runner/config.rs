use mush_ai::types::{Model, StreamOptions};
use mush_session::ConversationState;

pub type FavouriteModelsSaver = std::sync::Arc<dyn Fn(&[String]) + Send + Sync>;

pub type RecentModelsSaver = std::sync::Arc<dyn Fn(&[String]) + Send + Sync>;

pub type LiveToolOutput =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>;

pub use mush_runtime::{PaneSnapshot, SessionSaver, SessionSnapshot};

pub type KernelNotices = std::sync::Arc<std::sync::Mutex<Vec<mush_tools::kernel_jobs::JobNotice>>>;

pub struct KernelBusyRequest {
    pub reply: tokio::sync::oneshot::Sender<mush_tools::ipython::BusyKernelChoice>,
}

pub type KernelPrompts = std::sync::Arc<std::sync::Mutex<Vec<KernelBusyRequest>>>;

#[derive(Debug, Clone, Default)]
pub struct SkillCatalogue {
    pub skills: Vec<mush_ext::Skill>,
    pub diagnostics: Vec<mush_ext::SkillDiagnostic>,
}

pub struct ReloadedContext {
    pub system_prompt: String,
    pub templates: Vec<mush_ext::PromptTemplate>,
    pub skills: SkillCatalogue,
}

pub type ReloadCallback = std::sync::Arc<dyn Fn(&std::path::Path) -> ReloadedContext + Send + Sync>;

pub type ModelResolver = std::sync::Arc<dyn Fn(&str) -> Option<Model> + Send + Sync>;

pub struct TuiConfig {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub options: StreamOptions,
    pub max_turns: usize,
    pub initial_conversation: ConversationState,
    pub initial_title: Option<String>,
    pub initial_panes: Vec<PaneSnapshot>,
    pub theme: crate::theme::Theme,
    pub config_path: Option<std::path::PathBuf>,
    pub provider_api_keys: std::collections::HashMap<String, mush_ai::ApiKey>,
    pub thinking_prefs: super::ThinkingPrefs,
    pub save_thinking_prefs: Option<super::ThinkingPrefsSaver>,
    pub last_models: super::LastModels,
    pub save_last_models: Option<super::LastModelsSaver>,
    pub save_session: Option<SessionSaver>,
    pub generate_title: bool,
    pub confirm_tools: bool,
    pub auto_compact: bool,
    pub auto_fork_compact: bool,
    pub show_cost: bool,
    pub show_usage_lines: bool,
    pub show_token_counters: bool,
    pub status_bar_config: crate::app_state::StatusBarConfig,
    pub debug_cache: bool,
    pub cache_timer: bool,
    pub thinking_display: crate::app::ThinkingDisplay,
    pub tool_output_live: Option<LiveToolOutput>,
    pub log_buffer: Option<std::sync::Arc<dyn Fn(usize) -> Vec<String> + Send + Sync>>,
    pub terminal_policy: crate::terminal_policy::TerminalPolicy,
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    pub cwd: std::path::PathBuf,
    pub dynamic_system_context: Option<mush_agent::DynamicContext>,
    pub file_rules: Option<mush_agent::FileRuleCallback>,
    pub lsp_diagnostics: Option<mush_agent::DiagnosticCallback>,
    pub agent_card: Option<mush_agent::AgentCard>,
    pub control: bool,
    pub model_tiers: std::collections::HashMap<String, String>,
    pub compaction_model: Option<(Model, StreamOptions)>,
    pub resolve_model: Option<ModelResolver>,
    pub http_client: Option<mush_ai::http::HttpClient>,
    pub session_id: mush_ai::types::SessionId,
    pub settings: crate::settings::ScopedSettings,
    pub favourite_models: Vec<String>,
    pub favourites_locked: bool,
    pub save_favourite_models: Option<FavouriteModelsSaver>,
    pub recent_models: Vec<String>,
    pub save_recent_models: Option<RecentModelsSaver>,
    pub keymap: crate::keybinds::KeyMap,
    pub reload_context: Option<ReloadCallback>,
    pub mcp: Option<std::sync::Arc<mush_mcp::McpManager>>,
    pub skills: SkillCatalogue,
    pub tool_allowlist: Option<Vec<String>>,
    pub kernel_notices: Option<KernelNotices>,
    pub kernel_prompts: Option<KernelPrompts>,
    pub kernel_state: Option<std::sync::Arc<dyn mush_kernel::KernelState>>,
    pub rlm_depth: u32,
    pub rlm_max_depth: u32,
    pub forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars,
}

impl TuiConfig {
    #[must_use]
    pub fn stream_options(&self) -> mush_ai::types::StreamOptions {
        let mut options = self.options.clone();
        options.session_id = Some(self.session_id.clone());
        options
    }

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

    #[must_use]
    pub fn injections(&self) -> mush_agent::AgentInjections {
        mush_agent::AgentInjections {
            lifecycle_hooks: self.lifecycle_hooks.clone(),
            cwd: Some(self.cwd.clone()),
            dynamic_system_context: self.dynamic_system_context.clone(),
            file_rules: self.file_rules.clone(),
            lsp_diagnostics: self.lsp_diagnostics.clone(),
            control_endpoint: None,
        }
    }

    #[must_use]
    pub fn turn_config(&self) -> mush_runtime::TurnConfig {
        mush_runtime::TurnConfig {
            default_model: self.model.clone(),
            options: self.stream_options(),
            max_turns: self.max_turns,
            provider_api_keys: self.provider_api_keys.clone(),
            confirm_tools: self.confirm_tools,
            auto_compact: self.auto_compact,
            compaction_model: self.compaction_options(),
            forbidden_chars: self.forbidden_chars.clone(),
            tool_allowlist: self.tool_allowlist.clone(),
            generate_title: self.generate_title,
        }
    }

    pub(crate) fn seed_app(&self, app: &mut crate::app::App) {
        app.thinking_display = self.thinking_display;
        app.theme = self.theme.clone();
        app.keymap = self.keymap.clone();
        app.session_title = self.initial_title.clone();
        app.interaction.show_cost = self.show_cost;
        app.interaction.show_usage_lines = self.show_usage_lines;
        app.interaction.show_token_counters = self.show_token_counters;
        app.interaction.status_bar = self.status_bar_config.clone();
        app.interaction.mouse_scroll_lines = self.terminal_policy.mouse_scroll_lines;
        app.completion.favourite_models = self.favourite_models.clone();
        app.completion.favourites_locked = self.favourites_locked;
        app.completion.recent_models = self.recent_models.clone();
        app.compaction_model_id = self
            .compaction_model
            .as_ref()
            .map(|(model, _)| model.id.clone());
    }
}

#[cfg(test)]
impl TuiConfig {
    pub(crate) fn for_test() -> Self {
        Self {
            model: mush_ai::models::all_models_with_user()
                .into_iter()
                .next()
                .expect("builtin model catalogue is never empty"),
            system_prompt: None,
            options: StreamOptions::default(),
            max_turns: 32,
            initial_conversation: ConversationState::default(),
            initial_title: None,
            initial_panes: Vec::new(),
            theme: crate::theme::Theme::default(),
            config_path: None,
            provider_api_keys: std::collections::HashMap::new(),
            thinking_prefs: super::ThinkingPrefs::default(),
            save_thinking_prefs: None,
            last_models: super::LastModels::default(),
            save_last_models: None,
            save_session: None,
            generate_title: false,
            confirm_tools: false,
            auto_compact: false,
            auto_fork_compact: false,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar_config: crate::app_state::StatusBarConfig::default(),
            debug_cache: false,
            cache_timer: false,
            thinking_display: crate::app::ThinkingDisplay::Collapse,
            tool_output_live: None,
            log_buffer: None,
            terminal_policy: crate::terminal_policy::TerminalPolicy::default(),
            lifecycle_hooks: mush_agent::LifecycleHooks::default(),
            cwd: std::path::PathBuf::from("/tmp"),
            dynamic_system_context: None,
            file_rules: None,
            lsp_diagnostics: None,
            agent_card: None,
            control: false,
            model_tiers: std::collections::HashMap::new(),
            compaction_model: None,
            resolve_model: None,
            http_client: None,
            session_id: mush_ai::types::SessionId::new(),
            settings: crate::settings::ScopedSettings::default(),
            favourite_models: Vec::new(),
            favourites_locked: false,
            save_favourite_models: None,
            recent_models: Vec::new(),
            save_recent_models: None,
            keymap: crate::keybinds::KeyMap::default(),
            reload_context: None,
            mcp: None,
            skills: SkillCatalogue::default(),
            tool_allowlist: None,
            kernel_notices: None,
            kernel_prompts: None,
            kernel_state: None,
            rlm_depth: 0,
            rlm_max_depth: mush_runtime::session::DEFAULT_RLM_MAX_DEPTH,
            forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_notch_reaches_the_app() {
        let mut config = TuiConfig::for_test();
        config.terminal_policy.mouse_scroll_lines = 5;
        let mut app = crate::app::App::new("test".into(), mush_ai::types::TokenCount::new(200_000));

        config.seed_app(&mut app);

        assert_eq!(app.interaction.mouse_scroll_lines, 5);
    }
}
