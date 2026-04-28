//! compaction-related slash command implementations
//!
//! split out from commands.rs due to the async complexity
//! and distinct concerns (LLM calls, hook execution)

use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_session::ConversationState;

use crate::app::App;

use super::commands::rebuild_display;

/// minimum messages before compaction is worthwhile
const MIN_MESSAGES_FOR_COMPACTION: usize = 4;

/// run LLM compaction on the conversation
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
    let (compacted_messages, tokens_before, tokens_after, summarised_count) =
        run_compaction(messages, model, options, registry, lifecycle_hooks, cwd).await;

    conversation.replace_messages(compacted_messages);
    let after = conversation.context();
    rebuild_display(app, &after);
    // rebuild_display walks the surviving assistants and accumulates
    // their historical usage into app.stats. wipe the resulting
    // "live" fields so the token counter shows the real post-compact
    // size and the next call doesn't trip a false ContextDecrease.
    app.stats
        .reset_live_state(TokenCount::new(tokens_after as u64));
    // the compaction summary is at the top of the rebuilt view; jump
    // there so the user actually sees the new summary instead of
    // staying at the bottom amid the same kept messages
    app.scroll_to_top();
    app.status = Some(format!(
        "compacted: {before} → {} messages, ~{tokens_before} → ~{tokens_after} tokens ({summarised_count} summarised)",
        after.len(),
    ));
}

/// fork the session tree then compact the new branch
///
/// the original conversation is preserved in the parent branch.
/// a summary of the old branch is injected at the fork point so
/// the LLM knows the branch happened.
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
            rebuild_display(app, &conversation.context());
            app.stats
                .reset_live_state(TokenCount::new(tokens_after as u64));
            app.scroll_to_top();
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

    /// after a manual /compact, the user is left at the BOTTOM of the
    /// conversation by default (rebuild_display calls clear_messages
    /// which resets scroll_offset to 0). the compaction summary lives
    /// at index 0 (top), so without scrolling the user sees the same
    /// kept messages they were already looking at and concludes
    /// nothing happened. fix: scroll the view to the top after a
    /// manual compaction so the freshly generated summary is the
    /// first thing visible.
    #[tokio::test]
    async fn handle_compact_scrolls_to_show_summary() {
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

        // simulate the user at the bottom of the conversation
        app.scroll_offset = 0;

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
            app.scroll_offset > 0,
            "expected scroll to move toward the top so the summary is visible, was {}",
            app.scroll_offset
        );
    }
}
