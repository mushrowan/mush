use mush_ai::types::Message;
use mush_session::compact::CompactionFocus;

use super::{
    ClientEvent, ConnectionError, ContextUsage, InProcessConnection, failed_event, take_finished,
};
use crate::compaction::CompactionTaskResult;

pub(super) struct FailedCompaction {
    failure: mush_session::compact::CompactionFailureReport,
    tokens_before: usize,
    model_id: String,
}

impl InProcessConnection {
    pub(super) async fn deliver_kernel_compaction_notice(&mut self) {
        if self.turn_active() || !std::mem::take(&mut self.kernel_compaction_notice_due) {
            return;
        }
        if let Some(notice) =
            crate::kernel_notice::after_compaction(self.runtime.kernel_state.as_deref()).await
        {
            self.runtime.conversation.append_message(notice);
            self.runtime.save_now_and_settle();
        }
    }

    pub(super) fn fork_compaction_due(&self) -> bool {
        if !self.config.auto_fork_compact
            || !self
                .runtime
                .auto_compact
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return false;
        }
        let model = self.runtime.model(&self.config.default_model);
        let reply = mush_ai::providers::effective_max_output_tokens(&model, &self.config.options);
        mush_session::compact::needs_compaction_for_messages(
            self.runtime.context_tokens,
            model.compaction_window(),
            &self.runtime.conversation.context(),
            &self.config.compaction.leaving_room_for(reply),
        )
    }

    pub(super) fn try_poll_compaction(&mut self) -> Option<ClientEvent> {
        let result = take_finished(&mut self.compaction)?.ok()?;
        if self.compaction_auto
            && let CompactionTaskResult::Failed {
                failure,
                tokens_before,
                model_id,
            } = result
        {
            let hooks = self.injections.lifecycle_hooks.clone();
            let cwd = self.injections.cwd.clone();
            self.compaction = Some(tokio::spawn(async move {
                CompactionTaskResult::Applied(
                    crate::compaction::fallback_applied_compaction(
                        failure,
                        tokens_before,
                        model_id,
                        Some(&hooks),
                        cwd.as_deref(),
                    )
                    .await,
                )
            }));
            return None;
        }
        Some(self.finish_compaction(result))
    }

    fn finish_compaction(&mut self, result: CompactionTaskResult) -> ClientEvent {
        match result {
            CompactionTaskResult::Applied(applied) => {
                if self
                    .runtime
                    .apply_compaction(
                        applied.summary.clone(),
                        applied.summarised_count,
                        &applied.messages,
                    )
                    .is_none()
                {
                    return failed_event(
                        "compaction discarded, the conversation changed while it ran",
                    );
                }
                if let Some(commands) = &self.host_commands {
                    commands.record_context(ContextUsage {
                        tokens: None,
                        ..commands.context_usage()
                    });
                }
                self.runtime.save_now_and_settle();
                self.compact_refine_pending = true;
                self.kernel_compaction_notice_due = true;
                ClientEvent::Compacted(applied)
            }
            CompactionTaskResult::Failed {
                failure,
                tokens_before,
                model_id,
            } => {
                self.failed_compaction = Some(FailedCompaction {
                    failure: failure.clone(),
                    tokens_before,
                    model_id: model_id.clone(),
                });
                ClientEvent::CompactionFailed {
                    failure,
                    tokens_before,
                    model_id,
                }
            }
        }
    }

    pub(super) async fn start_compaction(
        &mut self,
        focus: CompactionFocus,
        auto: bool,
    ) -> Result<(), ConnectionError> {
        let messages = self.runtime.conversation.context();
        if messages.len() <= crate::compaction::MIN_MESSAGES_FOR_COMPACTION {
            return Err(ConnectionError::TooShortToCompact);
        }
        self.spawn_compaction(messages, focus).await?;
        self.compaction_auto = auto;
        Ok(())
    }

    pub(super) async fn retry_compaction(&mut self) -> Result<(), ConnectionError> {
        let failed = self
            .failed_compaction
            .take()
            .ok_or(ConnectionError::NothingToRecover)?;
        let messages = failed.failure.original_messages().to_vec();
        let focus = failed.failure.focus().clone();
        self.spawn_compaction(messages, focus).await
    }

    pub(super) async fn fallback_compaction(&mut self) -> Result<ClientEvent, ConnectionError> {
        let failed = self
            .failed_compaction
            .take()
            .ok_or(ConnectionError::NothingToRecover)?;
        let applied = crate::compaction::fallback_applied_compaction(
            failed.failure,
            failed.tokens_before,
            failed.model_id,
            Some(&self.injections.lifecycle_hooks),
            self.injections.cwd.as_deref(),
        )
        .await;
        Ok(self.finish_compaction(CompactionTaskResult::Applied(applied)))
    }

    async fn spawn_compaction(
        &mut self,
        messages: Vec<Message>,
        focus: CompactionFocus,
    ) -> Result<(), ConnectionError> {
        if self.compaction.is_some() {
            return Err(ConnectionError::Compacting);
        }
        self.compaction_auto = false;
        let target = self
            .runtime
            .compaction_target(&self.config)
            .await
            .fitting(|| {
                mush_session::compact::summary_request_tokens(
                    &messages,
                    self.config.compaction.keep_recent(&messages),
                    &focus,
                    &self.config.compaction,
                )
            });
        let pane_model = self.runtime.model(&self.config.default_model);
        let live = target.uses_session_model().then(|| {
            let tools = crate::turn::aligned_tools(
                &self.tools,
                &pane_model,
                &self.config,
                self.injections.cwd.clone().unwrap_or_default(),
            );
            mush_session::compact::LiveContext {
                system_prompt: self.runtime.system_prompt.clone(),
                tools: mush_agent::tool_definitions(&tools, pane_model.api),
            }
        });
        let task = crate::compaction::start_compaction(
            messages,
            target.model,
            target.options,
            self.registry.clone(),
            Some(self.injections.lifecycle_hooks.clone()),
            self.injections.cwd.clone(),
            focus,
            live,
            self.config.compaction,
        );
        self.compaction = Some(task);
        self.queued.push_back(ClientEvent::Compacting);
        Ok(())
    }
}
