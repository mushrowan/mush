//! compaction-related slash command implementations
//!
//! split out from commands.rs due to the async complexity
//! and distinct concerns (LLM calls, hook execution)

use std::path::PathBuf;

use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_session::ConversationState;

use crate::app::App;

/// pull the freshly-generated summary text out of a compacted message
/// list. compaction always produces a leading user message tagged with
/// `COMPACTION_SUMMARY_PREFIX`; the body of that message is what we want
/// to surface in the visible chat. returns `None` if no summary message
/// is present (e.g. compaction no-op'd because the conversation was too
/// short)
pub fn extract_fresh_summary(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|m| match m {
        Message::User(u) => crate::conversation_display::extract_compaction_summary(&u.text()),
        _ => None,
    })
}

/// minimum messages before compaction is worthwhile
pub const MIN_MESSAGES_FOR_COMPACTION: usize = 4;

/// owned result of a backgrounded compaction task.
pub enum CompactionTaskResult {
    Applied(AppliedCompaction),
    Failed {
        failure: mush_session::compact::CompactionFailureReport,
        tokens_before: usize,
        model_id: String,
    },
}

/// values needed to apply a successful compaction.
pub struct AppliedCompaction {
    /// flat compacted message list, used as a last-resort fallback when
    /// the tree-native `compact_at_leaf` path can't find the boundary
    pub messages: Vec<Message>,
    /// raw summary text produced by the LLM or the structured fallback.
    /// `apply_compaction_result` writes this into the new tree
    /// `EntryKind::Compaction` node so `collect_context` can synthesise
    /// the visible summary message
    pub summary: String,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// number of messages from the start of the original context that
    /// were folded into `summary`. equivalently: the context index of
    /// the first kept message, used as `first_kept_context_index` when
    /// inserting the tree-native compaction node
    pub summarised_count: usize,
    pub before_count: usize,
    pub model_id: String,
    pub diagnostics: mush_session::compact::CompactionDiagnostics,
}

/// a backgrounded compaction the runtime is still waiting on
pub struct PendingCompaction {
    pub task: tokio::task::JoinHandle<CompactionTaskResult>,
}

/// spawn the LLM compaction on a background task and return its handle.
/// the spawned future owns all its inputs (model, options, registry,
/// hooks, cwd are cloned/converted to owned forms) so the calling code
/// can return to the event loop while the LLM call runs. callers poll
/// the handle from the main loop and use [`apply_compaction_result`]
/// to finalise once the task completes.
///
/// `context_budget` is an optional summariser hint. when set, it's
/// passed through to [`run_compaction`] and surfaces as an extra line
/// in the compaction prompt asking the LLM to target that token count
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

/// finalise a compaction task: insert a tree-native compaction marker
/// so future API calls see the collapsed context while keeping every
/// pre-compaction entry navigable via `/tree` and `/branch`, append a
/// "compacted summary" system banner to the visible chat (without
/// nuking the existing scrollback), reset the "recent live call" token
/// stats so the next real call doesn't trip a false ContextDecrease
/// anomaly, and update the status bar.
///
/// the display chat (app.messages) is unaffected by the LLM-context
/// rewrite: the user keeps their full scrollback, and a single summary
/// banner is appended so it's clear something happened. tree state is
/// also preserved, so `/branch <n>` can rewind to a pre-compaction
/// point if the user wants
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

    // tree-native path: append a Compaction entry summarising the first
    // `summarised_count` context messages. preserves old entries so
    // /tree / /branch can navigate back. falls back to the flat-list
    // replace_messages path when the tree state is unexpected (no
    // entries to compact, or a prior Compaction node already exists)
    let tree_compaction_applied = !result.summary.is_empty()
        && result.summarised_count > 0
        && conversation
            .compact_at_leaf(result.summary.clone(), result.summarised_count)
            .is_some();
    if !tree_compaction_applied {
        conversation.replace_messages(result.messages);
    }

    let after_count = conversation.context_len();
    app.stats
        .reset_live_state(TokenCount::new(result.tokens_after as u64));

    if let Some(body) = summary_body {
        app.push_system_message(crate::conversation_display::format_compaction_banner(&body));
    }
    if let Some(note) = result.diagnostics.status_note() {
        app.push_system_message(note);
    }

    app.status = Some(format!(
        "compacted with {}: {} → {} messages, ~{} → ~{} tokens ({} summarised)",
        result.model_id,
        result.before_count,
        after_count,
        result.tokens_before,
        result.tokens_after,
        result.summarised_count,
    ));
}

/// run LLM compaction on the conversation synchronously. kept as a thin
/// wrapper over [`start_compaction`] + [`apply_compaction_result`] so
/// existing tests and any synchronous callers (e.g. legacy paths,
/// integration tests) keep working unchanged.
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

/// shared compaction + hook logic for /compact and the auto-compaction
/// path triggered when context approaches the model's window
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
    let mut compacted = result.messages;

    if let Some(hooks) = lifecycle_hooks
        && !hooks
            .for_point(mush_agent::HookPoint::PostCompaction)
            .is_empty()
    {
        let hook_results = hooks
            .run_all(mush_agent::HookPoint::PostCompaction, cwd)
            .await;
        let output: String = hook_results
            .iter()
            .filter(|r| !r.output.is_empty())
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !output.is_empty() {
            compacted.push(Message::User(UserMessage {
                content: UserContent::Text(format!("[post-compaction hook output]\n{output}")),
                timestamp_ms: Timestamp::now(),
            }));
        }
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
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
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

    /// /compact must leave app.stats in a "no recent live call" state:
    /// `prev_usage = None` and `context_tokens = post-compact estimate`.
    /// the bug being guarded against is rebuild_display walking the
    /// post-compact assistants and re-applying each one's historical
    /// usage to the live stats. that left prev_usage pointing at the
    /// last surviving pre-compact assistant (a large context size) and
    /// context_tokens matching it; the next real call (smaller context)
    /// then tripped a false `ContextDecrease` cache anomaly and the
    /// token counter never visibly dropped.
    #[tokio::test]
    async fn handle_compact_resets_live_stats() {
        // 24 messages > keep_recent (10), so llm_compact actually
        // summarises rather than no-op'ing
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

    /// `/compact` must surface the compaction summary in `app.messages`
    /// so the user actually sees something happened. specifically, the
    /// fallback summary path (no LLM available) must still produce a
    /// visible system message containing the compaction header.
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

    /// after /compact the user-facing scroll position is left where it
    /// was. the previous implementation rebuilt the display from scratch
    /// (clearing it via `clear_messages`, which reset scroll_offset to
    /// the bottom) and then jumped to the top so the freshly rendered
    /// summary at index 0 was visible. now the summary is appended at
    /// the END of the existing display, so leaving scroll alone keeps
    /// the user where they were and the new banner is naturally below
    /// the last message.
    #[tokio::test]
    async fn handle_compact_does_not_jump_to_top() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("question {i}")));
            msgs.push(assistant_msg(&format!("answer {i}"), 1_000));
        }
        let mut conversation = ConversationState::from_messages(msgs);
        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let registry = ApiRegistry::new();
        let model = test_model();
        let options = StreamOptions::default();

        // simulate the user reading scrolled-up history
        app.scroll_offset = 10;

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

        assert_eq!(
            app.scroll_offset, 10,
            "/compact must not move the scroll position; was {} after, expected 10",
            app.scroll_offset
        );
    }

    /// /compact must NOT block the calling task: the LLM call has to
    /// run on a background `tokio::spawn` so the runner's input loop
    /// can keep redrawing the "compacting…" status. this test proves
    /// the primitives compose: `start_compaction` returns a JoinHandle
    /// that, when awaited and fed to `apply_compaction_result`,
    /// produces the same end-state as the synchronous `handle_compact`
    /// wrapper. that lets the runner spawn the task, return to the
    /// event loop, and finalise on a future iteration without losing
    /// any of the post-compaction invariants.
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

        // same invariants the synchronous path tests assert
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
                .as_deref()
                .is_some_and(|status| status.contains("summary-model"))
        );
    }

    /// /compact must NOT nuke the user's pre-compact display. the
    /// conversation (LLM context) gets the compacted form so future
    /// requests are smaller, but app.messages keeps every previously
    /// rendered message and just appends a system banner with the new
    /// compaction summary. the bug being guarded against: previously
    /// `apply_compaction_result` called `rebuild_display` which wiped
    /// `app.messages` and re-rendered from the compacted conversation,
    /// so users lost their entire scrollback after every /compact.
    #[tokio::test]
    async fn handle_compact_preserves_existing_display_messages() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("question {i}")));
            msgs.push(assistant_msg(&format!("answer {i}"), 1_000));
        }
        let mut conversation = ConversationState::from_messages(msgs.clone());
        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        // simulate an existing populated display, as the user would have
        // built up while chatting before invoking /compact
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

        // pre-compact display preserved in original order
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

        // a compaction summary banner must be appended. diagnostic notes
        // may follow it when the summary had to fall back or retry
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

    /// /compact must preserve the conversation tree so the user can
    /// `/tree` / `/branch` back to pre-compaction state. the previous
    /// implementation called `replace_messages` which built a fresh tree
    /// from the flat compacted message list, wiping every original
    /// entry. now the post-compact tree has the original entries plus a
    /// new `EntryKind::Compaction` node, and the LLM context resolves
    /// to summary + kept messages via `collect_context`
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

        // exactly one Compaction-kind entry should exist now
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

        // the original user/assistant messages must still be reachable
        // via the tree (we can find them by id, not just walk current branch)
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
}
