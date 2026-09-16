use std::path::{Path, PathBuf};

use mush_ai::registry::ApiRegistry;
use mush_ai::types::{Message, Model, StreamOptions};
use mush_session::compact::{
    CompactionDiagnostics, CompactionFailureReport, CompactionFocus, CompactionResult,
};

use crate::session::SessionRuntime;

#[must_use]
pub fn extract_compaction_summary(text: &str) -> Option<String> {
    let rest = text.strip_prefix(mush_session::COMPACTION_SUMMARY_PREFIX)?;
    let rest = rest.trim_start();
    let inner = rest
        .strip_prefix("<summary>")
        .and_then(|s| s.strip_suffix("</summary>"))
        .unwrap_or(rest);
    Some(inner.trim().to_string())
}

pub fn extract_fresh_summary(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|m| match m {
        Message::User(u) => extract_compaction_summary(&u.text()),
        _ => None,
    })
}

pub const MIN_MESSAGES_FOR_COMPACTION: usize = 4;

#[derive(Debug)]
pub enum CompactionTaskResult {
    Applied(AppliedCompaction),
    Failed {
        failure: CompactionFailureReport,
        tokens_before: usize,
        model_id: String,
    },
}

#[derive(Debug)]
pub struct AppliedCompaction {
    pub messages: Vec<Message>,
    pub summary: String,
    pub tokens_before: usize,
    pub tokens_after: usize,
    pub summarised_count: usize,
    pub before_count: usize,
    pub model_id: String,
    pub diagnostics: CompactionDiagnostics,
    pub instructions: Option<String>,
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
    focus: CompactionFocus,
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
            focus,
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

pub async fn fallback_applied_compaction(
    failure: CompactionFailureReport,
    tokens_before: usize,
    model_id: String,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&Path>,
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
        failure: CompactionFailureReport,
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
    cwd: Option<&Path>,
    focus: CompactionFocus,
) -> RunCompactionResult {
    use mush_session::compact;

    let tokens_before = compact::estimate_tokens(&messages);
    let instructions = focus.instructions.clone();
    let result =
        match compact::try_llm_compact(messages, registry, model, options, Some(10), focus).await {
            Ok(result) => result,
            Err(failure) => {
                return RunCompactionResult::Failed {
                    failure,
                    tokens_before,
                };
            }
        };

    let mut applied = build_applied_compaction(
        result,
        tokens_before,
        0,
        String::new(),
        lifecycle_hooks,
        cwd,
    )
    .await;
    applied.instructions = instructions;
    RunCompactionResult::Applied(applied)
}

async fn build_applied_compaction(
    result: CompactionResult,
    tokens_before: usize,
    before_count: usize,
    model_id: String,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&Path>,
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
        instructions: None,
    }
}

impl SessionRuntime {
    pub fn apply_compaction(
        &mut self,
        summary: String,
        summarised_count: usize,
        messages: Vec<Message>,
    ) -> usize {
        self.conversation
            .apply_compaction(summary, summarised_count, messages);
        self.refresh_context_tokens();
        self.conversation.context_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mush_ai::types::{Timestamp, UserContent, UserMessage};

    fn user_msg(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
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
        let result = CompactionResult {
            messages: vec![user_msg("the summary"), user_msg("a kept message")],
            summarised_count: 4,
            summary: "the summary".into(),
            diagnostics: CompactionDiagnostics::default(),
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

    #[test]
    fn extract_compaction_summary_strips_the_prefix_and_tags() {
        let text = format!(
            "{}<summary>kept the gist</summary>",
            mush_session::COMPACTION_SUMMARY_PREFIX
        );

        assert_eq!(
            extract_compaction_summary(&text).as_deref(),
            Some("kept the gist")
        );
        assert_eq!(extract_compaction_summary("plain text"), None);
    }
}
