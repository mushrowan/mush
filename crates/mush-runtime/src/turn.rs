use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use mush_agent::tool::ToolRegistry;
use mush_agent::{AgentConfig, AgentInjections, agent_loop};
use mush_ai::ApiKey;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::{
    ImageContent, Message, Model, PaneId, StreamOptions, TextContent, Timestamp, TokenCount,
    ToolCallId, UserContent, UserContentPart, UserMessage,
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
}

pub struct TurnPrep {
    pub model: Model,
    pub target: CompactionTarget,
    pub api_key: Option<ApiKey>,
    pub account_id: Option<String>,
}

pub struct CompactionTarget {
    pub model: Model,
    pub options: StreamOptions,
    pub window: TokenCount,
}

#[must_use]
pub fn resolve_runtime_model(id: &str, default: &Model) -> Model {
    mush_ai::discovery::resolve_model_by_id(id).unwrap_or_else(|| default.clone())
}

pub async fn resolve_auth_for_model(
    model: &Model,
    provider_api_keys: &HashMap<String, ApiKey>,
) -> (Option<ApiKey>, Option<String>) {
    let resolved = mush_ai::auth::resolve_auth(
        &model.provider,
        provider_api_keys,
        mush_ai::credentials::default_store().as_ref(),
    )
    .await;
    (resolved.api_key, resolved.account_id)
}

pub async fn resolve_auth_into_options(
    model: &Model,
    mut options: StreamOptions,
    provider_api_keys: &HashMap<String, ApiKey>,
) -> StreamOptions {
    let (api_key, account_id) = resolve_auth_for_model(model, provider_api_keys).await;
    options.api_key = api_key;
    options.account_id = account_id;
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
        let (model, mut options) = config
            .compaction_model
            .clone()
            .unwrap_or_else(|| (pane_model.clone(), config.options.clone()));
        options.session_id = Some(self.session_id.clone());
        let options = resolve_auth_into_options(&model, options, &config.provider_api_keys).await;
        CompactionTarget {
            model,
            options,
            window: pane_model.compaction_window(),
        }
    }

    pub async fn prepare_turn(&self, config: &TurnConfig) -> TurnPrep {
        let model = self.model(&config.default_model);
        let target = self.compaction_target(config).await;
        let (api_key, account_id) = resolve_auth_for_model(&model, &config.provider_api_keys).await;
        TurnPrep {
            model,
            target,
            api_key,
            account_id,
        }
    }

    pub async fn start_turn(&mut self, request: TurnRequest, deps: &TurnDeps<'_>) {
        let prep = self.prepare_turn(&deps.config).await;
        self.start_prepared_turn(request, prep, deps);
    }

    pub fn start_prepared_turn(
        &mut self,
        request: TurnRequest,
        prep: TurnPrep,
        deps: &TurnDeps<'_>,
    ) {
        let TurnRequest { pane_id, prompt } = request;
        let TurnPrep {
            model,
            target,
            api_key,
            account_id,
        } = prep;
        let thinking_level = self.thinking_level.clone();
        let context_tokens = self.context_tokens;
        let TurnDeps {
            config,
            injections,
            tools,
            registry,
        } = deps;
        let tools: &ToolRegistry = tools;
        let registry: &ApiRegistry = registry;
        let steering_queue = self.steering_queue.clone();
        self.harness_notice = self.refresh_harness_digest();
        self.pending_prompt = Some(prompt);
        self.persist_pending_prompt();
        let conversation_snapshot = self.conversation.context();
        self.pending_prompt = None;

        let initial_estimated_tokens = estimated_tokens_u64(&conversation_snapshot);
        let context_tokens_shared =
            Arc::new(std::sync::atomic::AtomicU64::new(context_tokens.get()));
        let cwd = injections.cwd.clone().unwrap_or_default();
        let tool_registry = aligned_tools(tools, &model, config, cwd.clone());
        let live = (target.model.id == model.id).then(|| LiveContext {
            system_prompt: mush_agent::effective_system_prompt(
                self.system_prompt.as_deref(),
                injections.dynamic_system_context.as_ref(),
            ),
            tools: mush_agent::tool_definitions(&tool_registry, model.api),
        });
        let transform = build_context_transform(
            target.model,
            target.options,
            config.auto_compact,
            config.compaction,
            model.compaction_window(),
            registry.clone(),
            context_tokens_shared.clone(),
            initial_estimated_tokens,
            injections.lifecycle_hooks.clone(),
            cwd.clone(),
            live,
        );

        let steering = build_steering_callback(steering_queue.clone());
        let follow_up = build_steering_callback(steering_queue.clone());
        let (confirm_req_rx, confirm) = build_confirm_callback(config.confirm_tools);

        let mut call_options = config.options.clone();
        call_options.session_id = Some(self.session_id.clone());
        call_options.api_key = api_key;
        call_options.account_id = account_id;
        call_options.thinking = Some(thinking_level);

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
                    transform,
                    confirm,
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

#[allow(clippy::too_many_arguments)]
pub fn build_context_transform(
    compact_model: Model,
    compact_options: StreamOptions,
    do_auto_compact: bool,
    settings: mush_session::compact::CompactionSettings,
    compaction_window: TokenCount,
    registry: ApiRegistry,
    context_tokens_shared: Arc<std::sync::atomic::AtomicU64>,
    initial_estimated_tokens: u64,
    lifecycle_hooks: mush_agent::LifecycleHooks,
    cwd: std::path::PathBuf,
    live: Option<mush_session::compact::LiveContext>,
) -> Option<mush_agent::TransformFn> {
    if !do_auto_compact {
        return None;
    }
    let baseline_estimated_tokens =
        Arc::new(std::sync::atomic::AtomicU64::new(initial_estimated_tokens));
    let hooks = Arc::new(lifecycle_hooks);
    let cwd = Arc::new(cwd);
    let registry = Arc::new(registry);
    let live = Arc::new(live);

    Some(Box::new(move |messages| {
        let owned_messages = messages.to_vec();
        let model = compact_model.clone();
        let options = compact_options.clone();
        let ctx_tokens = context_tokens_shared.clone();
        let baseline_estimated_tokens = baseline_estimated_tokens.clone();
        let hooks = hooks.clone();
        let cwd = cwd.clone();
        let registry = registry.clone();
        let live = live.clone();
        Box::pin(async move {
            let current_tokens =
                TokenCount::new(ctx_tokens.load(std::sync::atomic::Ordering::Relaxed));
            let current_estimated_tokens = TokenCount::new(estimated_tokens_u64(&owned_messages));
            let baseline_tokens = TokenCount::new(
                baseline_estimated_tokens.load(std::sync::atomic::Ordering::Relaxed),
            );
            let effective_compaction_tokens =
                mush_session::compact::compaction_tokens_for_estimated_payload(
                    current_tokens,
                    baseline_tokens,
                    current_estimated_tokens,
                );
            if !mush_session::compact::needs_compaction_at(
                effective_compaction_tokens,
                compaction_window,
                owned_messages.len(),
                &settings,
            ) {
                baseline_estimated_tokens.store(
                    current_estimated_tokens.get(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                return mush_agent::ContextTransformResult::Unchanged;
            }
            let result = auto_compact(
                owned_messages,
                effective_compaction_tokens,
                compaction_window,
                &registry,
                &model,
                &options,
                Some(&hooks),
                Some(cwd.as_path()),
                live.as_ref().as_ref(),
                &settings,
            )
            .await;
            let tokens_after = estimated_tokens_u64(&result.messages);
            baseline_estimated_tokens.store(tokens_after, std::sync::atomic::Ordering::Relaxed);
            if result.summarised_count > 0 {
                ctx_tokens.store(tokens_after, std::sync::atomic::Ordering::Relaxed);
                mush_agent::ContextTransformResult::Compacted(mush_agent::ContextCompaction {
                    messages: result.messages,
                    summary: result.summary,
                    summarised_count: result.summarised_count,
                })
            } else if result.masked_count > 0 {
                mush_agent::ContextTransformResult::Silent(result.messages)
            } else {
                mush_agent::ContextTransformResult::Unchanged
            }
        })
    }))
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

    if result.masked_count > 0 {
        tracing::info!(
            masked = result.masked_count,
            tokens_saved = result.mask_tokens_saved,
            "observation masking applied"
        );
    }
    if result.summarised_count > 0 {
        tracing::info!(
            summarised = result.summarised_count,
            "LLM compaction applied"
        );
    }

    if let Some(hooks) = lifecycle_hooks
        && !hooks
            .for_point(mush_agent::HookPoint::PostCompaction)
            .is_empty()
    {
        let results = hooks
            .run_all(mush_agent::HookPoint::PostCompaction, cwd)
            .await;
        for r in &results {
            if !r.success {
                tracing::warn!(command = %r.command, "post-compaction hook failed: {}", r.output);
            }
        }
    }

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

        let options = resolve_auth_into_options(&model, stale_options, &provider_api_keys).await;

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
            model,
            StreamOptions::default(),
            true,
            mush_session::compact::CompactionSettings::default(),
            TokenCount::new(10_000),
            ApiRegistry::new(),
            context_tokens,
            0,
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
            None,
        )
        .expect("auto-compaction enabled should build a transform");

        let baseline: Vec<Message> = (0..20).map(|i| user_msg(format!("small {i}"))).collect();
        assert!(matches!(
            transform(&baseline).await,
            mush_agent::ContextTransformResult::Unchanged
        ));

        let mut grown = baseline;
        grown.push(user_msg("x".repeat(16_000)));

        match transform(&grown).await {
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
            model,
            StreamOptions::default(),
            true,
            mush_session::compact::CompactionSettings::default(),
            TokenCount::new(10_000),
            ApiRegistry::new(),
            context_tokens,
            estimated_tokens_u64(&messages),
            mush_agent::LifecycleHooks::default(),
            std::path::PathBuf::from("/tmp"),
            None,
        )
        .expect("auto-compaction enabled should build a transform");

        match transform(&messages).await {
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
