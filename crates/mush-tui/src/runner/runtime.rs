use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use mush_ai::models;
use mush_ai::types::{Message, ThinkingLevel};
use mush_session::ConversationState;
use notify::RecommendedWatcher;

use crate::app::{self, App, ModelCompletion, SlashCommand};
use crate::pane::{Pane, PaneId, PaneManager};

use super::{ThinkingPrefsSaver, TuiConfig};

const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("help", "show available commands"),
    ("keys", "show keyboard shortcuts"),
    ("new", "save session, start fresh"),
    ("model", "show or switch model"),
    ("sessions", "browse and resume sessions"),
    ("branch", "branch from nth user message"),
    ("tree", "show conversation tree"),
    ("compact", "summarise old messages to free context"),
    ("export", "save conversation as markdown"),
    ("undo", "revert last turn"),
    ("search", "search conversation"),
    ("cost", "show session cost"),
    ("logs", "show recent log entries"),
    ("injection", "toggle prompt injection preview"),
    ("close", "close focused pane"),
    ("broadcast", "send a message to all panes"),
    ("lock", "lock a file for this pane"),
    ("unlock", "release a file lock"),
    ("locks", "list all file locks"),
    ("label", "set pane label"),
    ("panes", "list all panes"),
    ("merge", "merge forked pane's work back"),
    ("quit", "exit mush"),
];

pub(super) struct RunnerServices {
    pub message_bus: crate::messaging::MessageBus,
    pub shared_state: crate::shared_state::SharedState,
    pub file_tracker: crate::file_tracker::FileTracker,
    pub delegation_queue: crate::delegate::DelegationQueue,
}

/// how often the TUI calls into the usage poller (the poller itself
/// has its own internal cache interval for actual HTTP requests)
const USAGE_POLL_TICK: std::time::Duration = std::time::Duration::from_secs(60);

pub(super) struct RunnerRuntime {
    pub cwd: PathBuf,
    pub pane_mgr: PaneManager,
    pub thinking_prefs: HashMap<String, ThinkingLevel>,
    pub thinking_saver: Option<ThinkingPrefsSaver>,
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    pub pending_prompt: Option<String>,
    pub usage_poller: Option<mush_ai::oauth::usage::UsagePoller>,
    last_usage_poll: std::time::Instant,
    _config_watcher: Option<RecommendedWatcher>,
    config_rx: Option<mpsc::Receiver<crate::theme::Theme>>,
}

impl RunnerRuntime {
    pub(super) async fn new(tui_config: &mut TuiConfig) -> (Self, RunnerServices) {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut app = build_initial_app(tui_config, &cwd);
        let conversation = replay_initial_messages(&mut app, &tui_config.initial_messages);
        let mut pane_mgr =
            PaneManager::new(Pane::with_conversation(PaneId::new(1), app, conversation));

        let message_bus = crate::messaging::MessageBus::new();
        let shared_state = crate::shared_state::SharedState::new();
        let file_tracker = crate::file_tracker::FileTracker::new(cwd.clone());
        let initial_inbox = message_bus.register(PaneId::new(1));
        pane_mgr.focused_mut().inbox = Some(initial_inbox);

        // restore additional panes from saved session
        for pane_snapshot in std::mem::take(&mut tui_config.initial_panes) {
            let pane_id = pane_mgr.next_id();
            let mut pane_app = build_initial_app(tui_config, &cwd);
            pane_app.model_id = pane_snapshot.model_id.into();
            let pane_conversation =
                replay_initial_messages(&mut pane_app, &pane_snapshot.conversation.context());
            let mut pane = Pane::with_conversation(pane_id, pane_app, pane_conversation);
            pane.label = pane_snapshot.label;
            pane.inbox = Some(message_bus.register(pane_id));
            pane_mgr.add_pane(pane);
        }

        if matches!(
            tui_config.isolation_mode,
            crate::file_tracker::IsolationMode::Worktree
        ) {
            let cleaned = crate::isolation::cleanup_stale_worktrees(&cwd).await;
            if cleaned > 0 {
                pane_mgr.focused_mut().app.status =
                    Some(format!("cleaned {cleaned} stale worktree(s)"));
            }
        }

        let (_config_watcher, config_rx) = watch_config(tui_config.config_path.as_ref());

        // create usage poller if anthropic oauth credentials exist
        let usage_poller = tui_config.http_client.as_ref().and_then(|client| {
            mush_ai::oauth::load_credentials()
                .ok()
                .filter(|store| store.providers.contains_key("anthropic"))
                .map(|_| mush_ai::oauth::usage::UsagePoller::new(client.clone()))
        });
        tracing::debug!(has_poller = usage_poller.is_some(), "usage poller setup");

        (
            Self {
                cwd,
                pane_mgr,
                thinking_prefs: std::mem::take(&mut tui_config.thinking_prefs),
                thinking_saver: tui_config.save_thinking_prefs.clone(),
                lifecycle_hooks: tui_config.lifecycle_hooks.clone(),
                pending_prompt: None,
                usage_poller,
                last_usage_poll: std::time::Instant::now() - USAGE_POLL_TICK,
                _config_watcher,
                config_rx,
            },
            RunnerServices {
                message_bus,
                shared_state,
                file_tracker,
                delegation_queue: crate::delegate::new_queue(),
            },
        )
    }

    pub(super) fn apply_config_reload(&mut self, tui_config: &mut TuiConfig) {
        if let Some(ref rx) = self.config_rx
            && let Ok(new_theme) = rx.try_recv()
        {
            tui_config.theme = new_theme;
            for pane in self.pane_mgr.panes_mut() {
                pane.app.theme = tui_config.theme.clone();
            }
            self.pane_mgr.focused_mut().app.status = Some("config reloaded".into());
        }
    }

    pub(super) fn focused_should_quit(&self) -> bool {
        self.pane_mgr.focused().app.should_quit
    }

    pub(super) fn tick_streaming_panes(&mut self) {
        for pane in self.pane_mgr.panes_mut() {
            if pane.app.stream.active {
                pane.app.tick();
            }
        }
    }

    pub(super) fn notify_cache_state(&mut self, enabled: bool) {
        if !enabled {
            return;
        }

        for pane in self.pane_mgr.panes_mut() {
            if let Some(remaining) = pane.app.cache.remaining_secs() {
                if remaining == 0 && !pane.app.cache.expired_sent {
                    pane.app.cache.expired_sent = true;
                    crate::notify::send_with_sound(
                        "cache expired",
                        "prompt cache has gone cold",
                        Some(crate::notify::Sound::Attention),
                    );
                } else if remaining > 0
                    && remaining <= crate::app::CACHE_WARN_SECS
                    && !pane.app.cache.warn_sent
                {
                    pane.app.cache.warn_sent = true;
                    crate::notify::send_with_sound(
                        "cache expiring soon",
                        &format!("prompt cache expires in {remaining}s"),
                        Some(crate::notify::Sound::Attention),
                    );
                }
            }
        }
    }

    /// poll oauth usage data and distribute to all panes
    pub(super) async fn poll_usage(&mut self) {
        if self.last_usage_poll.elapsed() < USAGE_POLL_TICK {
            return;
        }
        self.last_usage_poll = std::time::Instant::now();
        let Some(ref poller) = self.usage_poller else {
            return;
        };
        if let Some(usage) = poller.get_usage().await {
            for pane in self.pane_mgr.panes_mut() {
                pane.app.oauth_usage = Some(usage.clone());
            }
        }
    }
}

fn watch_config(
    config_path: Option<&PathBuf>,
) -> (
    Option<RecommendedWatcher>,
    Option<mpsc::Receiver<crate::theme::Theme>>,
) {
    if let Some(path) = config_path {
        match crate::config_watcher::watch_config(path.clone()) {
            Some((watcher, rx)) => (Some(watcher), Some(rx)),
            None => (None, None),
        }
    } else {
        (None, None)
    }
}

fn build_initial_app(tui_config: &TuiConfig, cwd: &Path) -> App {
    let mut app = App::new(tui_config.model.id.clone(), tui_config.model.context_window);
    app.thinking_level = tui_config
        .options
        .thinking
        .unwrap_or(ThinkingLevel::Off)
        .normalize_visible();
    app.thinking_display = tui_config.thinking_display;
    app.show_cost = tui_config.show_cost;
    app.theme = tui_config.theme.clone();
    app.cache.ttl_secs = if tui_config.cache_timer {
        app::cache_ttl_secs(
            &tui_config.model.provider,
            tui_config.options.cache_retention.as_ref(),
        )
    } else {
        0
    };

    app.completions = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|(name, _)| format!("/{name}"))
        .collect();
    app.slash_commands = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|(name, description)| SlashCommand {
            name: (*name).to_string(),
            description: (*description).to_string(),
        })
        .collect();

    for template in mush_ext::discover_templates(cwd) {
        app.completions.push(format!("/{}", template.name));
        app.slash_commands.push(SlashCommand {
            name: template.name.clone(),
            description: template.description.clone(),
        });
    }

    for model in models::all_models_with_user() {
        app.completions.push(model.id.to_string());
        app.model_completions.push(ModelCompletion {
            id: model.id.to_string(),
            name: model.name.clone(),
        });
    }

    app
}

fn replay_initial_messages(app: &mut App, initial_messages: &[Message]) -> ConversationState {
    if initial_messages.is_empty() {
        return ConversationState::new();
    }

    crate::conversation_display::rebuild_display(app, initial_messages);
    app.status = Some(format!(
        "resumed session ({} messages)",
        initial_messages.len()
    ));
    ConversationState::from_messages(initial_messages.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{
        Api, AssistantContentPart, AssistantMessage, Provider, StopReason, StreamOptions,
        TextContent, Timestamp, TokenCount, Usage, UserContent, UserMessage,
    };

    fn test_model() -> mush_ai::types::Model {
        models::all_models_with_user()
            .into_iter()
            .next()
            .expect("expected at least one model")
    }

    fn test_config() -> TuiConfig {
        let model = test_model();
        TuiConfig {
            model,
            system_prompt: None,
            options: StreamOptions {
                thinking: Some(ThinkingLevel::Medium),
                ..StreamOptions::default()
            },
            max_turns: 32,
            initial_messages: vec![],
            initial_panes: vec![],
            theme: crate::theme::Theme::default(),
            prompt_enricher: None,
            hint_mode: crate::runner::HintMode::Message,
            config_path: None,
            provider_api_keys: HashMap::new(),
            thinking_prefs: HashMap::new(),
            save_thinking_prefs: None,
            save_last_model: None,
            save_session: None,
            update_title: None,
            confirm_tools: false,
            auto_compact: false,
            auto_fork_compact: false,
            show_cost: true,
            debug_cache: false,
            cache_timer: true,
            thinking_display: crate::app::ThinkingDisplay::Collapse,
            tool_output_live: None,
            log_buffer: None,
            isolation_mode: crate::file_tracker::IsolationMode::None,
            terminal_policy: crate::terminal_policy::TerminalPolicy::default(),
            lifecycle_hooks: mush_agent::LifecycleHooks::default(),
            cwd: std::path::PathBuf::from("/tmp"),
            dynamic_system_context: None,
            file_rules: None,
            lsp_diagnostics: None,
            agent_card: None,
            model_tiers: HashMap::new(),
            compaction_model: None,
            http_client: None,
            session_id: mush_ai::types::SessionId::new(),
        }
    }

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn assistant_message(text: &str) -> Message {
        let model = test_model();
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContentPart::Text(TextContent {
                text: text.into(),
            })],
            model: model.id,
            provider: Provider::Anthropic,
            api: Api::AnthropicMessages,
            usage: Usage {
                input_tokens: TokenCount::new(10),
                output_tokens: TokenCount::new(5),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::ZERO,
            },
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        })
    }

    #[test]
    fn build_initial_app_populates_core_display_state() {
        let config = test_config();
        let app = build_initial_app(&config, Path::new("."));

        assert_eq!(app.thinking_level, ThinkingLevel::Medium);
        assert_eq!(app.thinking_display, crate::app::ThinkingDisplay::Collapse);
        assert!(app.show_cost);
        assert!(app.cache.ttl_secs > 0);
        assert!(app.completions.iter().any(|item| item == "/help"));
        assert!(app.slash_commands.iter().any(|cmd| cmd.name == "help"));
        assert!(
            app.model_completions
                .iter()
                .any(|model| model.id == config.model.id.to_string())
        );
    }

    #[test]
    fn replay_initial_messages_restores_display_and_stats() {
        let config = test_config();
        let mut app = build_initial_app(&config, Path::new("."));
        let initial_messages = vec![user_message("hello"), assistant_message("hi there")];

        let conversation = replay_initial_messages(&mut app, &initial_messages);

        assert_eq!(conversation.context(), initial_messages);
        assert_eq!(app.messages.len(), 2);
        assert!(app.stats.total_tokens > TokenCount::ZERO);
        assert_eq!(app.status.as_deref(), Some("resumed session (2 messages)"));
    }
}
