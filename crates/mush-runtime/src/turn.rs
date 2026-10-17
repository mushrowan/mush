use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use mush_agent::tool::ToolRegistry;
use mush_agent::{AgentConfig, AgentInjections, agent_loop};
use mush_ai::ApiKey;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    AccountLabel, ImageContent, Message, Model, PaneId, StreamOptions, TextContent, Timestamp,
    TokenCount, ToolCallId, UserContent, UserContentPart, UserMessage,
};
use mush_session::compact::LiveContext;
use tokio::sync::Mutex;

use crate::session::{ConfirmRequest, PendingImage, PendingPrompt, SessionRuntime, StreamMeta};

#[derive(Clone)]
pub struct TurnConfig {
    pub default_model: Model,
    pub options: StreamOptions,
    pub max_turns: usize,
    pub provider_api_keys: HashMap<String, ApiKey>,
    pub confirm_tools: bool,
    pub auto_compact: bool,
    pub compaction: mush_session::compact::CompactionSettings,
    pub branch_summary: mush_session::compact::CompactionSettings,
    pub auto_refine: mush_session::refine::AutoRefineSettings,
    pub auto_fork_compact: bool,
    pub compaction_model: Option<(Model, StreamOptions)>,
    pub forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars,
    pub tool_allowlist: Option<Vec<String>>,
    pub generate_title: bool,
}

pub struct TurnDeps<'a> {
    pub config: TurnConfig,
    pub injections: AgentInjections,
    pub tools: &'a ToolRegistry,
    pub registry: &'a ApiRegistry,
}

pub struct TurnRequest {
    pub pane_id: PaneId,
    pub prompt: PendingPrompt,
    pub goal: Option<String>,
}

pub struct TurnPrep {
    pub model: Model,
    pub target: CompactionTarget,
    pub api_key: Option<ApiKey>,
    pub account_id: Option<String>,
    pub oauth_account: Option<AccountLabel>,
}

#[derive(Clone)]
pub struct CompactionTarget {
    pub model: Model,
    pub options: StreamOptions,
    pub window: TokenCount,
    pub session: Option<(Model, StreamOptions)>,
}

impl CompactionTarget {
    #[must_use]
    pub fn fitting(self, required_tokens: impl FnOnce() -> usize) -> Self {
        let Some((model, options)) = self.session else {
            return Self {
                session: None,
                ..self
            };
        };
        let required_tokens = required_tokens();
        if mush_session::compact::window_fits(&self.model, required_tokens) {
            return Self {
                session: Some((model, options)),
                ..self
            };
        }
        tracing::warn!(
            compaction_model = %self.model.id,
            required_tokens,
            "compaction model window is too small for the summary request, using the session model"
        );
        Self {
            model,
            options,
            session: None,
            ..self
        }
    }

    #[must_use]
    pub fn session_model(model: Model, options: StreamOptions) -> Self {
        Self {
            window: model.compaction_window(),
            model,
            options,
            session: None,
        }
    }

    #[must_use]
    pub fn uses_session_model(&self) -> bool {
        self.session.is_none()
    }
}

#[must_use]
pub fn resolve_runtime_model(id: &str, default: &Model) -> Model {
    mush_ai::discovery::resolve_model_by_id(id).unwrap_or_else(|| default.clone())
}

pub async fn resolve_auth_for_model(
    model: &Model,
    provider_api_keys: &HashMap<String, ApiKey>,
    account: Option<&AccountLabel>,
) -> (Option<ApiKey>, Option<String>) {
    let resolved = mush_ai::auth::resolve_auth(
        &model.provider,
        provider_api_keys,
        mush_ai::credentials::default_store().as_ref(),
        account,
    )
    .await;
    (resolved.api_key, resolved.account_id)
}

pub async fn resolve_auth_into_options(
    model: &Model,
    mut options: StreamOptions,
    provider_api_keys: &HashMap<String, ApiKey>,
    account: Option<AccountLabel>,
) -> StreamOptions {
    let (api_key, account_id) =
        resolve_auth_for_model(model, provider_api_keys, account.as_ref()).await;
    options.api_key = api_key;
    options.account_id = account_id;
    options.oauth_account = account;
    options
}

#[must_use]
pub fn build_user_content(text: &str, images: &[PendingImage]) -> UserContent {
    if images.is_empty() {
        return UserContent::Text(text.to_owned());
    }
    let mut parts: Vec<UserContentPart> = vec![UserContentPart::Text(TextContent {
        text: text.to_owned(),
    })];
    for image in images {
        use base64::Engine;
        parts.push(UserContentPart::Image(ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(&image.data),
            mime_type: image.mime_type,
        }));
    }
    UserContent::Parts(parts)
}

impl SessionRuntime {
    #[must_use]
    pub fn model(&self, default: &Model) -> Model {
        resolve_runtime_model(&self.model_id, default)
    }

    pub fn persist_pending_prompt(&mut self) {
        let Some(prompt) = self.pending_prompt.as_mut() else {
            return;
        };
        if prompt.is_persisted() {
            return;
        }
        let user_message = Message::User(
            UserMessage::builder()
                .content(build_user_content(&prompt.text, &prompt.images))
                .timestamp_ms(Timestamp::now())
                .prompt_id(mush_ai::types::PromptId::new())
                .build(),
        );
        self.conversation.append_message(user_message);
        prompt.mark_persisted();
    }

    pub async fn compaction_target(&self, config: &TurnConfig) -> CompactionTarget {
        let pane_model = self.model(&config.default_model);
        let window = pane_model.compaction_window();
        let (model, options) = config
            .compaction_model
            .clone()
            .unwrap_or_else(|| (pane_model.clone(), config.options.clone()));
        let options = self.resolved_options(&model, options, config).await;
        let session = if model.id == pane_model.id {
            None
        } else {
            let options = self
                .resolved_options(&pane_model, config.options.clone(), config)
                .await;
            Some((pane_model, options))
        };
        CompactionTarget {
            model,
            options,
            window,
            session,
        }
    }

    #[must_use]
    pub fn call_options(&self, config: &TurnConfig) -> StreamOptions {
        let mut options = config.options.clone();
        options.session_id = Some(self.session_id.clone());
        options.thinking = Some(self.thinking_level.clone());
        options
    }

    #[must_use]
    pub fn live_context(&self, aligned_tools: &ToolRegistry, model: &Model) -> LiveContext {
        LiveContext {
            system_prompt: self.system_prompt.clone(),
            tools: mush_agent::tool_definitions(aligned_tools, model.api),
        }
    }

    async fn resolved_options(
        &self,
        model: &Model,
        mut options: StreamOptions,
        config: &TurnConfig,
    ) -> StreamOptions {
        options.session_id = Some(self.session_id.clone());
        resolve_auth_into_options(
            model,
            options,
            &config.provider_api_keys,
            self.account_for(model),
        )
        .await
    }

    pub async fn prepare_turn(&self, config: &TurnConfig) -> TurnPrep {
        let model = self.model(&config.default_model);
        let target = self.compaction_target(config).await;
        let oauth_account = self.account_for(&model);
        let (api_key, account_id) =
            resolve_auth_for_model(&model, &config.provider_api_keys, oauth_account.as_ref()).await;
        TurnPrep {
            model,
            target,
            api_key,
            account_id,
            oauth_account,
        }
    }

    pub fn start_prepared_turn(
        &mut self,
        request: TurnRequest,
        prep: TurnPrep,
        deps: &TurnDeps<'_>,
    ) {
        let TurnRequest {
            pane_id,
            prompt,
            goal,
        } = request;
        let TurnPrep {
            model,
            target,
            api_key,
            account_id,
            oauth_account,
        } = prep;
        if let Some(kernel) = &self.kernel_state {
            kernel.set_model(
                &model.id,
                model.input.contains(&mush_ai::types::InputModality::Image),
            );
        }
        let TurnDeps {
            ref config,
            ref injections,
            tools,
            registry,
        } = *deps;
        let steering_queue = self.steering_queue.clone();
        let unsent_prompt = (!prompt.is_persisted()).then_some(prompt.text.as_str());
        self.deliver_pending_harness_digest(goal.as_deref(), unsent_prompt);
        self.refresh_notes();
        self.restate_task_brief();
        self.record_thinking_level();
        self.pending_prompt = Some(prompt);
        self.persist_pending_prompt();
        let conversation_snapshot = self.conversation.context();
        self.pending_prompt = None;
        if self.context_tokens.get() == 0
            && let Some(measured) =
                mush_session::compact::last_measured_tokens(&conversation_snapshot)
        {
            self.context_tokens = measured;
        }
        let context_tokens = self.context_tokens;

        let measured_len = mush_session::compact::measured_len(&conversation_snapshot);
        let context_tokens_shared =
            Arc::new(std::sync::atomic::AtomicU64::new(context_tokens.get()));
        let cwd = injections.cwd.clone().unwrap_or_default();
        let tool_registry = aligned_tools(tools, &model, config, cwd.clone());
        let live = Some(self.live_context(&tool_registry, &model));
        let mut call_options = self.call_options(config);
        call_options.api_key = api_key;
        call_options.account_id = account_id;
        call_options.oauth_account = oauth_account;
        let next_prompt = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let transform = build_context_transform(
            target,
            self.auto_compact.clone(),
            config.compaction,
            model.compaction_window(),
            registry.clone(),
            context_tokens_shared.clone(),
            measured_len,
            injections.lifecycle_hooks.clone(),
            cwd.clone(),
            live,
            AfterCompaction {
                kernel: self.kernel_state.clone(),
                notes: self.notes.clone(),
                task: self.conversation.task_brief(),
            },
            next_prompt.clone(),
        );

        let steering = build_steering_callback(steering_queue.clone());
        let follow_up = build_steering_callback(steering_queue.clone());
        let (confirm_req_rx, confirm) = build_confirm_callback(config.confirm_tools);

        let cancel = tokio_util::sync::CancellationToken::new();
        let generation = self.turns.register_active(
            pane_id,
            StreamMeta {
                steering_queue,
                confirm_req_rx,
                confirm_reply: Arc::new(Mutex::new(None)),
                model: model.clone(),
                context_tokens: context_tokens_shared,
                cancel: cancel.clone(),
            },
        );

        let system_prompt = self.system_prompt.clone();
        let max_turns = config.max_turns;
        let injections = injections.clone();
        self.turns.spawn(registry.clone(), move |registry| {
            let config = AgentConfig {
                model,
                system_prompt,
                tools: tool_registry,
                registry,
                options: call_options,
                max_turns,
                hooks: Box::new(mush_agent::ClosureHooks {
                    steering,
                    follow_up,
                    transform: Some(transform),
                    confirm,
                    prompt_tokens: Some(prompt_tokens_from(next_prompt)),
                }),
                injections,
                cancel: Some(cancel),
            };
            Box::pin(futures::StreamExt::map(
                agent_loop(config, conversation_snapshot),
                move |event| (pane_id, generation, event),
            ))
        });
    }
}

#[derive(Clone, Default)]
pub struct AfterCompaction {
    pub kernel: Option<Arc<dyn mush_kernel::KernelState>>,
    pub notes: Option<Arc<crate::notes::Notes>>,
    pub task: Option<String>,
}

impl AfterCompaction {
    pub async fn restore(
        &self,
        result: mush_session::compact::AutoCompactResult,
    ) -> mush_agent::ContextCompaction {
        let mut messages = result.messages;
        let restored = self.messages(&messages).await;
        let appended = restored.len();
        messages.extend(restored);
        mush_agent::ContextCompaction {
            messages,
            summary: result.summary,
            summarised_count: result.summarised_count,
            appended,
        }
    }

    async fn messages(&self, compacted: &[Message]) -> Vec<Message> {
        let notice = crate::kernel_notice::after_compaction(self.kernel.as_deref()).await;
        let task = self
            .task
            .as_deref()
            .and_then(|task| mush_session::brief::restatement(task, compacted));
        let notes = self.notes.as_ref().and_then(|notes| {
            notes.delivery(crate::notes::latest_fingerprint(compacted).as_deref())
        });
        [notice, task, notes.map(|delivery| delivery.message)]
            .into_iter()
            .flatten()
            .collect()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_context_transform(
    target: CompactionTarget,
    enabled: Arc<std::sync::atomic::AtomicBool>,
    settings: mush_session::compact::CompactionSettings,
    compaction_window: TokenCount,
    registry: ApiRegistry,
    context_tokens_shared: Arc<std::sync::atomic::AtomicU64>,
    measured_len: usize,
    lifecycle_hooks: mush_agent::LifecycleHooks,
    cwd: std::path::PathBuf,
    live: Option<mush_session::compact::LiveContext>,
    after: AfterCompaction,
    next_prompt: Arc<std::sync::atomic::AtomicU64>,
) -> mush_agent::TransformFn {
    let measured_len = Arc::new(std::sync::atomic::AtomicUsize::new(measured_len));
    let overhead = live
        .as_ref()
        .map_or(0, mush_session::compact::overhead_tokens);
    let hooks = Arc::new(lifecycle_hooks);
    let cwd = Arc::new(cwd);
    let registry = Arc::new(registry);
    let live = Arc::new(live);

    Box::new(move |messages, pressure| {
        let owned_messages = messages.to_vec();
        let enabled = enabled.load(std::sync::atomic::Ordering::Relaxed);
        let target = target.clone();
        let ctx_tokens = context_tokens_shared.clone();
        let measured_len = measured_len.clone();
        let hooks = hooks.clone();
        let cwd = cwd.clone();
        let registry = registry.clone();
        let live = live.clone();
        let after = after.clone();
        let next_prompt = next_prompt.clone();
        Box::pin(async move {
            let current_tokens =
                TokenCount::new(ctx_tokens.load(std::sync::atomic::Ordering::Relaxed));
            let measured_tokens = mush_session::compact::compaction_tokens(
                current_tokens,
                &owned_messages,
                measured_len.load(std::sync::atomic::Ordering::Relaxed),
                overhead,
            );
            next_prompt.store(measured_tokens.get(), std::sync::atomic::Ordering::Relaxed);
            if !enabled {
                measured_len.store(owned_messages.len(), std::sync::atomic::Ordering::Relaxed);
                return mush_agent::ContextTransformResult::Unchanged;
            }
            let effective_compaction_tokens = match pressure {
                mush_agent::ContextPressure::Normal => measured_tokens,
                mush_agent::ContextPressure::Overflowed => measured_tokens.max(compaction_window),
            };
            if !mush_session::compact::needs_compaction_at(
                effective_compaction_tokens,
                compaction_window,
                &settings,
            ) {
                measured_len.store(owned_messages.len(), std::sync::atomic::Ordering::Relaxed);
                return mush_agent::ContextTransformResult::Unchanged;
            }
            let target = target.fitting(|| {
                mush_session::compact::summary_request_tokens(
                    &owned_messages,
                    settings.keep_recent(&owned_messages),
                    &mush_session::compact::CompactionFocus::default(),
                    &settings,
                )
            });
            let result = auto_compact(
                owned_messages,
                effective_compaction_tokens,
                compaction_window,
                &registry,
                &target.model,
                &target.options,
                Some(&hooks),
                Some(cwd.as_path()),
                live.as_ref()
                    .as_ref()
                    .filter(|_| target.uses_session_model()),
                &settings,
            )
            .await;
            if result.summarised_count == 0 {
                measured_len.store(result.messages.len(), std::sync::atomic::Ordering::Relaxed);
                return mush_agent::ContextTransformResult::Unchanged;
            }
            let compaction = after.restore(result).await;
            measured_len.store(
                compaction.messages.len(),
                std::sync::atomic::Ordering::Relaxed,
            );
            let compacted = estimated_tokens_u64(&compaction.messages);
            ctx_tokens.store(compacted, std::sync::atomic::Ordering::Relaxed);
            next_prompt.store(
                compacted.saturating_add(overhead as u64),
                std::sync::atomic::Ordering::Relaxed,
            );
            mush_agent::ContextTransformResult::Compacted(compaction)
        })
    })
}

pub fn prompt_tokens_from(
    next_prompt: Arc<std::sync::atomic::AtomicU64>,
) -> mush_agent::PromptTokensFn {
    Box::new(move || {
        let tokens = next_prompt.load(std::sync::atomic::Ordering::Relaxed);
        (tokens > 0).then(|| TokenCount::new(tokens))
    })
}

#[must_use]
pub fn estimated_tokens_u64(messages: &[Message]) -> u64 {
    mush_session::compact::estimate_tokens(messages)
        .try_into()
        .unwrap_or(u64::MAX)
}

fn build_steering_callback(
    steering_queue: Arc<Mutex<Vec<Message>>>,
) -> Option<mush_agent::SteeringFn> {
    Some(Box::new(move || {
        let steering_queue = steering_queue.clone();
        Box::pin(async move {
            let mut queue = steering_queue.lock().await;
            queue.drain(..).collect()
        })
    }))
}

fn build_confirm_callback(
    confirm_tools: bool,
) -> (
    tokio::sync::mpsc::Receiver<ConfirmRequest>,
    Option<mush_agent::ConfirmFn>,
) {
    let (confirm_req_tx, confirm_req_rx) = tokio::sync::mpsc::channel::<ConfirmRequest>(1);
    if !confirm_tools {
        return (confirm_req_rx, None);
    }
    let confirm: mush_agent::ConfirmFn = Box::new(
        move |tool_call_id: &ToolCallId, name: &str, args: &serde_json::Value| {
            let tx = confirm_req_tx.clone();
            let summary = mush_agent::summarise_tool_args(name, args);
            let prompt = format!("{name} {summary}");
            let tool_call_id = tool_call_id.clone();
            Box::pin(async move {
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                if tx
                    .send(ConfirmRequest {
                        tool_call_id,
                        prompt,
                        reply: resp_tx,
                    })
                    .await
                    .is_err()
                {
                    return mush_agent::ConfirmAction::Allow;
                }
                match resp_rx.await {
                    Ok(true) => mush_agent::ConfirmAction::Allow,
                    _ => mush_agent::ConfirmAction::Deny,
                }
            })
        },
    );
    (confirm_req_rx, Some(confirm))
}

#[must_use]
pub fn aligned_tools(
    tools: &ToolRegistry,
    model: &Model,
    config: &TurnConfig,
    cwd: PathBuf,
) -> ToolRegistry {
    let mut tool_registry = tools.clone();
    mush_tools::align_for_model(
        &mut tool_registry,
        model,
        Arc::from(cwd),
        config.forbidden_chars.clone(),
    );
    if let Some(allowed) = &config.tool_allowlist {
        tool_registry.retain_named(allowed);
    }
    tool_registry
}

#[allow(clippy::too_many_arguments)]
pub async fn auto_compact(
    messages: Vec<Message>,
    context_tokens: TokenCount,
    context_window: TokenCount,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
    live: Option<&mush_session::compact::LiveContext>,
    settings: &mush_session::compact::CompactionSettings,
) -> mush_session::compact::AutoCompactResult {
    let result = mush_session::compact::auto_compact(
        messages,
        context_tokens,
        context_window,
        registry,
        model,
        options,
        live,
        settings,
    )
    .await;

    if result.summarised_count > 0 {
        tracing::info!(
            summarised = result.summarised_count,
            "LLM compaction applied"
        );
    }

    crate::compaction::run_post_compaction_hooks(lifecycle_hooks, cwd).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Api, InputModality, ModelCost, Provider};

    #[tokio::test]
    async fn compaction_target_follows_the_pane_model_not_the_startup_one() {
        let catalogue = mush_ai::models::all_models_with_user();
        let startup = catalogue.first().unwrap().clone();
        let switched = catalogue
            .iter()
            .find(|m| m.id != startup.id && m.compaction_window() != startup.compaction_window())
            .unwrap()
            .clone();
        let config = crate::testing::echo_config(startup);
        let mut runtime = SessionRuntime::default();
        runtime.model_id = switched.id.to_string();

        let target = runtime.compaction_target(&config).await;

        assert_eq!(target.model.id, switched.id);
        assert_eq!(target.window, switched.compaction_window());
    }

    #[tokio::test]
    async fn compaction_target_prefers_the_configured_summariser_model() {
        let catalogue = mush_ai::models::all_models_with_user();
        let pane_model = catalogue.first().unwrap().clone();
        let summariser = catalogue
            .iter()
            .find(|m| m.id != pane_model.id)
            .unwrap()
            .clone();
        let mut config = crate::testing::echo_config(pane_model.clone());
        config.compaction_model = Some((summariser.clone(), StreamOptions::default()));
        let mut runtime = SessionRuntime::default();
        runtime.model_id = pane_model.id.to_string();

        let target = runtime.compaction_target(&config).await;

        assert_eq!(target.model.id, summariser.id);
        assert_eq!(target.window, pane_model.compaction_window());
        assert_eq!(
            target.session.as_ref().map(|(model, _)| &model.id),
            Some(&pane_model.id)
        );
        assert!(!target.uses_session_model());
    }

    #[test]
    fn a_summary_request_too_big_for_the_compaction_model_falls_back_to_the_session_model() {
        let session = custom_model("session");
        let mut summariser = custom_model("summariser");
        summariser.context_window = TokenCount::new(8_000);
        let target = |summariser: &Model| CompactionTarget {
            model: summariser.clone(),
            options: StreamOptions::default(),
            window: session.compaction_window(),
            session: Some((session.clone(), StreamOptions::default())),
        };

        let fits = target(&summariser).fitting(|| 8_000);
        assert_eq!(fits.model.id, summariser.id);
        assert!(!fits.uses_session_model());

        let falls_back = target(&summariser).fitting(|| 8_001);
        assert_eq!(falls_back.model.id, session.id);
        assert!(falls_back.uses_session_model());
        assert_eq!(falls_back.window, session.compaction_window());

        summariser.context_window = TokenCount::ZERO;
        assert_eq!(
            target(&summariser).fitting(|| 1_000_000).model.id,
            summariser.id
        );
        assert_eq!(
            CompactionTarget::session_model(session.clone(), StreamOptions::default())
                .fitting(|| 1_000_000)
                .model
                .id,
            session.id
        );
    }

    fn custom_model(provider: &str) -> Model {
        Model {
            id: format!("{provider}/model").into(),
            name: "test model".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom(provider.into()),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: Vec::new(),
            },
            context_window: TokenCount::new(128_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(4096),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }

    #[tokio::test]
    async fn resolve_auth_into_options_replaces_stale_key_for_compaction() {
        let provider = "compaction-auth-test-provider";
        let model = custom_model(provider);
        let fresh_key = ApiKey::new("fresh-compaction-key").unwrap();
        let mut provider_api_keys = HashMap::new();
        provider_api_keys.insert(provider.into(), fresh_key.clone());
        let stale_options = StreamOptions {
            api_key: Some(ApiKey::new("stale-startup-key").unwrap()),
            account_id: Some("stale-account".into()),
            ..StreamOptions::default()
        };

        let options =
            resolve_auth_into_options(&model, stale_options, &provider_api_keys, None).await;

        assert_eq!(options.api_key, Some(fresh_key));
        assert_eq!(options.account_id, None);
    }

    #[test]
    fn build_user_content_persists_pending_prompt_text() {
        let content = build_user_content("hello from pending", &[]);

        assert_eq!(content.text(), "hello from pending");
    }

    fn user_msg(text: impl Into<String>) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[tokio::test]
    async fn context_transform_compacts_after_unsent_payload_growth() {
        let model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        let context_tokens = Arc::new(std::sync::atomic::AtomicU64::new(6_000));
        let transform = build_context_transform(
            CompactionTarget::session_model(model, StreamOptions::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            mush_session::compact::CompactionSettings {
                keep_recent_tokens: 2_000,
                ..mush_session::compact::CompactionSettings::default()
            },
            TokenCount::new(10_000),
            ApiRegistry::new(),
            context_tokens,
            0,
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
            None,
            AfterCompaction::default(),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );

        let baseline: Vec<Message> = (0..20).map(|i| user_msg(format!("small {i}"))).collect();
        assert!(matches!(
            transform(&baseline, mush_agent::ContextPressure::Normal).await,
            mush_agent::ContextTransformResult::Unchanged
        ));

        let mut grown = baseline;
        grown.push(user_msg("x".repeat(16_000)));

        match transform(&grown, mush_agent::ContextPressure::Normal).await {
            mush_agent::ContextTransformResult::Compacted(compaction) => {
                assert!(
                    compaction.messages.len() < grown.len(),
                    "unsent payload growth should force compaction before the next provider call"
                );
            }
            other => panic!("expected compaction update, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_switched_off_transform_leaves_even_an_overflowing_context_alone() {
        let model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transform = build_context_transform(
            CompactionTarget::session_model(model, StreamOptions::default()),
            enabled.clone(),
            mush_session::compact::CompactionSettings {
                keep_recent_tokens: 2_000,
                ..mush_session::compact::CompactionSettings::default()
            },
            TokenCount::new(10_000),
            ApiRegistry::new(),
            Arc::new(std::sync::atomic::AtomicU64::new(9_900)),
            0,
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
            None,
            AfterCompaction::default(),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        let mut messages: Vec<Message> = (0..20).map(|i| user_msg(format!("small {i}"))).collect();
        messages.push(user_msg("x".repeat(16_000)));

        for pressure in [
            mush_agent::ContextPressure::Normal,
            mush_agent::ContextPressure::Overflowed,
        ] {
            assert!(matches!(
                transform(&messages, pressure).await,
                mush_agent::ContextTransformResult::Unchanged
            ));
        }

        enabled.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            transform(&messages, mush_agent::ContextPressure::Normal).await,
            mush_agent::ContextTransformResult::Compacted(_)
        ));
    }

    struct NamedKernel;

    #[async_trait::async_trait]
    impl mush_kernel::KernelState for NamedKernel {
        async fn live_names(&self) -> Option<Result<Vec<String>, String>> {
            Some(Ok(vec!["df".into(), "load".into()]))
        }
    }

    #[tokio::test]
    async fn an_auto_compaction_tells_the_model_which_python_names_are_still_live() {
        let model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        let transform = build_context_transform(
            CompactionTarget::session_model(model, StreamOptions::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            mush_session::compact::CompactionSettings {
                keep_recent_tokens: 2_000,
                ..mush_session::compact::CompactionSettings::default()
            },
            TokenCount::new(10_000),
            ApiRegistry::new(),
            Arc::new(std::sync::atomic::AtomicU64::new(6_000)),
            0,
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
            None,
            AfterCompaction {
                kernel: Some(Arc::new(NamedKernel)),
                ..AfterCompaction::default()
            },
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        let mut grown: Vec<Message> = (0..20).map(|i| user_msg(format!("small {i}"))).collect();
        grown.push(user_msg("x".repeat(16_000)));

        let mush_agent::ContextTransformResult::Compacted(compaction) =
            transform(&grown, mush_agent::ContextPressure::Normal).await
        else {
            panic!("expected a compaction");
        };
        let mut conversation = mush_session::ConversationState::from_messages(grown.clone());
        let applied = crate::apply_agent_event(
            &mut conversation,
            &mush_agent::AgentEvent::ContextCompacted {
                before_count: grown.len(),
                compaction: compaction.clone(),
            },
        );
        assert!(
            matches!(applied, crate::Applied::Compacted { .. }),
            "the session must keep the compaction the model saw"
        );
        assert_eq!(conversation.context().last(), compaction.messages.last());

        let Some(Message::User(last)) = compaction.messages.last() else {
            panic!("{:?}", compaction.messages.last());
        };
        assert_eq!(
            last.text(),
            "[python-state]\n\nYour Python kernel persisted through compaction; its remaining \
             variables, imports, and helpers are still available. These names are still \
             defined: df, load."
        );
    }

    fn user_text_of(message: &Message) -> String {
        let Message::User(user) = message else {
            panic!("{message:?}");
        };
        user.text()
    }

    fn grown_child_transform(after: AfterCompaction) -> mush_agent::TransformFn {
        let model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        build_context_transform(
            CompactionTarget::session_model(model, StreamOptions::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            mush_session::compact::CompactionSettings {
                keep_recent_tokens: 2_000,
                ..mush_session::compact::CompactionSettings::default()
            },
            TokenCount::new(10_000),
            ApiRegistry::new(),
            Arc::new(std::sync::atomic::AtomicU64::new(6_000)),
            0,
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
            None,
            after,
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        )
    }

    #[tokio::test]
    async fn an_auto_compaction_restates_the_task_and_shows_the_notes_again() {
        let task = "[task from parent]\n\nwrite the report";
        let notes = crate::notes::Notes::new(None);
        notes.append("the draft lives in /tmp/report.md").unwrap();
        let transform = grown_child_transform(AfterCompaction {
            notes: Some(notes),
            task: Some(task.into()),
            ..AfterCompaction::default()
        });
        let mut grown = vec![user_msg(task)];
        grown.extend((0..20).map(|i| user_msg(format!("small {i}"))));
        grown.push(user_msg("x".repeat(16_000)));

        let mush_agent::ContextTransformResult::Compacted(compaction) =
            transform(&grown, mush_agent::ContextPressure::Normal).await
        else {
            panic!("expected a compaction");
        };

        assert_eq!(compaction.appended, 2);
        let tail: Vec<String> = compaction.messages[compaction.messages.len() - 2..]
            .iter()
            .map(user_text_of)
            .collect();
        assert!(mush_session::brief::is_restated(&tail[0]), "{tail:?}");
        assert!(tail[0].ends_with("\n\nwrite the report"), "{tail:?}");
        assert!(tail[1].contains("/tmp/report.md"), "{tail:?}");

        let mut conversation = mush_session::ConversationState::from_messages(grown.clone());
        let applied = crate::apply_agent_event(
            &mut conversation,
            &mush_agent::AgentEvent::ContextCompacted {
                before_count: grown.len(),
                compaction: compaction.clone(),
            },
        );
        assert!(matches!(applied, crate::Applied::Compacted { .. }));
        let context = conversation.context();
        assert_eq!(context.len(), compaction.messages.len());
        assert_eq!(
            context[context.len() - 2..],
            compaction.messages[compaction.messages.len() - 2..]
        );
    }

    #[tokio::test]
    async fn a_task_the_compaction_kept_is_not_restated() {
        let task = "[task from parent]\n\nwrite the report";
        let transform = grown_child_transform(AfterCompaction {
            task: Some(task.into()),
            ..AfterCompaction::default()
        });
        let mut grown: Vec<Message> = (0..20).map(|i| user_msg(format!("small {i}"))).collect();
        grown.push(user_msg("x".repeat(16_000)));
        grown.push(user_msg(task));

        let mush_agent::ContextTransformResult::Compacted(compaction) =
            transform(&grown, mush_agent::ContextPressure::Normal).await
        else {
            panic!("expected a compaction");
        };

        assert_eq!(compaction.appended, 0);
        assert!(
            !compaction
                .messages
                .iter()
                .any(|message| mush_session::brief::is_restated(&user_text_of(message)))
        );
    }

    #[test]
    fn the_first_turn_after_a_compaction_restates_the_task_once() {
        let task = "[task from parent]\n\nwrite the report";
        let mut messages = vec![user_msg(task)];
        messages.extend((0..12).map(|i| user_msg(format!("step {i}"))));
        let mut runtime = SessionRuntime::new(mush_session::ConversationState::from_messages(
            messages.clone(),
        ));
        assert!(!runtime.restate_task_brief());

        let compacted = [vec![user_msg("summary")], messages[10..].to_vec()].concat();
        assert!(
            runtime
                .apply_compaction("summary".into(), 10, &compacted)
                .is_some()
        );

        assert!(runtime.restate_task_brief());
        assert!(!runtime.restate_task_brief());
        let context = runtime.conversation.context();
        let last = user_text_of(context.last().unwrap());
        assert!(mush_session::brief::is_restated(&last), "{last}");
        assert!(last.ends_with("\n\nwrite the report"));
    }

    #[tokio::test]
    async fn context_transform_does_not_double_count_initial_payload_estimate() {
        let model = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        let context_tokens = Arc::new(std::sync::atomic::AtomicU64::new(9_000));
        let messages: Vec<Message> = (0..20)
            .map(|i| user_msg(format!("small {i}: {}", "x".repeat(120))))
            .collect();
        let transform = build_context_transform(
            CompactionTarget::session_model(model, StreamOptions::default()),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            mush_session::compact::CompactionSettings::default(),
            TokenCount::new(10_000),
            ApiRegistry::new(),
            context_tokens,
            messages.len(),
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
            None,
            AfterCompaction::default(),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );

        match transform(&messages, mush_agent::ContextPressure::Normal).await {
            mush_agent::ContextTransformResult::Unchanged => {}
            other => {
                panic!("expected unchanged context without new estimated growth, got {other:?}")
            }
        }
    }

    #[test]
    fn persist_pending_prompt_appends_once() {
        let mut runtime = SessionRuntime::new(mush_session::ConversationState::new());
        runtime.pending_prompt = Some(PendingPrompt::text("hi"));

        runtime.persist_pending_prompt();
        runtime.persist_pending_prompt();

        assert_eq!(runtime.conversation.messages().len(), 1);
        assert!(runtime.pending_prompt.as_ref().unwrap().is_persisted());
    }
}
