use mush_ai::types::{Model, StreamOptions};

pub type FavouriteModelsSaver = std::sync::Arc<dyn Fn(&[String]) + Send + Sync>;

pub type RecentModelsSaver = std::sync::Arc<dyn Fn(&[String]) + Send + Sync>;

pub use mush_runtime::{PaneSnapshot, SessionSaver, SessionSnapshot};

#[derive(Debug, Clone, Default)]
pub struct SkillCatalogue {
    pub skills: Vec<mush_ext::Skill>,
    pub diagnostics: Vec<mush_ext::SkillDiagnostic>,
}

pub struct ReloadedContext {
    pub templates: Vec<mush_ext::PromptTemplate>,
    pub skills: SkillCatalogue,
}

pub type ReloadCallback = std::sync::Arc<dyn Fn(&std::path::Path) -> ReloadedContext + Send + Sync>;

pub type ModelResolver = std::sync::Arc<dyn Fn(&str) -> Option<Model> + Send + Sync>;

pub struct TuiConfig {
    pub model: Model,
    pub options: StreamOptions,
    pub initial_title: Option<String>,
    pub theme: crate::theme::Theme,
    pub config_path: Option<std::path::PathBuf>,
    pub provider_api_keys: std::collections::HashMap<String, mush_ai::ApiKey>,
    pub thinking_prefs: super::ThinkingPrefs,
    pub save_thinking_prefs: Option<super::ThinkingPrefsSaver>,
    pub last_models: super::LastModels,
    pub save_last_models: Option<super::LastModelsSaver>,
    pub show_cost: bool,
    pub show_usage_lines: bool,
    pub show_token_counters: bool,
    pub status_bar_config: crate::app_state::StatusBarConfig,
    pub debug_cache: bool,
    pub cache_timer: bool,
    pub thinking_display: crate::app::ThinkingDisplay,
    pub log_buffer: Option<std::sync::Arc<dyn Fn(usize) -> Vec<String> + Send + Sync>>,
    pub terminal_policy: crate::terminal_policy::TerminalPolicy,
    pub cwd: std::path::PathBuf,
    pub agent_card: Option<mush_agent::AgentCard>,
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
    pub open_session_picker: bool,
    pub branch_summary_skip_prompt: bool,
    pub startup_notice: Option<String>,
    pub startup_commands: Vec<mush_runtime::ClientCommand>,
    pub reload_context: Option<ReloadCallback>,
    pub mcp: Option<std::sync::Arc<mush_mcp::McpManager>>,
    pub skills: SkillCatalogue,
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
    pub(crate) fn worker_for_test(&self) -> mush_runtime::WorkerConfig {
        let mut worker = mush_runtime::WorkerConfig::new(mush_runtime::TurnConfig {
            default_model: self.model.clone(),
            options: self.stream_options(),
            max_turns: 32,
            provider_api_keys: self.provider_api_keys.clone(),
            confirm_tools: false,
            auto_compact: false,
            compaction: Default::default(),
            branch_summary: Default::default(),
            auto_refine: Default::default(),
            auto_fork_compact: false,
            compaction_model: self.compaction_options(),
            forbidden_chars: Default::default(),
            tool_allowlist: None,
            generate_title: false,
        });
        worker.session_id = self.session_id.clone();
        worker.injections.cwd = Some(self.cwd.clone());
        worker
    }

    pub(crate) fn for_test() -> Self {
        Self {
            model: mush_ai::models::all_models_with_user()
                .into_iter()
                .next()
                .expect("builtin model catalogue is never empty"),
            options: StreamOptions::default(),
            initial_title: None,
            theme: crate::theme::Theme::default(),
            config_path: None,
            provider_api_keys: std::collections::HashMap::new(),
            thinking_prefs: super::ThinkingPrefs::default(),
            save_thinking_prefs: None,
            last_models: super::LastModels::default(),
            save_last_models: None,
            show_cost: false,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar_config: crate::app_state::StatusBarConfig::default(),
            debug_cache: false,
            cache_timer: false,
            thinking_display: crate::app::ThinkingDisplay::Collapse,
            log_buffer: None,
            terminal_policy: crate::terminal_policy::TerminalPolicy::default(),
            cwd: std::path::PathBuf::from("/tmp"),
            agent_card: None,
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
            open_session_picker: false,
            branch_summary_skip_prompt: false,
            startup_notice: None,
            startup_commands: Vec::new(),
            reload_context: None,
            mcp: None,
            skills: SkillCatalogue::default(),
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
