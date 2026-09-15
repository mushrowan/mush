use std::path::PathBuf;

use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_session::ConversationState;

use crate::app::App;

pub fn extract_fresh_summary(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|m| match m {
        Message::User(u) => crate::conversation_display::extract_compaction_summary(&u.text()),
        _ => None,
    })
}

pub const MIN_MESSAGES_FOR_COMPACTION: usize = 4;

pub enum CompactionTaskResult {
    Applied(AppliedCompaction),
    Failed {
        failure: mush_session::compact::CompactionFailureReport,
        tokens_before: usize,
        model_id: String,
    },
}

pub struct AppliedCompaction {
    pub messages: Vec<Message>,
    pub summary: String,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub summarised_count: usize,
    pub before_count: usize,
    pub model_id: String,
    pub diagnostics: mush_session::compact::CompactionDiagnostics,
}

pub struct PendingCompaction {
    pub task: tokio::task::JoinHandle<CompactionTaskResult>,
}

#[allow(clippy::too_many_arguments)]
pub fn start_compaction(
    messages: Vec<Message>,
    model: Model,
    options: StreamOptions,
    registry: ApiRegistry,
    lifecycle_hooks: Option<mush_agent::LifecycleHooks>,
    cwd: Option<PathBuf>,
    context_budget: Option<TokenCount>,
) -> tokio::task::JoinHandle<CompactionTaskResult> {
    let before_count = messages.len();
    let model_id = model.id.to_string();
    tokio::spawn(async move {
        let run = run_compaction(
            messages,
            &model,
            &options,
            &registry,
            lifecycle_hooks.as_ref(),
            cwd.as_deref(),
            context_budget,
        )
        .await;
        match run {
            RunCompactionResult::Applied(applied) => {
                CompactionTaskResult::Applied(AppliedCompaction {
                    before_count,
                    model_id,
                    ..applied
                })
            }
            RunCompactionResult::Failed {
                failure,
                tokens_before,
            } => CompactionTaskResult::Failed {
                failure,
                tokens_before,
                model_id,
            },
        }
    })
}

pub fn apply_compaction_result(
    app: &mut App,
    conversation: &mut ConversationState,
    result: CompactionTaskResult,
) {
    let CompactionTaskResult::Applied(result) = result else {
        return;
    };
    apply_applied_compaction(app, conversation, result);
}

pub fn apply_applied_compaction(
    app: &mut App,
    conversation: &mut ConversationState,
    result: AppliedCompaction,
) {
    app.compacting = false;
    let summary_body = extract_fresh_summary(&result.messages);
    if let Some(Message::Event(event)) = result.messages.first()
        && event.kind == mush_ai::types::EventKind::Summary
    {
        app.push_event_message(event);
    }

    conversation.apply_compaction(result.summary, result.summarised_count, result.messages);

    let after_count = conversation.context_len();
    app.stats
        .reset_live_state(TokenCount::new(result.tokens_after as u64));

    if let Some(body) = summary_body {
        app.push_system_message(crate::conversation_display::format_compaction_banner(&body));
    }
    if let Some(note) = result.diagnostics.status_note() {
        app.push_system_message(note);
    }

    app.status = Some(
        format!(
            "compacted with {}: {} → {} messages, ~{} → ~{} tokens ({} summarised)",
            result.model_id,
            result.before_count,
            after_count,
            result.tokens_before,
            result.tokens_after,
            result.summarised_count,
        )
        .into(),
    );
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_compact(
    app: &mut App,
    conversation: &mut ConversationState,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
    context_budget: Option<TokenCount>,
) {
    let messages = conversation.context();
    let before = messages.len();
    if before <= MIN_MESSAGES_FOR_COMPACTION {
        app.push_system_message("conversation too short to compact");
        return;
    }

    app.status = Some("compacting…".into());
    let task = start_compaction(
        messages,
        model.clone(),
        options.clone(),
        registry.clone(),
        lifecycle_hooks.cloned(),
        cwd.map(std::path::Path::to_path_buf),
        context_budget,
    );
    let Ok(result) = task.await else {
        app.push_system_message("compaction task failed");
        return;
    };
    match result {
        CompactionTaskResult::Applied(result) => {
            apply_applied_compaction(app, conversation, result)
        }
        CompactionTaskResult::Failed {
            failure,
            tokens_before,
            model_id,
        } => {
            let applied =
                fallback_applied_compaction(failure, tokens_before, model_id, lifecycle_hooks, cwd)
                    .await;
            apply_applied_compaction(app, conversation, applied);
        }
    }
}

pub async fn fallback_applied_compaction(
    failure: mush_session::compact::CompactionFailureReport,
    tokens_before: usize,
    model_id: String,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> AppliedCompaction {
    let before_count = failure.original_messages().len();
    let result = failure.into_fallback_result();
    build_applied_compaction(
        result,
        tokens_before,
        before_count,
        model_id,
        lifecycle_hooks,
        cwd,
    )
    .await
}

pub enum RunCompactionResult {
    Applied(AppliedCompaction),
    Failed {
        failure: mush_session::compact::CompactionFailureReport,
        tokens_before: usize,
    },
}

#[allow(clippy::too_many_arguments)]
pub async fn run_compaction(
    messages: Vec<Message>,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
    context_budget: Option<TokenCount>,
) -> RunCompactionResult {
    use mush_session::compact;

    let tokens_before = compact::estimate_tokens(&messages);
    let result = match compact::try_llm_compact(
        messages,
        registry,
        model,
        options,
        Some(10),
        context_budget,
    )
    .await
    {
        Ok(result) => result,
        Err(failure) => {
            return RunCompactionResult::Failed {
                failure,
                tokens_before,
            };
        }
    };

    RunCompactionResult::Applied(
        build_applied_compaction(
            result,
            tokens_before,
            0,
            String::new(),
            lifecycle_hooks,
            cwd,
        )
        .await,
    )
}

async fn build_applied_compaction(
    result: mush_session::compact::CompactionResult,
    tokens_before: usize,
    before_count: usize,
    model_id: String,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> AppliedCompaction {
    use mush_session::compact;

    let summary = result.summary.clone();
    let summarised_count = result.summarised_count;
    let diagnostics = result.diagnostics;
    let compacted = result.messages;

    if let Some(hooks) = lifecycle_hooks
        && !hooks
            .for_point(mush_agent::HookPoint::PostCompaction)
            .is_empty()
    {
        let hook_results = hooks
            .run_all(mush_agent::HookPoint::PostCompaction, cwd)
            .await;
        for r in &hook_results {
            if !r.success {
                tracing::warn!(command = %r.command, "post-compaction hook failed: {}", r.output);
            }
        }
    }

    let tokens_after = compact::estimate_tokens(&compacted);
    AppliedCompaction {
        messages: compacted,
        summary,
        tokens_before,
        tokens_after,
        summarised_count,
        before_count,
        model_id,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use mush_ai::types::{
        Api, AssistantContentPart, AssistantMessage, Provider, TextContent, Timestamp, TokenCount,
        Usage, UserContent, UserMessage,
    };

    struct SummaryProvider;

    #[async_trait::async_trait]
    impl mush_ai::registry::ApiProvider for SummaryProvider {
        fn api(&self) -> Api {
            Api::AnthropicMessages
        }

        async fn stream(
            &self,
            model: &Model,
            _context: &mush_ai::registry::LlmContext,
            _options: &StreamOptions,
        ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
            let message = AssistantMessage::builder()
                .model(model.id.clone())
                .provider(model.provider.clone())
                .api(model.api)
                .timestamp_ms(Timestamp::zero())
                .build();
            Ok(Box::pin(futures::stream::iter(vec![
                mush_ai::stream::StreamEvent::TextDelta {
                    content_index: 0,
                    delta: "successful summary".into(),
                },
                mush_ai::stream::StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                },
            ])))
        }
    }

    fn test_model() -> Model {
        mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .expect("at least one model")
    }

    fn user_msg(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn assistant_msg(text: &str, input_tokens: u64) -> Message {
        Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: text.into(),
                })])
                .model(test_model().id)
                .provider(Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .usage(Usage {
                    input_tokens: TokenCount::new(input_tokens),
                    output_tokens: TokenCount::new(20),
                    cache_read_tokens: TokenCount::ZERO,
                    cache_write_tokens: TokenCount::ZERO,
                })
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    #[tokio::test]
    async fn handle_compact_resets_live_stats() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("question {i}")));
            msgs.push(assistant_msg(&format!("answer {i}"), 1_000 + i * 100));
        }
        let mut conversation = ConversationState::from_messages(msgs);

        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(SummaryProvider));
        let mut model = test_model();
        model.api = Api::AnthropicMessages;
        model.provider = Provider::Anthropic;
        let options = StreamOptions::default();

        handle_compact(
            &mut app,
            &mut conversation,
            &model,
            &options,
            &registry,
            None,
            None,
            None,
        )
        .await;

        assert!(
            app.stats.prev_usage().is_none(),
            "prev_usage must be cleared after /compact (was {:?})",
            app.stats.prev_usage()
        );

        let post = conversation.context();
        let estimate = mush_session::compact::estimate_tokens(&post);
        assert_eq!(
            app.stats.context_tokens,
            TokenCount::new(estimate as u64),
            "context_tokens must reflect post-compact estimate"
        );
    }

    #[tokio::test]
    async fn handle_compact_pushes_visible_summary() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("question {i}")));
            msgs.push(assistant_msg(&format!("answer {i}"), 1_000));
        }
        let mut conversation = ConversationState::from_messages(msgs);
        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(SummaryProvider));
        let model = test_model();
        let options = StreamOptions::default();

        handle_compact(
            &mut app,
            &mut conversation,
            &model,
            &options,
            &registry,
            None,
            None,
            None,
        )
        .await;

        let summary_idx = app.messages.iter().position(|m| {
            m.role == crate::app::MessageRole::System && m.content.contains("compacted summary")
        });
        assert!(
            summary_idx.is_some(),
            "expected a system message containing 'compacted summary', got messages: {:?}",
            app.messages
                .iter()
                .map(|m| (
                    m.role.clone(),
                    m.content.chars().take(40).collect::<String>()
                ))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn start_and_apply_match_handle_compact_endstate() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("question {i}")));
            msgs.push(assistant_msg(&format!("answer {i}"), 1_000));
        }
        let conversation = ConversationState::from_messages(msgs.clone());

        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let mut model = test_model();
        model.api = Api::AnthropicMessages;
        model.provider = Provider::Anthropic;
        let options = StreamOptions::default();
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(SummaryProvider));

        let task = start_compaction(
            conversation.context(),
            model,
            options,
            registry,
            None,
            None,
            None,
        );
        let result = task.await.expect("compaction task panicked");

        let mut conv = conversation;
        apply_compaction_result(&mut app, &mut conv, result);

        assert!(app.stats.prev_usage().is_none());
        let summary_present = app.messages.iter().any(|m| {
            m.role == crate::app::MessageRole::System && m.content.contains("compacted summary")
        });
        assert!(
            summary_present,
            "compaction summary missing from app.messages"
        );
    }

    #[test]
    fn apply_compaction_result_surfaces_diagnostic_note() {
        let mut conversation = ConversationState::from_messages(vec![
            user_msg("question"),
            assistant_msg("answer", 10),
        ]);
        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let result = CompactionTaskResult::Applied(AppliedCompaction {
            messages: conversation.context(),
            summary: String::new(),
            tokens_before: 100,
            tokens_after: 50,
            summarised_count: 1,
            before_count: 2,
            model_id: "summary-model".into(),
            diagnostics: mush_session::compact::CompactionDiagnostics {
                model_id: "summary-model".into(),
                retries: 2,
                trimmed_messages: 0,
                fallback_reason: None,
            },
        });

        apply_compaction_result(&mut app, &mut conversation, result);

        assert!(
            app.messages
                .iter()
                .any(|m| m.content.contains("compaction recovered after 2 retries")),
            "expected diagnostic system message, got: {:?}",
            app.messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
        );
        assert!(
            app.status
                .as_ref()
                .is_some_and(|status| status.text.contains("summary-model"))
        );
    }

    #[tokio::test]
    async fn handle_compact_preserves_existing_display_messages() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("question {i}")));
            msgs.push(assistant_msg(&format!("answer {i}"), 1_000));
        }
        let mut conversation = ConversationState::from_messages(msgs.clone());
        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        crate::conversation_display::rebuild_display(&mut app, &msgs);
        let display_count_before = app.messages.len();
        assert!(
            display_count_before > 0,
            "test precondition: existing display should be populated"
        );
        let first_content_before = app.messages[0].content.clone();
        let last_content_before = app.messages.last().unwrap().content.clone();

        let registry = ApiRegistry::new();
        let model = test_model();
        let options = StreamOptions::default();

        handle_compact(
            &mut app,
            &mut conversation,
            &model,
            &options,
            &registry,
            None,
            None,
            None,
        )
        .await;

        assert!(
            app.messages.len() > display_count_before,
            "expected display to grow (summary appended), \
             before={display_count_before} after={}",
            app.messages.len()
        );
        assert_eq!(
            app.messages[0].content, first_content_before,
            "first display message must be untouched"
        );
        assert_eq!(
            app.messages[display_count_before - 1].content,
            last_content_before,
            "the message that used to be last must still appear before the new summary"
        );

        assert!(
            app.messages.iter().skip(display_count_before).any(|m| {
                m.role == crate::app::MessageRole::System && m.content.contains("compacted summary")
            }),
            "expected appended summary banner, got: {:?}",
            app.messages
                .iter()
                .skip(display_count_before)
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_resumed_session_still_shows_the_compaction_banner() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("question {i}")));
            msgs.push(assistant_msg(&format!("answer {i}"), 1_000));
        }
        let mut conversation = ConversationState::from_messages(msgs);
        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let registry = ApiRegistry::new();

        handle_compact(
            &mut app,
            &mut conversation,
            &test_model(),
            &StreamOptions::default(),
            &registry,
            None,
            None,
            None,
        )
        .await;

        let json = serde_json::to_string(&conversation).expect("serialise conversation");
        let loaded: ConversationState = serde_json::from_str(&json).expect("load conversation");

        let banner = |messages: &[Message]| {
            let mut resumed = App::new(test_model().id, TokenCount::new(200_000));
            crate::conversation_display::rebuild_display(&mut resumed, messages);
            resumed
                .messages
                .iter()
                .any(|m| m.content.contains("compacted summary"))
        };

        assert!(
            banner(&loaded.display_history()),
            "startup resume should show the banner, history: {:?}",
            loaded
                .display_history()
                .iter()
                .map(message_preview)
                .collect::<Vec<_>>()
        );
        assert!(
            banner(&loaded.context()),
            "/resume should show the banner, context: {:?}",
            loaded
                .context()
                .iter()
                .map(message_preview)
                .collect::<Vec<_>>()
        );
    }

    fn message_preview(message: &Message) -> String {
        let text = match message {
            Message::User(user) => user.text(),
            Message::Assistant(assistant) => assistant.text(),
            _ => String::new(),
        };
        text.chars().take(40).collect()
    }

    #[tokio::test]
    async fn handle_compact_preserves_conversation_tree() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("q {i}")));
            msgs.push(assistant_msg(&format!("a {i}"), 1_000));
        }
        let mut conversation = ConversationState::from_messages(msgs);
        let entries_before = conversation.tree().entries().len();
        assert_eq!(entries_before, 24, "test precondition: 24 entries");

        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let registry = ApiRegistry::new();
        let model = test_model();
        let options = StreamOptions::default();

        handle_compact(
            &mut app,
            &mut conversation,
            &model,
            &options,
            &registry,
            None,
            None,
            None,
        )
        .await;

        let entries_after = conversation.tree().entries().len();
        assert!(
            entries_after > entries_before,
            "tree must grow (compaction node appended), not shrink. \
             before={entries_before} after={entries_after}. \
             if entries_after < entries_before the tree was wiped via replace_messages"
        );

        let compaction_count = conversation
            .tree()
            .entries()
            .iter()
            .filter(|e| matches!(e.kind, mush_session::tree::EntryKind::Compaction { .. }))
            .count();
        assert_eq!(
            compaction_count, 1,
            "expected exactly one Compaction tree entry after /compact"
        );

        let original_messages_present = conversation
            .tree()
            .entries()
            .iter()
            .filter(|e| matches!(e.kind, mush_session::tree::EntryKind::Message { .. }))
            .count();
        assert_eq!(
            original_messages_present, 24,
            "all 24 original Message entries must remain in the tree"
        );
    }

    #[tokio::test]
    async fn a_post_compaction_hooks_output_stays_out_of_the_conversation() {
        let mut hooks = mush_agent::LifecycleHooks::default();
        hooks.set(
            mush_agent::HookPoint::PostCompaction,
            vec![mush_agent::LifecycleHook {
                tool_match: "*".into(),
                command: "echo reloaded the index".into(),
                timeout: std::time::Duration::from_secs(5),
                blocking: false,
            }],
        );
        let result = mush_session::compact::CompactionResult {
            messages: vec![user_msg("the summary"), user_msg("a kept message")],
            summarised_count: 4,
            summary: "the summary".into(),
            diagnostics: mush_session::compact::CompactionDiagnostics::default(),
        };

        let applied =
            build_applied_compaction(result, 100, 6, "test".into(), Some(&hooks), None).await;

        let texts: Vec<String> = applied
            .messages
            .iter()
            .map(|message| match message {
                Message::User(user) => user.text(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts, vec!["the summary", "a kept message"]);
    }
}
