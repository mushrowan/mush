use std::path::{Path, PathBuf};
use std::sync::mpsc;

use super::{ThinkingPrefs, ThinkingPrefsSaver, TuiConfig};
use mush_ai::types::{Message, ThinkingLevel};
use notify::RecommendedWatcher;

use crate::app::App;
use crate::session::{AgentConnection, Session};
use crate::slash_menu::SlashCommand;

pub(super) const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("help", "show available commands"),
    ("keys", "show keyboard shortcuts"),
    ("subagents", "show or register session supervisors"),
    ("agents", "list the child agents of this session"),
    ("system-prompt", "show the system prompt the model sees"),
    ("goal", "set a goal the harness keeps working toward"),
    ("heartbeat", "prompt this session every so often"),
    ("schedule", "run a prompt later or on an interval"),
    (
        "autonomous",
        "keep working without a human until gates pass or a limit",
    ),
    (
        "new",
        "save session, start fresh ([--name <name> --] [prompt])",
    ),
    ("model", "show or switch model"),
    ("sessions", "browse and resume sessions"),
    ("resume", "browse sessions, or resume one by id"),
    ("branch", "branch from an earlier user message"),
    ("fork", "new session from before an earlier user message"),
    ("clone", "copy the current branch into a new session"),
    ("tree", "browse the conversation tree and jump to a message"),
    (
        "refine",
        "refine the continual harness (prompt notes, memories, skills, subagent specs)",
    ),
    (
        "compact",
        "summarise old messages ([--context-budget <n>] [focus instructions])",
    ),
    ("compaction-model", "view or set the compaction model"),
    (
        "subagent-model",
        "pick the model new subagents use ([<model-id>|unset])",
    ),
    (
        "pause",
        "stop this agent taking new turns ([<reason>] | off)",
    ),
    ("unpause", "let this agent take turns again"),
    ("export", "save conversation as markdown"),
    ("undo", "revert last turn"),
    ("search", "search conversation"),
    (
        "context",
        "token, cost and context usage for this agent and its children",
    ),
    ("cost", "show session cost"),
    ("name", "show or set the session title"),
    ("session", "show the session id, file and message counts"),
    ("rlm-max-depth", "show or set how deep subagents may nest"),
    (
        "btw",
        "ask a side question without adding it to the session",
    ),
    (
        "skills",
        "list loaded skills, their sources and any warnings",
    ),
    ("credits", "show openrouter credit balance"),
    ("endpoints", "show openrouter providers serving a model"),
    ("logs", "show recent log entries"),
    ("mcp", "MCP server status, tools, schemas, reconnect"),
    (
        "settings",
        "view or modify runtime settings (anthropic betas, scope)",
    ),
    ("login", "open the login picker (oauth + api keys)"),
    (
        "login-complete",
        "finish oauth login with the authorization code",
    ),
    ("card", "show this agent's card"),
    ("thinking-expand", "expand or collapse thinking blocks"),
    ("debug", "dump internal state (for bug reports)"),
    ("reload", "rebuild AGENTS.md, prompt templates, and skills"),
    ("refresh-models", "refresh discovered model catalogue"),
    ("quit", "exit mush"),
];

const USAGE_POLL_TICK: std::time::Duration = std::time::Duration::from_secs(60);

pub enum DiscoveryUpdate {
    Refreshed,
}

pub(super) struct RunnerRuntime {
    pub cwd: PathBuf,
    pub session: Session,
    pub thinking_prefs: ThinkingPrefs,
    pub thinking_saver: Option<ThinkingPrefsSaver>,
    pub pending_prompt: Option<String>,
    pub connection_closed: bool,
    pub resize: super::input::ResizeState,
    usage_rx: Option<tokio::sync::mpsc::UnboundedReceiver<mush_ai::oauth::usage::OAuthUsage>>,
    _config_watcher: Option<RecommendedWatcher>,
    config_rx: Option<mpsc::Receiver<crate::theme::Theme>>,
    discovery_rx: Option<mpsc::Receiver<DiscoveryUpdate>>,
}

impl RunnerRuntime {
    #[cfg(test)]
    pub(super) fn from_pane(session: Session, thinking_prefs: ThinkingPrefs) -> Self {
        Self {
            cwd: PathBuf::new(),
            session,
            thinking_prefs,
            thinking_saver: None,
            pending_prompt: None,
            connection_closed: false,
            resize: super::input::ResizeState::default(),
            usage_rx: None,
            _config_watcher: None,
            config_rx: None,
            discovery_rx: None,
        }
    }

    pub(super) async fn new(
        tui_config: &mut TuiConfig,
        mut connection: Box<dyn AgentConnection>,
    ) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut app = build_initial_app(tui_config, &cwd);
        seed_from_state(&mut app, tui_config, &connection.state());
        let history = connection.history().await;
        let context = connection.transcript().await;
        replay_history(&mut app, &history, &context);
        let session = Session::with_connection(app, connection);

        let (_config_watcher, config_rx) = watch_config(tui_config.config_path.as_ref());

        let usage_poller = tui_config.http_client.as_ref().and_then(|client| {
            mush_ai::oauth::load_credentials()
                .ok()
                .filter(|store| store.providers.contains_key("anthropic"))
                .map(|_| mush_ai::oauth::usage::UsagePoller::new(client.clone()))
        });
        let usage_rx = usage_poller.map(spawn_usage_poller);
        tracing::debug!(has_poller = usage_rx.is_some(), "usage poller setup");

        Self {
            cwd,
            session,
            thinking_prefs: std::mem::take(&mut tui_config.thinking_prefs),
            thinking_saver: tui_config.save_thinking_prefs.clone(),
            pending_prompt: None,
            connection_closed: false,
            resize: super::input::ResizeState::default(),
            usage_rx,
            _config_watcher,
            config_rx,
            discovery_rx: None,
        }
    }

    pub(super) fn apply_config_reload(&mut self, tui_config: &mut TuiConfig) {
        if let Some(ref rx) = self.config_rx
            && let Ok(new_theme) = rx.try_recv()
        {
            tui_config.theme = new_theme;
            {
                let pane = &mut self.session;
                pane.app.theme = tui_config.theme.clone();
            }
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
                DiscoveryUpdate::Refreshed => {
                    {
                        let pane = &mut self.session;
                        crate::input::reload_model_completions(&mut pane.app);
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    pub(super) fn focused_should_quit(&self) -> bool {
        self.session.app.should_quit
    }

    pub(super) fn tick_animation_panes(&mut self) -> bool {
        let mut ticked = false;
        {
            let pane = &mut self.session;
            if pane.app.has_visible_animation() {
                pane.app.tick_animation();
                ticked = true;
            }
        }
        ticked
    }

    pub(super) fn tick_streaming_text_panes(&mut self) {
        {
            let pane = &mut self.session;
            if pane.app.stream.active {
                pane.app.tick_streaming_text();
            }
        }
    }

    pub(super) fn needs_periodic_redraw(&self, cache_timer_enabled: bool) -> bool {
        self.session.app.needs_periodic_redraw(cache_timer_enabled)
    }

    pub(super) fn active_tool_labels(&self) -> Vec<String> {
        std::iter::once(&self.session)
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

    pub(super) fn notify_cache_state(&mut self, enabled: bool) {
        if !enabled {
            return;
        }

        {
            let pane = &mut self.session;
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

        {
            let pane = &mut self.session;
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

pub(super) fn promote_queued_inputs(pane: &mut Session) {
    if pane.app.promote_queued_messages() == 0 {
        return;
    }
    pane.app.active_tools.clear();
    pane.app.start_streaming();
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
    tui_config.seed_app(&mut app);
    app.thinking_level = initial_thinking_level(tui_config);
    app.supported_thinking_levels = tui_config.model.supported_thinking_levels.clone();
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

    for skill in &tui_config.skills.skills {
        app.completion
            .completions
            .push(format!("/skill:{}", skill.name));
        app.completion.slash_commands.push(SlashCommand {
            name: format!("skill:{}", skill.name),
            description: skill.description.clone(),
        });
    }

    for entry in mush_ai::discovery::merged_catalogue() {
        app.completion.completions.push(entry.model.id.to_string());
        app.completion
            .model_completions
            .push(crate::slash_menu::model_completion_from_merged(&entry));
    }

    app
}

fn seed_from_state(
    app: &mut App,
    tui_config: &mut TuiConfig,
    state: &mush_runtime::ConnectionState,
) {
    tui_config.session_id = state.session_id.clone();
    tui_config.options.session_id = Some(state.session_id.clone());
    app.session_title = state.title.clone();
    app.thinking_level = state.thinking_level.clone();
    app.subagent_model_id = state
        .subagent_model
        .as_deref()
        .map(mush_ai::types::ModelId::from);
    app.paused = state.paused.clone();
    if state.turn_active {
        app.start_streaming();
    }
    if app.model_id.as_str() != state.model_id {
        app.model_id = state.model_id.as_str().into();
        if let Some(merged) = mush_ai::discovery::find_merged_model_by_id(&state.model_id) {
            app.stats.context_window = merged.model.context_window;
            app.supported_thinking_levels = merged.model.supported_thinking_levels.clone();
            tui_config.model = merged.model;
        }
    }
}

fn initial_thinking_level(tui_config: &TuiConfig) -> ThinkingLevel {
    tui_config
        .options
        .thinking
        .clone()
        .unwrap_or(ThinkingLevel::Off)
        .normalize_visible()
}

fn replay_history(app: &mut App, history: &[Message], context: &[Message]) {
    if context.is_empty() {
        return;
    }
    crate::conversation_display::rebuild_display(app, history);
    crate::conversation_display::accumulate_session_stats(app, context);
    let (usage, cost) = crate::conversation_display::session_spend(history);
    app.stats.restore_totals(&usage, cost);
    app.status = Some(format!("resumed session ({} messages)", history.len()).into());
}

#[cfg(test)]
mod tests {

    use std::sync::mpsc;

    use super::*;
    use mush_session::ConversationState;

    use mush_ai::types::{
        Api, AssistantContentPart, AssistantMessage, Message, Provider, TextContent, Timestamp,
        TokenCount, Usage, UserContent, UserMessage,
    };

    fn test_model() -> mush_ai::types::Model {
        mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .expect("expected at least one model")
    }

    fn test_config() -> TuiConfig {
        let mut config = TuiConfig::for_test();
        config.model = test_model();
        config.options.thinking = Some(ThinkingLevel::Medium);
        config.show_cost = true;
        config.cache_timer = true;
        config
    }

    fn user_message(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
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
    fn every_menu_command_parses_as_a_builtin() {
        for (name, _) in BUILTIN_SLASH_COMMANDS {
            let parsed = crate::slash::parse(&format!("/{name}"));
            assert!(
                !matches!(parsed, Ok(crate::slash::SlashAction::Other { .. })),
                "/{name} is in the menu but does not parse"
            );
        }
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
        let mut config = test_config();
        config.model.provider = Provider::Custom("fixture".into());
        config.model.cost.cache_read = 0.0;
        config.model.cost.cache_write = 0.0;
        let app = build_initial_app(&config, Path::new("."));
        assert_eq!(app.cache.ttl_secs, 0);
    }

    #[test]
    fn build_initial_app_seeds_serving_provider_for_direct_routes() {
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
        let mut config = test_config();
        config.model.provider = Provider::OpenRouter;
        config.model.cost.cache_read = 0.3;
        config.model.cost.cache_write = 3.75;
        let app = build_initial_app(&config, Path::new("."));
        assert!(app.cache.last_serving_provider.is_none());
        assert_eq!(app.cache.ttl_secs, 300);
    }

    #[test]
    fn replay_conversation_restores_display_and_stats() {
        use mush_session::ConversationState;

        let config = test_config();
        let mut app = build_initial_app(&config, Path::new("."));
        let messages = vec![user_message("hello"), assistant_message("hi there")];
        let source = ConversationState::from_messages(messages.clone());

        replay_history(&mut app, &source.display_history(), &source.context());

        assert_eq!(app.messages.len(), 2);
        assert!(app.stats.total_tokens > TokenCount::ZERO);
        assert_eq!(
            app.status.as_ref().map(|s| s.text.as_str()),
            Some("resumed session (2 messages)")
        );
    }

    #[test]
    fn replay_counts_the_spend_from_before_a_compaction() {
        use mush_session::ConversationState;

        let model = mush_ai::models::all_models_with_user()
            .into_iter()
            .find(|model| model.cost.input > 0.0)
            .expect("expected a priced model");
        let priced = |text: &str| {
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::Text(TextContent {
                        text: text.into(),
                    })])
                    .model(model.id.clone())
                    .provider(model.provider.clone())
                    .api(model.api)
                    .usage(Usage {
                        input_tokens: TokenCount::new(1_000_000),
                        ..Usage::default()
                    })
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            )
        };
        let mut source = ConversationState::new();
        for i in 0..6u64 {
            source.append_message(user_message(&format!("question {i}")));
            source.append_message(priced(&format!("answer {i}")));
        }
        source
            .compact_at_leaf("EARLIER SUMMARY".into(), 8)
            .expect("compaction applies");

        let config = test_config();
        let mut app = build_initial_app(&config, Path::new("."));
        replay_history(&mut app, &source.display_history(), &source.context());

        let expected = model.cost.input * 6.0;
        assert!((app.stats.total_cost.get() - expected).abs() < 1e-9);
        assert_eq!(app.stats.total_tokens, TokenCount::new(6_000_000));
    }

    #[test]
    fn replay_conversation_restores_full_scrollback_after_compaction() {
        use mush_session::ConversationState;

        let mut source = ConversationState::new();
        for i in 0..12u64 {
            source.append_message(user_message(&format!("question {i}")));
            source.append_message(assistant_message(&format!("answer {i}")));
        }
        source
            .compact_at_leaf("EARLIER SUMMARY".into(), 14)
            .expect("compaction applies");
        let collapsed_len = source.context().len();

        let config = test_config();
        let mut app = build_initial_app(&config, Path::new("."));
        replay_history(&mut app, &source.display_history(), &source.context());

        assert!(
            app.messages.len() > collapsed_len,
            "resume must restore the full scrollback, got {} display rows vs {collapsed_len} collapsed",
            app.messages.len()
        );
        assert!(
            app.messages
                .iter()
                .any(|m| m.role == crate::app::MessageRole::System
                    && m.content.contains("compacted summary")),
            "the compaction banner should be shown"
        );
    }

    #[tokio::test]
    async fn replay_compaction_inputs_collapses_into_single_streaming_turn() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.queue_input_during_compaction("first thought".into(), vec![])
            .await;
        pane.queue_input_during_compaction("second thought".into(), vec![])
            .await;

        promote_queued_inputs(&mut pane);

        assert!(!pane.app.has_queued_messages());
        assert!(pane.app.stream.active, "should kick off a new stream");
        assert_eq!(
            pane.in_process()
                .runtime
                .pending_prompt
                .as_ref()
                .map(|prompt| prompt.text.as_str()),
            Some("first thought\n\nsecond thought")
        );
        assert!(
            pane.app
                .messages
                .iter()
                .any(|m| !m.queued && m.content.contains("first thought"))
        );
    }

    #[tokio::test]
    async fn replay_compaction_inputs_is_noop_when_nothing_queued() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);

        promote_queued_inputs(&mut pane);

        assert!(pane.in_process().runtime.pending_prompt.is_none());
        assert!(!pane.app.stream.active);
        assert!(pane.app.messages.is_empty());
    }

    #[tokio::test]
    async fn replay_compaction_inputs_preserves_images_for_pending_prompt() {
        use crate::input_buffer::PendingImage;
        use mush_ai::types::{ImageMimeType, Message, UserContent, UserContentPart};

        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.queue_input_during_compaction(
            "look".into(),
            vec![PendingImage {
                data: vec![1, 2, 3, 4],
                mime_type: ImageMimeType::Png,
                dimensions: Some((10, 10)),
            }],
        )
        .await;

        promote_queued_inputs(&mut pane);

        pane.in_process().runtime.persist_pending_prompt();
        assert!(pane.in_process().runtime.pending_prompt.is_some());
        let snapshot = pane.in_process().runtime.snapshot();
        let Message::User(user) = &snapshot.primary.context()[0] else {
            panic!("expected user message");
        };
        let UserContent::Parts(parts) = &user.content else {
            panic!("expected multipart user content");
        };
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, UserContentPart::Image(_)))
        );
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
        let runtime = RunnerRuntime::from_pane(Session::new(app), ThinkingPrefs::default());

        assert_eq!(runtime.active_tool_labels(), vec!["bash: cargo test"]);
    }

    #[tokio::test]
    async fn applied_compaction_shows_the_banner_and_replays_queued_input() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_message(&format!("q {i}")));
            msgs.push(assistant_message(&format!("a {i}")));
        }
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.in_process().runtime.conversation = ConversationState::from_messages(msgs);
        pane.queue_input_during_compaction("while you were away".into(), Vec::new())
            .await;
        let mut config = test_config();

        let context = pane.in_process().runtime.conversation.context();
        let applied = crate::slash::AppliedCompaction {
            messages: context,
            summary: "summary of earlier turns".into(),
            tokens_before: 100,
            tokens_after: 50,
            summarised_count: 14,
            before_count: 24,
            model_id: "test".into(),
            diagnostics: mush_session::compact::CompactionDiagnostics::default(),
            instructions: None,
        };

        super::super::looping::apply_command_events(
            &mut pane,
            &mut config,
            vec![mush_runtime::ClientEvent::Compacted(applied)],
        )
        .await;

        assert!(!pane.app.compacting);
        assert!(
            pane.app
                .status
                .as_ref()
                .is_some_and(|status| status.text.starts_with("compacted with test")),
            "{:?}",
            pane.app.status
        );
        assert_eq!(
            pane.in_process()
                .runtime
                .pending_prompt
                .as_ref()
                .map(|p| p.text.as_str()),
            Some("while you were away")
        );
    }

    #[tokio::test]
    async fn compaction_failure_opens_recovery_prompt() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut runtime = RunnerRuntime::from_pane(Session::new(app), ThinkingPrefs::default());
        let original = vec![user_message("hello"), assistant_message("hi")];
        let diagnostics = mush_session::compact::CompactionDiagnostics {
            model_id: "test".into(),
            retries: 1,
            trimmed_messages: 0,
            fallback_reason: Some("request failed: ephemeral api key rejected".into()),
        };
        let failed = crate::slash::CompactionTaskResult::Failed {
            failure: mush_session::compact::CompactionFailureReport::new(
                original,
                Some(1),
                "request failed: ephemeral api key rejected".into(),
                diagnostics,
            ),
            tokens_before: 10,
            model_id: "test".into(),
        };

        let crate::slash::CompactionTaskResult::Failed {
            failure,
            tokens_before,
            model_id,
        } = failed
        else {
            unreachable!()
        };
        super::super::looping::apply_command_events(
            &mut runtime.session,
            &mut test_config(),
            vec![mush_runtime::ClientEvent::CompactionFailed {
                failure,
                tokens_before,
                model_id,
            }],
        )
        .await;

        let app = &runtime.session.app;
        assert_eq!(
            app.interaction.mode,
            crate::app::AppMode::CompactionRecovery
        );
        let prompt = app
            .interaction
            .compaction_recovery
            .as_ref()
            .expect("recovery prompt should be open");
        assert_eq!(prompt.options.len(), 3);
        assert!(
            prompt
                .options
                .iter()
                .any(|option| option.label == "try again")
        );
        assert!(prompt.options.iter().any(|option| {
            option.label.starts_with("fallback")
                && option.description.contains("structured summary")
        }));
        assert!(
            prompt
                .options
                .iter()
                .any(|option| option.label == "cancel compaction")
        );
        assert!(
            prompt
                .reason
                .contains("request failed: ephemeral api key rejected")
        );
    }

    #[tokio::test]
    async fn fallback_recovery_applies_summary_and_replays_queued_input() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.queue_input_during_compaction("queued thought".into(), vec![])
            .await;
        pane.app.open_compaction_recovery(
            failure_report(vec![
                user_message("old question"),
                assistant_message("old answer"),
                user_message("recent question"),
            ]),
            100,
            "test".into(),
        );
        let mut runtime = RunnerRuntime::from_pane(pane, ThinkingPrefs::default());

        let recovery = runtime
            .session
            .app
            .interaction
            .compaction_recovery
            .take()
            .expect("recovery prompt");
        let result = recovery.failure.into_fallback_result();
        let applied = crate::slash::AppliedCompaction {
            tokens_after: mush_session::compact::estimate_tokens(&result.messages),
            messages: result.messages,
            summary: result.summary,
            tokens_before: recovery.tokens_before,
            summarised_count: result.summarised_count,
            before_count: 3,
            model_id: recovery.model_id,
            diagnostics: result.diagnostics,
            instructions: None,
        };
        {
            let pane = &mut runtime.session;
            let (app, runtime) = pane.app_and_runtime();
            crate::slash::apply_applied_compaction(app, runtime, applied);
            promote_queued_inputs(pane);
        }

        let pane = &mut runtime.session;
        assert!(pane.in_process().runtime.conversation.context_len() < 3);
        assert!(pane.app.stream.active, "queued input should replay");
        assert_eq!(
            pane.in_process()
                .runtime
                .pending_prompt
                .as_ref()
                .map(|prompt| prompt.text.as_str()),
            Some("queued thought")
        );
    }

    #[tokio::test]
    async fn cancel_recovery_replays_queued_input_without_compacting() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.in_process().runtime.conversation = ConversationState::from_messages(vec![
            user_message("old question"),
            assistant_message("old answer"),
        ]);
        pane.queue_input_during_compaction("queued thought".into(), vec![])
            .await;
        let report = failure_report(pane.in_process().runtime.conversation.context());
        pane.app
            .open_compaction_recovery(report, 100, "test".into());
        let mut runtime = RunnerRuntime::from_pane(pane, ThinkingPrefs::default());

        runtime.session.app.close_compaction_recovery();
        {
            let pane = &mut runtime.session;
            pane.app.compacting = false;
            promote_queued_inputs(pane);
        }

        let pane = &mut runtime.session;
        assert_eq!(pane.in_process().runtime.conversation.context_len(), 2);
        assert!(pane.app.stream.active, "queued input should replay");
        assert_eq!(
            pane.in_process()
                .runtime
                .pending_prompt
                .as_ref()
                .map(|prompt| prompt.text.as_str()),
            Some("queued thought")
        );
    }

    fn failure_report(messages: Vec<Message>) -> mush_session::compact::CompactionFailureReport {
        mush_session::compact::CompactionFailureReport::new(
            messages,
            Some(1),
            "request failed: ephemeral api key rejected".into(),
            mush_session::compact::CompactionDiagnostics {
                model_id: "test".into(),
                retries: 1,
                trimmed_messages: 0,
                fallback_reason: Some("request failed: ephemeral api key rejected".into()),
            },
        )
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
        let mut runtime = RunnerRuntime::from_pane(Session::new(app), ThinkingPrefs::default());

        assert!(runtime.tick_animation_panes());
        assert_eq!(runtime.session.app.shimmer_phase(), 1);
    }

    #[test]
    fn poll_usage_drains_background_updates_without_awaiting_http() {
        let app = App::new("claude-sonnet-4-5".into(), TokenCount::new(200_000));
        let mut runtime = RunnerRuntime::from_pane(Session::new(app), ThinkingPrefs::default());
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
        let app = &runtime.session.app;
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
    fn apply_discovery_updates_reloads_models_without_posting_message() {
        let mut config = test_config();
        let mut runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let connection = config.worker_for_test().connect(
                    &mush_agent::tool::ToolRegistry::new(),
                    &mush_ai::registry::ApiRegistry::new(),
                );
                RunnerRuntime::new(&mut config, Box::new(connection)).await
            });
        let (tx, rx) = mpsc::channel();
        runtime.install_discovery_updates(rx);

        tx.send(DiscoveryUpdate::Refreshed).unwrap();

        assert!(runtime.apply_discovery_updates());
        let app = &runtime.session.app;
        assert!(
            !app.messages
                .iter()
                .any(|m| m.content.contains("model discovery")),
            "startup discovery must not post a system message"
        );
        assert!(!app.completion.model_completions.is_empty());
    }

    fn submitted_prompt_id(text: &str) -> mush_ai::types::PromptId {
        let mut runtime = mush_runtime::SessionRuntime::new(ConversationState::new());
        runtime.pending_prompt = Some(crate::session::PendingPrompt::text(text));
        runtime.persist_pending_prompt();
        match runtime.conversation.messages().last().expect("a message") {
            Message::User(user) => user.prompt_id.clone().expect("a prompt id"),
            other => panic!("expected a user message, got {other:?}"),
        }
    }

    #[test]
    fn a_submitted_prompt_carries_a_uuid_prompt_id() {
        let id = submitted_prompt_id("test test 1212 hello").to_string();

        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "id={id}"
        );
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "id={id}"
        );
    }

    #[test]
    fn two_submissions_carry_different_prompt_ids() {
        let first = submitted_prompt_id("hi");
        let second = submitted_prompt_id("and again");

        assert_ne!(first, second);
    }
}
