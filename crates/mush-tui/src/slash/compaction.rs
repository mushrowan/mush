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

    app.status = Some("compacting...".into());
    let (compacted_messages, tokens_before, tokens_after, summarised_count) =
        run_compaction(messages, model, options, registry, lifecycle_hooks, cwd).await;

    conversation.replace_messages(compacted_messages);
    let after = conversation.context();
    rebuild_display(app, &after);
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

    app.status = Some("fork-compacting...".into());
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
