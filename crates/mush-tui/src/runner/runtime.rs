use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use super::{ThinkingPrefs, ThinkingPrefsSaver, TuiConfig, streams};
use mush_ai::types::PaneId;
use mush_ai::types::{Message, ThinkingLevel, Timestamp, UserMessage};
use mush_session::ConversationState;
use notify::RecommendedWatcher;

use crate::app::App;
use crate::session::Session;
use crate::slash::PendingCompaction;
use crate::slash_menu::SlashCommand;

pub(super) const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("help", "show available commands"),
    ("keys", "show keyboard shortcuts"),
    ("new", "save session, start fresh"),
    ("model", "show or switch model"),
    ("sessions", "browse and resume sessions"),
    ("resume", "resume last session in cwd (or by id)"),
    ("branch", "branch from nth user message"),
    ("tree", "show conversation tree"),
    ("compact", "summarise old messages (--context-budget <n>)"),
    ("compaction-model", "view or set the compaction model"),
    ("export", "save conversation as markdown"),
    ("undo", "revert last turn"),
    ("search", "search conversation"),
    ("cost", "show session cost"),
    ("credits", "show openrouter credit balance"),
    ("endpoints", "show openrouter providers serving a model"),
    ("logs", "show recent log entries"),
    ("mcp", "MCP server status, tools, schemas, reconnect"),
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
    ("locks", "list all file locks"),
    ("label", "set pane label"),
    ("panes", "list all panes"),
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
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    pub pending_prompt: Option<String>,
    pub pending_compactions: HashMap<PaneId, PendingCompaction>,
    usage_rx: Option<tokio::sync::mpsc::UnboundedReceiver<mush_ai::oauth::usage::OAuthUsage>>,
    _config_watcher: Option<RecommendedWatcher>,
    config_rx: Option<mpsc::Receiver<crate::theme::Theme>>,
    discovery_rx: Option<mpsc::Receiver<DiscoveryUpdate>>,
}

impl RunnerRuntime {
    #[cfg(test)]
    fn from_pane(session: Session, thinking_prefs: ThinkingPrefs) -> Self {
        Self {
            cwd: PathBuf::new(),
            session,
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

    pub(super) async fn new(tui_config: &mut TuiConfig) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut app = build_initial_app(tui_config, &cwd);
        let conversation = replay_conversation(
            &mut app,
            std::mem::take(&mut tui_config.initial_conversation),
        );
        let session = Session::with_conversation(app, conversation);

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
            lifecycle_hooks: tui_config.lifecycle_hooks.clone(),
            pending_prompt: None,
            pending_compactions: HashMap::new(),
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
            self.session.app.status = Some("config reloaded".into());
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

    pub(super) async fn poll_pending_compactions(&mut self, tui_config: &TuiConfig) -> bool {
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
                    {
                        let pane = &mut self.session;
                        pane.app.compacting = false;
                        pane.app.push_system_message("compaction failed");
                    }
                    continue;
                }
            };
            self.handle_compaction_task_result(pane_id, result, tui_config);
        }
        true
    }

    pub(super) fn handle_compaction_task_result(
        &mut self,
        _pane_id: PaneId,
        result: crate::slash::CompactionTaskResult,
        tui_config: &TuiConfig,
    ) {
        let applied = {
            let pane = &mut self.session;
            match result {
                crate::slash::CompactionTaskResult::Applied(result) => {
                    crate::slash::apply_applied_compaction(
                        &mut pane.app,
                        &mut pane.conversation,
                        result,
                    );
                    replay_compaction_inputs(pane);
                    true
                }
                crate::slash::CompactionTaskResult::Failed {
                    failure,
                    tokens_before,
                    model_id,
                } => {
                    pane.app
                        .open_compaction_recovery(failure, tokens_before, model_id);
                    false
                }
            }
        };

        if applied && let Some(saver) = &tui_config.save_session {
            saver(streams::build_session_snapshot(&self.session, tui_config));
        }
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

pub(super) fn replay_compaction_inputs(pane: &mut Session) {
    let drained = pane.drain_compaction_inputs();
    if drained.is_empty() {
        return;
    }
    let mut texts: Vec<String> = Vec::with_capacity(drained.len());
    let mut images = Vec::new();
    for (text, imgs) in drained {
        texts.push(text);
        images.extend(imgs);
    }
    let combined = texts.join("\n\n");
    let image_bytes: Vec<Vec<u8>> = images.iter().map(|image| image.data.clone()).collect();
    if image_bytes.is_empty() {
        pane.app.push_user_message(combined.clone());
    } else {
        pane.app
            .push_user_message_with_images(combined.clone(), image_bytes);
    }
    pane.app.active_tools.clear();
    pane.app.start_streaming();
    pane.pending_prompt = Some(crate::session::PendingPrompt::with_images(combined, images));
}

pub(super) fn save_pane_pending_prompt(pane: &mut Session) {
    let Some(prompt) = pane.pending_prompt.as_mut() else {
        return;
    };
    if prompt.is_persisted() {
        return;
    }
    save_prompt_to_conversation(&mut pane.conversation, prompt);
    prompt.mark_persisted();
}

pub(super) fn save_prompt_to_conversation(
    conversation: &mut ConversationState,
    prompt: &crate::session::PendingPrompt,
) {
    let user_message = Message::User(
        UserMessage::builder()
            .content(streams::build_user_content(&prompt.text, &prompt.images))
            .timestamp_ms(Timestamp::now())
            .build(),
    );
    conversation.append_message(user_message);
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
    app.thinking_level = tui_config
        .options
        .thinking
        .clone()
        .unwrap_or(ThinkingLevel::Off)
        .normalize_visible();
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

    for entry in mush_ai::discovery::merged_catalogue() {
        app.completion.completions.push(entry.model.id.to_string());
        app.completion
            .model_completions
            .push(crate::slash_menu::model_completion_from_merged(&entry));
    }

    app
}

fn replay_conversation(app: &mut App, conversation: ConversationState) -> ConversationState {
    if conversation.is_empty() {
        return conversation;
    }

    let history = conversation.display_history();
    crate::conversation_display::rebuild_display(app, &history);
    crate::conversation_display::accumulate_session_stats(app, &conversation.context());
    app.status = Some(format!("resumed session ({} messages)", history.len()).into());
    conversation
}

#[cfg(test)]
mod tests {

    use std::sync::mpsc;

    use super::*;

    use mush_ai::types::{
        Api, AssistantContentPart, AssistantMessage, Provider, TextContent, Timestamp, TokenCount,
        Usage, UserContent, UserMessage,
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

        let conversation = replay_conversation(&mut app, source);

        assert_eq!(conversation.context(), messages);
        assert_eq!(app.messages.len(), 2);
        assert!(app.stats.total_tokens > TokenCount::ZERO);
        assert_eq!(
            app.status.as_ref().map(|s| s.text.as_str()),
            Some("resumed session (2 messages)")
        );
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
        let resumed = replay_conversation(&mut app, source);

        assert_eq!(resumed.context().len(), collapsed_len);
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
        assert!(
            resumed
                .tree()
                .entries()
                .iter()
                .any(|e| matches!(e.kind, mush_session::tree::EntryKind::Compaction { .. })),
            "the compaction node must survive resume"
        );
    }

    #[test]
    fn replay_compaction_inputs_collapses_into_single_streaming_turn() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.queue_input_during_compaction("first thought".into(), vec![]);
        pane.queue_input_during_compaction("second thought".into(), vec![]);

        replay_compaction_inputs(&mut pane);

        assert!(pane.pending_compaction_inputs.is_empty());
        assert!(!pane.app.has_queued_messages());
        assert!(pane.app.stream.active, "should kick off a new stream");
        assert_eq!(
            pane.pending_prompt
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

    #[test]
    fn replay_compaction_inputs_is_noop_when_nothing_queued() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);

        replay_compaction_inputs(&mut pane);

        assert!(pane.pending_prompt.is_none());
        assert!(!pane.app.stream.active);
        assert!(pane.app.messages.is_empty());
    }

    #[test]
    fn replay_compaction_inputs_preserves_images_for_pending_prompt() {
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
        );

        replay_compaction_inputs(&mut pane);

        save_pane_pending_prompt(&mut pane);
        assert!(pane.pending_prompt.is_some());
        let snapshot = crate::runner::streams::build_session_snapshot(&pane, &test_config());
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

    #[test]
    fn applied_compaction_persists_session() {
        use std::sync::{Arc, Mutex};

        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_message(&format!("q {i}")));
            msgs.push(assistant_message(&format!("a {i}")));
        }
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.conversation = ConversationState::from_messages(msgs);
        let mut runtime = RunnerRuntime::from_pane(pane, ThinkingPrefs::default());

        let saved: Arc<Mutex<Option<crate::runner::SessionSnapshot>>> = Arc::new(Mutex::new(None));
        let saved_clone = saved.clone();
        let mut config = test_config();
        config.save_session = Some(Arc::new(move |snapshot| {
            *saved_clone.lock().unwrap() = Some(snapshot);
        }));

        let context = runtime.session.conversation.context();
        let applied = crate::slash::AppliedCompaction {
            messages: context,
            summary: "summary of earlier turns".into(),
            tokens_before: 100,
            tokens_after: 50,
            summarised_count: 14,
            before_count: 24,
            model_id: "test".into(),
            diagnostics: mush_session::compact::CompactionDiagnostics::default(),
        };
        let result = crate::slash::CompactionTaskResult::Applied(applied);

        runtime.handle_compaction_task_result(PaneId::new(1), result, &config);

        let snapshot = saved
            .lock()
            .unwrap()
            .take()
            .expect("session must be saved after a successful compaction");
        let has_compaction =
            snapshot.primary.tree().entries().iter().any(|entry| {
                matches!(entry.kind, mush_session::tree::EntryKind::Compaction { .. })
            });
        assert!(
            has_compaction,
            "persisted snapshot must contain the compaction node"
        );
    }

    #[test]
    fn compaction_failure_opens_recovery_prompt() {
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

        runtime.handle_compaction_task_result(PaneId::new(1), failed, &test_config());

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

    #[test]
    fn fallback_recovery_applies_summary_and_replays_queued_input() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.queue_input_during_compaction("queued thought".into(), vec![]);
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
        };
        {
            let pane = &mut runtime.session;
            crate::slash::apply_applied_compaction(&mut pane.app, &mut pane.conversation, applied);
            replay_compaction_inputs(pane);
        }

        let pane = &mut runtime.session;
        assert!(pane.conversation.context_len() < 3);
        assert!(pane.app.stream.active, "queued input should replay");
        assert_eq!(
            pane.pending_prompt
                .as_ref()
                .map(|prompt| prompt.text.as_str()),
            Some("queued thought")
        );
    }

    #[test]
    fn cancel_recovery_replays_queued_input_without_compacting() {
        let app = App::new("test".into(), TokenCount::new(200_000));
        let mut pane = Session::new(app);
        pane.conversation = ConversationState::from_messages(vec![
            user_message("old question"),
            assistant_message("old answer"),
        ]);
        pane.queue_input_during_compaction("queued thought".into(), vec![]);
        pane.app.open_compaction_recovery(
            failure_report(pane.conversation.context()),
            100,
            "test".into(),
        );
        let mut runtime = RunnerRuntime::from_pane(pane, ThinkingPrefs::default());

        runtime.session.app.close_compaction_recovery();
        {
            let pane = &mut runtime.session;
            pane.app.compacting = false;
            replay_compaction_inputs(pane);
        }

        let pane = &mut runtime.session;
        assert_eq!(pane.conversation.context_len(), 2);
        assert!(pane.app.stream.active, "queued input should replay");
        assert_eq!(
            pane.pending_prompt
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
            .block_on(RunnerRuntime::new(&mut config));
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
}
