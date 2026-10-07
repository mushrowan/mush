use super::{
    ClientEvent, ConnectionError, InProcessConnection, take_finished, take_finished_keyed,
};
use crate::commands::SessionEvent;

impl InProcessConnection {
    pub(super) async fn start_refine(
        &mut self,
        request: mush_session::refine::RefineRequest,
    ) -> Result<(), ConnectionError> {
        if !self.runtime.harness_enabled() {
            return Err(ConnectionError::Refused(
                "the continual harness needs a saved session (not --no-session)".into(),
            ));
        }
        if self.refine.is_some() {
            return Err(ConnectionError::Refused(
                "a refinement is already running".into(),
            ));
        }
        let target = self.runtime.compaction_target(&self.config).await;
        let messages = self.runtime.conversation.context();
        let history = self.runtime.refinement_history();
        let session_id = self.runtime.session_id.clone();
        let registry = self.registry.clone();
        let rollback = request.rollback.is_some();
        let thinking = self.runtime.thinking_level.clone();
        self.refine = Some(tokio::spawn(crate::refine::plan_refinement(
            messages,
            session_id,
            history,
            registry,
            target.model,
            target.options,
            request,
            thinking,
        )));
        self.queued
            .push_back(ClientEvent::Session(SessionEvent::Status(if rollback {
                "rolling the harness back…".into()
            } else {
                "refining the harness…".into()
            })));
        Ok(())
    }

    fn auto_refine_due(&self) -> Option<mush_session::refine::AutoRefineReason> {
        use mush_session::refine::AutoRefineReason;
        let settings = self.config.auto_refine;
        if !settings.enabled
            || self.runtime.rlm_depth > 0
            || !self.runtime.harness_enabled()
            || self.turn_active()
            || self.refine.is_some()
            || self.auto_review.is_some()
        {
            return None;
        }
        let cooling = self
            .last_auto_review
            .is_some_and(|at| at.elapsed() < settings.cooldown());
        if cooling {
            return None;
        }
        if self.compact_refine_pending && settings.compact {
            return Some(AutoRefineReason::Compact);
        }
        (self.assistant_turns_since_review >= settings.turn_interval)
            .then_some(AutoRefineReason::TurnInterval)
    }

    pub(super) async fn maybe_auto_refine(&mut self) {
        let Some(reason) = self.auto_refine_due() else {
            return;
        };
        self.compact_refine_pending = false;
        self.last_auto_review = Some(std::time::Instant::now());
        let turns = std::mem::take(&mut self.assistant_turns_since_review);
        let target = self.runtime.compaction_target(&self.config).await;
        let messages = self.runtime.conversation.context();
        let history = self.runtime.refinement_history();
        let merged = mush_session::harness::load_merged(&self.runtime.session_id);
        let registry = self.registry.clone();
        let thinking = self.runtime.thinking_level.clone();
        self.auto_review = Some((
            reason,
            tokio::spawn(async move {
                mush_session::refine::review(
                    &messages,
                    &merged,
                    &history,
                    &registry,
                    &target.model,
                    &target.options,
                    reason,
                    turns,
                    &thinking,
                )
                .await
            }),
        ));
    }

    pub(super) fn try_poll_auto_review(&mut self) -> Option<ClientEvent> {
        let (reason, review) = take_finished_keyed(&mut self.auto_review)?;
        let review = match review {
            Ok(Ok(review)) => review,
            Ok(Err(error)) => {
                tracing::warn!(%error, "auto-refine review failed");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, "auto-refine review task failed");
                return None;
            }
        };
        if !review.should_refine {
            tracing::info!(rationale = %review.rationale, "auto-refine review declined");
            return None;
        }
        let request = review.refine_request(reason);
        self.host_commands
            .get_or_insert_with(Default::default)
            .replace_refine(request);
        Some(ClientEvent::Session(SessionEvent::Status(
            "auto-refine review approved, refining after this turn…".into(),
        )))
    }

    pub(super) fn try_poll_refine(&mut self) -> Option<ClientEvent> {
        if self.turn_active() {
            return None;
        }
        let outcome = match take_finished(&mut self.refine)? {
            Ok(outcome) => outcome,
            Err(error) => Err(format!("refine task failed: {error}")),
        };
        let result = outcome.and_then(|plan| {
            crate::refine::apply_plan(&self.runtime.session_id, &plan)
                .map(|result| (plan.source, result))
        });
        Some(match result {
            Ok((source, result)) => {
                self.runtime
                    .conversation
                    .append_refinement(source, result.clone());
                self.runtime.save_now_and_settle();
                ClientEvent::Refined(Box::new(result))
            }
            Err(error) => ClientEvent::Session(SessionEvent::Failed(format!("refine: {error}"))),
        })
    }
}
