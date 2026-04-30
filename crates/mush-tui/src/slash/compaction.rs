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

/// owned result of a backgrounded compaction task. carries every value
/// the synchronous finisher needs so the LLM call can run on a
/// `tokio::spawn` task without borrowing from app/conversation/runtime.
pub struct CompactionTaskResult {
    pub messages: Vec<Message>,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub summarised_count: usize,
    pub before_count: usize,
}

/// which compaction flavour produced a `CompactionTaskResult`. drives
/// only the post-task status formatting; the apply path is otherwise
/// identical for both kinds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionKind {
    /// `/compact` - rewrites the active branch in place
    Compact,
    /// `/fork-compact` - branched the tree before compacting; the
    /// original is still navigable via `/tree`
    ForkCompact,
}

/// a backgrounded compaction the runtime is still waiting on. the
/// kind disambiguates the status text shown when the task lands
pub struct PendingCompaction {
    pub task: tokio::task::JoinHandle<CompactionTaskResult>,
    pub kind: CompactionKind,
}

/// spawn the LLM compaction on a background task and return its handle.
/// the spawned future owns all its inputs (model, options, registry,
/// hooks, cwd are cloned/converted to owned forms) so the calling code
/// can return to the event loop while the LLM call runs. callers poll
/// the handle from the main loop and use [`apply_compaction_result`]
/// to finalise once the task completes.
pub fn start_compaction(
    messages: Vec<Message>,
    model: Model,
    options: StreamOptions,
    registry: ApiRegistry,
    lifecycle_hooks: Option<mush_agent::LifecycleHooks>,
    cwd: Option<PathBuf>,
) -> tokio::task::JoinHandle<CompactionTaskResult> {
    let before_count = messages.len();
    tokio::spawn(async move {
        let (compacted, tokens_before, tokens_after, summarised_count) = run_compaction(
            messages,
            &model,
            &options,
            &registry,
            lifecycle_hooks.as_ref(),
            cwd.as_deref(),
        )
        .await;
        CompactionTaskResult {
            messages: compacted,
            tokens_before,
            tokens_after,
            summarised_count,
            before_count,
        }
    })
}

/// finalise a compaction task: swap the conversation messages so future
/// API calls see the compacted context, append a "compacted summary"
/// system banner to the visible chat (without nuking the existing
/// scrollback), reset the "recent live call" token stats so the next
/// real call doesn't trip a false ContextDecrease anomaly, and update
/// the status bar. `kind` controls only the status text, since the
/// underlying state mutation is identical for /compact and
/// /fork-compact.
///
/// the previous implementation rebuilt the display from the compacted
/// conversation, which silently wiped every pre-compact user/assistant
/// exchange the user could see. now the LLM-facing conversation diverges
/// from the visible scrollback at the moment of compaction: app.messages
/// keeps everything that was there, with a single summary banner appended
/// so the user can scroll back through the full history at any time
pub fn apply_compaction_result(
    app: &mut App,
    conversation: &mut ConversationState,
    result: CompactionTaskResult,
    kind: CompactionKind,
) {
    let summary_body = extract_fresh_summary(&result.messages);

    conversation.replace_messages(result.messages);
    let after_count = conversation.context_len();
    app.stats
        .reset_live_state(TokenCount::new(result.tokens_after as u64));

    if let Some(body) = summary_body {
        app.push_system_message(crate::conversation_display::format_compaction_banner(&body));
    }

    app.status = Some(match kind {
        CompactionKind::Compact => format!(
            "compacted: {} → {} messages, ~{} → ~{} tokens ({} summarised)",
            result.before_count,
            after_count,
            result.tokens_before,
            result.tokens_after,
            result.summarised_count,
        ),
        CompactionKind::ForkCompact => format!(
            "fork-compacted: {} → {} messages, ~{} → ~{} tokens (original preserved, /tree to navigate)",
            result.before_count, after_count, result.tokens_before, result.tokens_after,
        ),
    });
}

/// run LLM compaction on the conversation synchronously. kept as a thin
/// wrapper over [`start_compaction`] + [`apply_compaction_result`] so
/// existing tests and any synchronous callers (e.g. legacy paths,
/// integration tests) keep working unchanged.
pub async fn handle_compact(
    app: &mut App,
    conversation: &mut ConversationState,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
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
    );
    let Ok(result) = task.await else {
        app.push_system_message("compaction task failed");
        return;
    };
    apply_compaction_result(app, conversation, result, CompactionKind::Compact);
}

/// fork the session tree then compact the new branch
///
/// the original conversation is preserved in the parent branch.
/// a summary of the old branch is injected at the fork point so
/// the LLM knows the branch happened.
///
/// thin wrapper around [`fork_and_compact`] that updates the visible
/// chat and status bar. only used by tests today; production code goes
/// through [`start_compaction`] + [`apply_compaction_result`] via the
/// pending-compaction queue
pub async fn handle_fork_compact(
    app: &mut App,
    conversation: &mut ConversationState,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) {
    let before = conversation.context_len();
    if before <= MIN_MESSAGES_FOR_COMPACTION {
        app.push_system_message("conversation too short to fork-compact");
        return;
    }

    app.status = Some("fork-compacting…".into());
    let result = fork_and_compact(
        conversation,
        "forked",
        model,
        options,
        registry,
        lifecycle_hooks,
        cwd,
    )
    .await;
    match result {
        Some((after, tokens_before, tokens_after)) => {
            // append the summary to the visible chat (mirrors the
            // /compact path; preserves the user's scrollback)
            if let Some(body) = extract_fresh_summary(&conversation.context()) {
                app.push_system_message(crate::conversation_display::format_compaction_banner(
                    &body,
                ));
            }
            app.stats
                .reset_live_state(TokenCount::new(tokens_after as u64));
            app.status = Some(format!(
                "fork-compacted: {before} → {after} messages, ~{tokens_before} → ~{tokens_after} tokens (original preserved, /tree to navigate)",
            ));
        }
        None => app.push_system_message("no conversation to fork"),
    }
}

/// fork the session tree at the current leaf and compact the new branch.
/// returns (after_count, tokens_before, tokens_after) or None if no leaf.
pub async fn fork_and_compact(
    conversation: &mut ConversationState,
    label: &str,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> Option<(usize, usize, usize)> {
    let messages = conversation.context();
    let before = messages.len();

    let leaf_id = conversation.leaf_id().cloned()?;

    conversation.branch_with_summary(
        &leaf_id,
        format!("{label} from branch with {before} messages for compaction"),
    );

    let (compacted_messages, tokens_before, tokens_after, _) =
        run_compaction(messages, model, options, registry, lifecycle_hooks, cwd).await;

    conversation.replace_messages(compacted_messages);
    Some((conversation.context_len(), tokens_before, tokens_after))
}

/// shared compaction + hook logic for /compact, /fork-compact, and auto-fork-compact
///
/// returns (compacted_messages, tokens_before, tokens_after, summarised_count)
pub async fn run_compaction(
    messages: Vec<Message>,
    model: &Model,
    options: &StreamOptions,
    registry: &ApiRegistry,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> (Vec<Message>, usize, usize, usize) {
    use mush_session::compact;

    let tokens_before = compact::estimate_tokens(&messages);
    let result = compact::llm_compact(messages, registry, model, options, Some(10)).await;
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
    (
        compacted,
        tokens_before,
        tokens_after,
        result.summarised_count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use mush_ai::types::{
        Api, AssistantContentPart, AssistantMessage, Provider, StopReason, TextContent, Timestamp,
        TokenCount, Usage, UserContent, UserMessage,
    };

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
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContentPart::Text(TextContent {
                text: text.into(),
            })],
            model: test_model().id,
            provider: Provider::Anthropic,
            api: Api::AnthropicMessages,
            usage: Usage {
                input_tokens: TokenCount::new(input_tokens),
                output_tokens: TokenCount::new(20),
                cache_read_tokens: TokenCount::ZERO,
                cache_write_tokens: TokenCount::ZERO,
            },
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        })
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
        let model = test_model();
        let options = StreamOptions::default();
        let registry = ApiRegistry::new();

        let task = start_compaction(conversation.context(), model, options, registry, None, None);
        let result = task.await.expect("compaction task panicked");

        let mut conv = conversation;
        apply_compaction_result(&mut app, &mut conv, result, CompactionKind::Compact);

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

    /// `CompactionKind::ForkCompact` must format status text that hints
    /// at the preserved original branch. without this, /fork-compact
    /// would just look like /compact in the status bar and the user
    /// would have no clue they could `/tree` back to the pre-compact
    /// state.
    #[tokio::test]
    async fn apply_compaction_result_formats_fork_status() {
        let mut msgs = Vec::new();
        for i in 0..12 {
            msgs.push(user_msg(&format!("q {i}")));
            msgs.push(assistant_msg(&format!("a {i}"), 100));
        }
        let mut conversation = ConversationState::from_messages(msgs.clone());
        let mut app = App::new(test_model().id, TokenCount::new(200_000));
        let registry = ApiRegistry::new();

        let task = start_compaction(
            conversation.context(),
            test_model(),
            StreamOptions::default(),
            registry,
            None,
            None,
        );
        let result = task.await.expect("compaction task panicked");

        apply_compaction_result(
            &mut app,
            &mut conversation,
            result,
            CompactionKind::ForkCompact,
        );

        let status = app.status.as_deref().unwrap_or_default();
        assert!(
            status.starts_with("fork-compacted:"),
            "expected fork-specific status prefix, got: {status:?}"
        );
        assert!(
            status.contains("/tree"),
            "fork status should hint at /tree navigation, got: {status:?}"
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

        // last message must be the compaction summary banner
        let last = app.messages.last().expect("at least one message");
        assert_eq!(last.role, crate::app::MessageRole::System);
        assert!(
            last.content.contains("compacted summary"),
            "expected last message to be the summary banner, got: {:?}",
            last.content
        );
    }
}
