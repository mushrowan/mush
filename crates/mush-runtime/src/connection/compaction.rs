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
        mush_session::compact::needs_compaction_for_messages(
            self.runtime.context_tokens,
            model.compaction_window(),
            &self.runtime.conversation.context(),
            &self
                .config
                .compaction
                .for_session(&model, self.runtime.rlm_depth > 0),
        )
    }

    pub(super) async fn compact_before_cache_expiry(&mut self, now: mush_ai::types::Timestamp) {
        let settings = self.config.compaction;
        if !settings
            .before_cache_expiry
            .applies(self.runtime.rlm_depth > 0)
            || !self
                .runtime
                .auto_compact
                .load(std::sync::atomic::Ordering::Relaxed)
            || self.runtime.context_tokens.get() < settings.before_cache_expiry_min_tokens
        {
            return;
        }
        match self.runtime.prompt_cache.expiry(now) {
            crate::prompt_cache::Expiry::Due => {}
            crate::prompt_cache::Expiry::Expired => {
                self.runtime.prompt_cache.forget();
                return;
            }
            crate::prompt_cache::Expiry::Unknown | crate::prompt_cache::Expiry::Warm => return,
        }
        if !self.quiet_for_compaction().await {
            return;
        }
        self.runtime.prompt_cache.forget();
        let messages = self.runtime.conversation.context();
        if messages.len() <= crate::compaction::MIN_MESSAGES_FOR_COMPACTION {
            return;
        }
        let target = self.runtime.session_compaction_target(&self.config).await;
        match self
            .spawn_compaction_on(messages, CompactionFocus::default(), target)
            .await
        {
            Ok(()) => {
                self.compaction_quiet = true;
                self.queued.push_back(super::status_event(
                    "compacting while the prompt cache is still warm",
                ));
            }
            Err(error) => tracing::debug!(%error, "no compaction before the prompt cache expired"),
        }
    }

    async fn quiet_for_compaction(&self) -> bool {
        !self.closed
            && self.idle().await
            && self.deferred_deliveries.is_empty()
            && self.deferred_events.is_empty()
            && self.side_questions.is_empty()
            && self.branch_summary.is_none()
            && self.busy_reply.is_none()
    }

    pub(super) fn try_poll_compaction(&mut self) -> Option<ClientEvent> {
        let finished = take_finished(&mut self.compaction)?;
        if std::mem::take(&mut self.compaction_quiet) {
            return self.finish_quiet_compaction(finished.ok()?);
        }
        let result = finished.ok()?;
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

    fn finish_quiet_compaction(&mut self, result: CompactionTaskResult) -> Option<ClientEvent> {
        if let CompactionTaskResult::Failed { failure, .. } = &result {
            tracing::warn!(
                reason = failure.reason(),
                "the compaction before the prompt cache expired failed, the context stays as it was"
            );
            return None;
        }
        match self.finish_compaction(result) {
            event @ ClientEvent::Compacted(_) => Some(event),
            other => {
                tracing::info!(
                    ?other,
                    "the compaction before the prompt cache expired was not kept"
                );
                None
            }
        }
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
        self.spawn_compaction_on(messages, focus, target).await
    }

    async fn spawn_compaction_on(
        &mut self,
        messages: Vec<Message>,
        focus: CompactionFocus,
        target: crate::turn::CompactionTarget,
    ) -> Result<(), ConnectionError> {
        if self.compaction.is_some() {
            return Err(ConnectionError::Compacting);
        }
        self.compaction_auto = false;
        self.compaction_quiet = false;
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
