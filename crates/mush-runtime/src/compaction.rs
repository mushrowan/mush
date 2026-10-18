use std::path::{Path, PathBuf};

use mush_ai::registry::ApiRegistry;
use mush_ai::types::{Message, Model, StreamOptions};
use mush_session::compact::{
    CompactionDiagnostics, CompactionFailureReport, CompactionFocus, CompactionResult,
};

use crate::session::SessionRuntime;

#[must_use]
pub fn extract_compaction_summary(text: &str) -> Option<String> {
    let rest = mush_ai::types::strip_compaction_summary_prefix(text)?;
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    live: Option<mush_session::compact::LiveContext>,
    settings: mush_session::compact::CompactionSettings,
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
            live.as_ref(),
            &settings,
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
    live: Option<&mush_session::compact::LiveContext>,
    settings: &mush_session::compact::CompactionSettings,
) -> RunCompactionResult {
    use mush_session::compact;

    let tokens_before = compact::estimate_tokens(&messages);
    let instructions = focus.instructions.clone();
    let keep = settings.keep_recent(&messages);
    let result = match compact::try_llm_compact(
        messages,
        registry,
        model,
        options,
        Some(keep),
        focus,
        live,
        settings,
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
    run_post_compaction_hooks(lifecycle_hooks, cwd).await;
    AppliedCompaction {
        tokens_after: mush_session::compact::estimate_tokens(&result.messages),
        messages: result.messages,
        summary: result.summary,
        tokens_before,
        summarised_count: result.summarised_count,
        before_count,
        model_id,
        diagnostics: result.diagnostics,
        instructions: None,
    }
}

pub(crate) async fn run_post_compaction_hooks(
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&Path>,
) {
    let Some(hooks) = lifecycle_hooks else {
        return;
    };
    if hooks
        .for_point(mush_agent::HookPoint::PostCompaction)
        .is_empty()
    {
        return;
    }
    for result in hooks
        .run_all(mush_agent::HookPoint::PostCompaction, cwd)
        .await
    {
        if !result.success {
            tracing::warn!(command = %result.command, "post-compaction hook failed: {}", result.output);
        }
    }
}

impl SessionRuntime {
    pub fn apply_compaction(
        &mut self,
        summary: String,
        summarised_count: usize,
        messages: &[Message],
    ) -> Option<usize> {
        self.conversation
            .apply_compaction(summary, summarised_count, messages)
            .ok()?;
        self.prompt_cache.forget();
        self.harness_digest_pending = true;
        self.refresh_context_tokens();
        Some(self.conversation.context_len())
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
        assert_eq!(
            extract_compaction_summary(&mush_ai::types::compaction_summary_text("new form"))
                .as_deref(),
            Some("new form")
        );
        assert_eq!(
            extract_compaction_summary(
                "The conversation history before this point was compacted into the following \
                 summary:\n\n<summary>\nold form\n</summary>"
            )
            .as_deref(),
            Some("old form")
        );
    }
}
