use mush_ai::registry::ApiRegistry;
use mush_ai::types::{Model, StreamOptions};
use mush_session::harness::{self, RefinementResult, Scope, State};
use mush_session::refine::{Plan, RefineRequest};

use crate::session::SessionRuntime;

impl SessionRuntime {
    #[must_use]
    pub fn harness_enabled(&self) -> bool {
        self.saver.is_some()
    }

    pub fn deliver_pending_harness_digest(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.harness_digest_pending) {
            return None;
        }
        self.refresh_harness_digest()
    }

    fn refresh_harness_digest(&mut self) -> Option<String> {
        if !self.harness_enabled() {
            return None;
        }
        let merged = harness::load_merged(&self.session_id);
        if merged.is_empty() {
            return None;
        }
        let current = merged.fingerprint();
        if harness::latest_digest_fingerprint(&self.conversation.context()).as_deref()
            == Some(&current)
        {
            return None;
        }
        let digest = merged.digest();
        self.conversation
            .append_message(harness::digest_message(&merged));
        Some(digest)
    }

    pub fn refresh_notes(&mut self) -> Option<String> {
        let latest = crate::notes::latest_fingerprint(&self.conversation.context());
        let delivery = self.notes.as_ref()?.delivery(latest.as_deref())?;
        self.conversation.append_message(delivery.message);
        Some(delivery.text)
    }

    #[must_use]
    pub fn refinement_history(&self) -> Vec<RefinementResult> {
        let mut history = harness::load_history(&harness::global_dir());
        history.extend(harness::load_history(&harness::local_dir(&self.session_id)));
        history
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn plan_refinement(
    messages: Vec<mush_ai::types::Message>,
    session_id: mush_ai::types::SessionId,
    history: Vec<RefinementResult>,
    registry: ApiRegistry,
    model: Model,
    options: StreamOptions,
    request: RefineRequest,
    session_thinking: mush_ai::types::ThinkingLevel,
) -> Result<Plan, String> {
    let merged = harness::load_merged(&session_id);
    mush_session::refine::plan(
        &messages,
        &merged,
        &history,
        &registry,
        &model,
        &options,
        &request,
        &session_thinking,
    )
    .await
}

pub fn apply_plan(
    session_id: &mush_ai::types::SessionId,
    plan: &Plan,
) -> Result<RefinementResult, String> {
    let dir = match plan.scope {
        Scope::Global => harness::global_dir(),
        Scope::Local => harness::local_dir(session_id),
    };
    let mut state = State::load(&dir, plan.scope);
    let result = state.apply(
        &plan.proposal,
        &plan.id,
        plan.scope,
        plan.rollback_of.as_deref(),
    );
    state
        .save(&dir)
        .map_err(|error| format!("could not save the harness state: {error}"))?;
    harness::append_history(&dir, &result)
        .map_err(|error| format!("could not record the refinement: {error}"))?;
    Ok(result)
}
