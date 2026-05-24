use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use super::{ThinkingPrefs, ThinkingPrefsSaver, TuiConfig};
use mush_ai::types::{Message, ThinkingLevel};
use mush_session::ConversationState;
use notify::RecommendedWatcher;

use crate::app::App;
use crate::pane::{Pane, PaneId, PaneManager};
use crate::slash::PendingCompaction;
use crate::slash_menu::SlashCommand;

const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("help", "show available commands"),
    ("keys", "show keyboard shortcuts"),
    ("new", "save session, start fresh"),
    ("model", "show or switch model"),
    ("sessions", "browse and resume sessions"),
    ("resume", "resume last session in cwd (or by id)"),
    ("branch", "branch from nth user message"),
    ("tree", "show conversation tree"),
    ("compact", "summarise old messages to free context"),
    ("compaction-model", "view or set the compaction model"),
    ("export", "save conversation as markdown"),
    ("undo", "revert last turn"),
    ("search", "search conversation"),
    ("cost", "show session cost"),
    ("credits", "show openrouter credit balance"),
    ("endpoints", "show openrouter providers serving a model"),
    ("logs", "show recent log entries"),
    ("injection", "toggle prompt injection preview"),
    (
        "settings",
        "view or modify runtime settings (anthropic betas, scope)",
    ),
    ("login", "open the login picker (oauth + api keys)"),
    (
        "login-complete",
        "finish oauth login with the authorization code",
    ),
    ("close", "close focused pane"),
    ("broadcast", "send a message to all panes"),
    ("lock", "lock a file for this pane"),
    ("unlock", "release a file lock"),
    ("locks", "list all file locks"),
    ("label", "set pane label"),
    ("panes", "list all panes"),
    ("merge", "merge forked pane's work back"),
    ("debug", "dump internal state (for bug reports)"),
    ("reload", "rebuild AGENTS.md, prompt templates, and skills"),
    ("refresh-models", "refresh discovered model catalogue"),
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

pub enum DiscoveryUpdate {
    Refreshed(String),
}

pub(super) struct RunnerRuntime {
    pub cwd: PathBuf,
    pub pane_mgr: PaneManager,
    pub thinking_prefs: ThinkingPrefs,
    pub thinking_saver: Option<ThinkingPrefsSaver>,
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    pub pending_prompt: Option<String>,
    /// background compaction tasks keyed by pane. spawned by /compact
    /// and /fork-compact so the LLM call doesn't block the input loop;
    /// `poll_pending_compactions` finalises them on subsequent
    /// iterations. one per pane (panes are independent contexts).
    pub pending_compactions: HashMap<PaneId, PendingCompaction>,
    usage_rx: Option<tokio::sync::mpsc::UnboundedReceiver<mush_ai::oauth::usage::OAuthUsage>>,
    _config_watcher: Option<RecommendedWatcher>,
    config_rx: Option<mpsc::Receiver<crate::theme::Theme>>,
    discovery_rx: Option<mpsc::Receiver<DiscoveryUpdate>>,
}

impl RunnerRuntime {
    #[cfg(test)]
    fn from_pane(pane: Pane, thinking_prefs: ThinkingPrefs) -> Self {
        Self {
            cwd: PathBuf::new(),
            pane_mgr: PaneManager::new(pane),
            thinking_prefs,
            thinking_saver: None,
            lifecycle_hooks: mush_agent::LifecycleHooks::default(),
            pending_prompt: None,
            pending_compactions: HashMap::new(),
            usage_rx: None,
            _config_watcher: None,
            config_rx: None,
            discovery_rx: None,
        }
    }

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
        let usage_rx = usage_poller.map(spawn_usage_poller);
        tracing::debug!(has_poller = usage_rx.is_some(), "usage poller setup");

        (
            Self {
                cwd,
                pane_mgr,
                thinking_prefs: std::mem::take(&mut tui_config.thinking_prefs),
                thinking_saver: tui_config.save_thinking_prefs.clone(),
                lifecycle_hooks: tui_config.lifecycle_hooks.clone(),
                pending_prompt: None,
                pending_compactions: HashMap::new(),
                usage_rx,
                _config_watcher,
                config_rx,
                discovery_rx: None,
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

    pub(super) fn install_discovery_updates(&mut self, rx: mpsc::Receiver<DiscoveryUpdate>) {
        self.discovery_rx = Some(rx);
    }

    pub(super) fn apply_discovery_updates(&mut self) -> bool {
        let Some(rx) = self.discovery_rx.as_ref() else {
            return false;
        };

        let mut changed = false;
        while let Ok(update) = rx.try_recv() {
            match update {
                DiscoveryUpdate::Refreshed(line) => {
                    for pane in self.pane_mgr.panes_mut() {
                        crate::input::reload_model_completions(&mut pane.app);
                    }
                    self.pane_mgr
                        .focused_mut()
                        .app
                        .push_system_message(format!("model discovery: {line}"));
                    changed = true;
                }
            }
        }
        changed
    }

    pub(super) fn focused_should_quit(&self) -> bool {
        self.pane_mgr.focused().app.should_quit
    }

    pub(super) fn tick_animation_panes(&mut self) -> bool {
        let mut ticked = false;
        for pane in self.pane_mgr.panes_mut() {
            if pane.app.has_visible_animation() {
                pane.app.tick_animation();
                ticked = true;
            }
        }
        ticked
    }

    pub(super) fn tick_streaming_text_panes(&mut self) {
        for pane in self.pane_mgr.panes_mut() {
            if pane.app.stream.active {
                pane.app.tick_streaming_text();
            }
        }
    }

    pub(super) fn needs_periodic_redraw(&self, cache_timer_enabled: bool) -> bool {
        self.pane_mgr
            .panes()
            .iter()
            .any(|pane| pane.app.needs_periodic_redraw(cache_timer_enabled))
    }

    pub(super) fn active_tool_labels(&self) -> Vec<String> {
        self.pane_mgr
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.app.active_tools.iter().map(|tool| {
                    if tool.summary.is_empty() {
                        tool.name.clone()
                    } else {
                        format!("{}: {}", tool.name, tool.summary)
                    }
                })
            })
            .collect()
    }

    /// finalise any background compaction tasks whose LLM call has
    /// returned. called once per main-loop iteration; returns `true` if
    /// at least one task completed (so the caller can force a redraw).
    /// tasks for panes that were closed during the compaction are
    /// silently dropped.
    pub(super) async fn poll_pending_compactions(&mut self) -> bool {
        let finished: Vec<PaneId> = self
            .pending_compactions
            .iter()
            .filter_map(|(id, pending)| pending.task.is_finished().then_some(*id))
            .collect();
        if finished.is_empty() {
            return false;
        }
        for pane_id in finished {
            let Some(pending) = self.pending_compactions.remove(&pane_id) else {
                continue;
            };
            let result = match pending.task.await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "compaction task panicked");
                    if let Some(pane) = self.pane_mgr.pane_mut(pane_id) {
                        pane.app.compacting = false;
                        pane.app.push_system_message("compaction failed");
                    }
                    continue;
                }
            };
            let Some(pane) = self.pane_mgr.pane_mut(pane_id) else {
                continue;
            };
            crate::slash::apply_compaction_result(
                &mut pane.app,
                &mut pane.conversation,
                result,
                pending.kind,
            );
            // replay anything the user submitted while compaction
            // was running. multiple submits collapse into one combined
            // user turn so the agent sees a coherent prompt rather
            // than firing N back-to-back turns
            replay_compaction_inputs(pane);
        }
        true
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

    /// drain completed oauth usage updates and distribute to all panes
    pub(super) fn poll_usage(&mut self) -> bool {
        let Some(rx) = &mut self.usage_rx else {
            return false;
        };

        let mut latest = None;
        while let Ok(usage) = rx.try_recv() {
            latest = Some(usage);
        }

        let Some(usage) = latest else {
            return false;
        };

        for pane in self.pane_mgr.panes_mut() {
            pane.app.oauth_usage = Some(usage.clone());
            pane.app.oauth_usage_provider = Some("anthropic".into());
        }
        true
    }
}

fn spawn_usage_poller(
    poller: mush_ai::oauth::usage::UsagePoller,
) -> tokio::sync::mpsc::UnboundedReceiver<mush_ai::oauth::usage::OAuthUsage> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            if tx.is_closed() {
                break;
            }
            if let Some(usage) = poller.get_usage().await
                && tx.send(usage).is_err()
            {
                break;
            }
            tokio::time::sleep(USAGE_POLL_TICK).await;
        }
    });
    rx
}

/// take any inputs queued during compaction and turn them into a
/// fresh user turn. multiple queued submits collapse to one combined
/// message (texts joined with blank lines, images concatenated in
/// order) so the agent sees one coherent prompt instead of being
/// pinged repeatedly. no-op when nothing was queued.
fn replay_compaction_inputs(pane: &mut Pane) {
    let drained = pane.drain_compaction_inputs();
    if drained.is_empty() {
        return;
    }
    let mut texts: Vec<String> = Vec::with_capacity(drained.len());
    let mut images: Vec<Vec<u8>> = Vec::new();
    for (text, imgs) in drained {
        texts.push(text);
        images.extend(imgs);
    }
    let combined = texts.join("\n\n");
    if images.is_empty() {
        pane.app.push_user_message(combined.clone());
    } else {
        pane.app
            .push_user_message_with_images(combined.clone(), images);
    }
    pane.app.active_tools.clear();
    pane.app.start_streaming();
    // take_pending_prompts drains every pane's slot, so setting just
    // this pane's slot is enough; no need to thread the focused
    // override through here
    pane.pending_prompt = Some(combined);
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
    app.supported_thinking_levels = tui_config.model.supported_thinking_levels.clone();
    app.thinking_display = tui_config.thinking_display;
    app.interaction.show_cost = tui_config.show_cost;
    app.interaction.show_usage_lines = tui_config.show_usage_lines;
    app.interaction.show_token_counters = tui_config.show_token_counters;
    app.interaction.status_bar = tui_config.status_bar_config.clone();
    app.keymap = tui_config.keymap.clone();
    app.theme = tui_config.theme.clone();
    app.scroll_lines = tui_config.scroll_lines;
    app.compaction_model_id = tui_config
        .compaction_model
        .as_ref()
        .map(|(model, _)| model.id.clone());
    app.cache.seed_for_model(
        &tui_config.model,
        &tui_config.options,
        tui_config.cache_timer,
    );

    app.completion.completions = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|(name, _)| format!("/{name}"))
        .collect();
    app.completion.slash_commands = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|(name, description)| SlashCommand {
            name: (*name).to_string(),
            description: (*description).to_string(),
        })
        .collect();

    for template in mush_ext::discover_templates(cwd) {
        app.completion
            .completions
            .push(format!("/{}", template.name));
        app.completion.slash_commands.push(SlashCommand {
            name: template.name.clone(),
            description: template.description.clone().unwrap_or_default(),
        });
        app.completion.templates.push(template);
    }

    for entry in mush_ai::discovery::merged_catalogue() {
        app.completion.completions.push(entry.model.id.to_string());
        app.completion
            .model_completions
            .push(crate::slash_menu::model_completion_from_merged(&entry));
    }

    app.completion.favourite_models = tui_config.favourite_models.clone();
    app.completion.favourites_locked = tui_config.favourites_locked;

    app
}

fn replay_initial_messages(app: &mut App, initial_messages: &[Message]) -> ConversationState {
    if initial_messages.is_empty() {
        return ConversationState::new();
    }

    crate::conversation_display::rebuild_display(app, initial_messages);
    crate::conversation_display::accumulate_session_stats(app, initial_messages);
    app.status = Some(format!(
        "resumed session ({} messages)",
        initial_messages.len()
    ));
    ConversationState::from_messages(initial_messages.to_vec())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::mpsc;

    use super::*;
    use crate::runner::LastModels;
    use mush_ai::types::{
        Api, AssistantContentPart, AssistantMessage, Provider, StreamOptions, TextContent,
        Timestamp, TokenCount, Usage, UserContent, UserMessage,
    };

    fn test_model() -> mush_ai::types::Model {
        mush_ai::models::all_models_with_user()
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
            config_path: None,
            provider_api_keys: HashMap::new(),
            thinking_prefs: ThinkingPrefs::default(),
            save_thinking_prefs: None,
            last_models: LastModels::default(),
            save_last_models: None,
            save_session: None,
            update_title: None,
            confirm_tools: false,
            auto_compact: false,
            auto_fork_compact: false,
            show_cost: true,
            show_usage_lines: false,
            show_token_counters: false,
            status_bar_config: Default::default(),
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
            resolve_model: None,
            http_client: None,
            session_id: mush_ai::types::SessionId::new(),
            settings: Default::default(),
            scroll_lines: crate::app::DEFAULT_SCROLL_LINES,
            favourite_models: Vec::new(),
            favourites_locked: false,
            save_favourite_models: None,
            keymap: crate::keybinds::KeyMap::default(),
            reload_context: None,
            forbidden_chars: Default::default(),
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
        Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: text.into(),
                })])
                .model(model.id)
                .provider(Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .usage(Usage {
                    input_tokens: TokenCount::new(10),
                    output_tokens: TokenCount::new(5),
                    cache_read_tokens: TokenCount::ZERO,
                    cache_write_tokens: TokenCount::ZERO,
                })
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[test]
    fn build_initial_app_populates_core_display_state() {
        let config = test_config();
        let app = build_initial_app(&config, Path::new("."));

        assert_eq!(app.thinking_level, ThinkingLevel::Medium);
        assert_eq!(app.thinking_display, crate::app::ThinkingDisplay::Collapse);
        assert!(app.interaction.show_cost);
        assert!(app.cache.ttl_secs > 0);
        assert!(
            app.completion
                .completions
                .iter()
                .any(|item| item == "/help")
        );
        assert!(
            app.completion
                .slash_commands
                .iter()
                .any(|cmd| cmd.name == "help")
        );
        assert!(
            app.completion
                .model_completions
                .iter()
                .any(|model| model.id == config.model.id.to_string())
        );
    }

    #[test]
    fn build_initial_app_zeroes_cache_ttl_when_model_lacks_caching() {
        // a model with no cache_read/cache_write pricing has no prompt
        // caching available upstream. the cache warmth timer would just
        // count down nothing, so we suppress it by zeroing ttl_secs even
        // when the user has cache_timer enabled in their config
        let mut config = test_config();
        config.model.cost.cache_read = 0.0;
        config.model.cost.cache_write = 0.0;
        let app = build_initial_app(&config, Path::new("."));
        assert_eq!(app.cache.ttl_secs, 0);
    }

    #[test]
    fn build_initial_app_seeds_serving_provider_for_direct_routes() {
        // direct routes know their serving provider up-front so the
        // cache timer should reflect the correct per-provider TTL
        // immediately. an openai model on the direct openai route
        // should get the openai-specific TTL (10m), not the default
        let mut config = test_config();
        config.model.provider = Provider::Custom("openai".into());
        config.model.cost.cache_read = 1.25;
        config.model.cost.cache_write = 0.0;
        let app = build_initial_app(&config, Path::new("."));
        assert_eq!(app.cache.last_serving_provider.as_deref(), Some("openai"));
        assert_eq!(app.cache.ttl_secs, 600);
    }

    #[test]
    fn build_initial_app_does_not_seed_serving_provider_for_openrouter() {
        // openrouter routes start unknown until /generation reconciles
        let mut config = test_config();
        config.model.provider = Provider::OpenRouter;
        config.model.cost.cache_read = 0.3;
        config.model.cost.cache_write = 3.75;
        let app = build_initial_app(&config, Path::new("."));
        assert!(app.cache.last_serving_provider.is_none());
        // openrouter default fallback (300s) since no learned provider
        assert_eq!(app.cache.ttl_secs, 300);
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

    #[test]
    fn replay_compaction_inputs_collapses_into_single_streaming_turn() {
        // post-compaction the runtime turns every queued submit into a
        // fresh user turn. multiple texts join with a blank-line gap,
        // queued bubbles disappear, streaming starts, and pending_prompt
        // gets the combined string so the runner picks it up next loop
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Pane::new(PaneId::new(1), app);
        pane.queue_input_during_compaction("first thought".into(), vec![]);
        pane.queue_input_during_compaction("second thought".into(), vec![]);

        replay_compaction_inputs(&mut pane);

        assert!(pane.pending_compaction_inputs.is_empty());
        assert!(!pane.app.has_queued_messages());
        assert!(pane.app.stream.active, "should kick off a new stream");
        assert_eq!(
            pane.pending_prompt.as_deref(),
            Some("first thought\n\nsecond thought")
        );
        // the combined prompt should have been pushed as a real (non-
        // queued) user message so it lands in conversation history
        assert!(
            pane.app
                .messages
                .iter()
                .any(|m| !m.queued && m.content.contains("first thought"))
        );
    }

    #[test]
    fn replay_compaction_inputs_is_noop_when_nothing_queued() {
        // common case: user waited out compaction without typing.
        // nothing should fire, nothing should change
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Pane::new(PaneId::new(1), app);

        replay_compaction_inputs(&mut pane);

        assert!(pane.pending_prompt.is_none());
        assert!(!pane.app.stream.active);
        assert!(pane.app.messages.is_empty());
    }

    #[test]
    fn active_tool_labels_include_name_and_summary() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.active_tools
            .push(crate::display_types::ActiveToolState {
                tool_call_id: "tool_1".into(),
                name: "bash".into(),
                summary: "cargo test".into(),
                live_output: None,
                status: crate::display_types::ToolCallStatus::Running,
                output: None,
            });
        let runtime =
            RunnerRuntime::from_pane(Pane::new(PaneId::new(1), app), ThinkingPrefs::default());

        assert_eq!(runtime.active_tool_labels(), vec!["bash: cargo test"]);
    }

    #[test]
    fn running_tool_ticks_activity_shimmer() {
        let mut app = App::new("test".into(), TokenCount::new(200_000));
        app.active_tools
            .push(crate::display_types::ActiveToolState {
                tool_call_id: "tool_1".into(),
                name: "bash".into(),
                summary: "sleep 10".into(),
                live_output: None,
                status: crate::display_types::ToolCallStatus::Running,
                output: None,
            });
        let mut runtime =
            RunnerRuntime::from_pane(Pane::new(PaneId::new(1), app), ThinkingPrefs::default());

        assert!(runtime.tick_animation_panes());
        assert_eq!(runtime.pane_mgr.focused().app.shimmer_phase(), 1);
    }

    #[test]
    fn poll_usage_drains_background_updates_without_awaiting_http() {
        let app = App::new("claude-sonnet-4-5".into(), TokenCount::new(200_000));
        let mut runtime =
            RunnerRuntime::from_pane(Pane::new(PaneId::new(1), app), ThinkingPrefs::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.usage_rx = Some(rx);

        tx.send(mush_ai::oauth::usage::OAuthUsage {
            five_hour: Some(mush_ai::oauth::usage::UsageWindow {
                utilization: 42.0,
                resets_at: chrono::Utc::now() + chrono::TimeDelta::hours(1),
            }),
            seven_day: None,
        })
        .unwrap();

        assert!(runtime.poll_usage());
        let app = &runtime.pane_mgr.focused().app;
        assert_eq!(app.oauth_usage_provider.as_deref(), Some("anthropic"));
        assert_eq!(
            app.oauth_usage
                .as_ref()
                .and_then(|usage| usage.five_hour.as_ref())
                .map(|window| window.utilization),
            Some(42.0)
        );
    }

    #[test]
    fn apply_discovery_updates_reloads_models_and_posts_message() {
        let mut config = test_config();
        let (mut runtime, _) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(RunnerRuntime::new(&mut config));
        let (tx, rx) = mpsc::channel();
        runtime.install_discovery_updates(rx);

        tx.send(DiscoveryUpdate::Refreshed(
            "7 models from openai-codex".into(),
        ))
        .unwrap();

        assert!(runtime.apply_discovery_updates());
        let app = &runtime.pane_mgr.focused().app;
        assert!(app.messages.iter().any(|m| {
            m.content
                .contains("model discovery: 7 models from openai-codex")
        }));
        assert!(!app.completion.model_completions.is_empty());
    }
}
