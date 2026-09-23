use mush_ai::registry::{ApiRegistry, LlmContext};
use mush_ai::types::{
    Message, Model, StreamOptions, ThinkingLevel, Timestamp, TokenCount, UserContent, UserMessage,
};

use crate::compact::{complete_text, serialize_conversation};
use crate::harness::{self, Proposal, RefinementResult, Scope, State};

const MAX_OUTPUT_TOKENS: u64 = 32_000;
const CONVERSATION_CHARS: usize = 80_000;
const CHARS_PER_TOKEN: usize = 4;

const SYSTEM_PROMPT: &str = "\
You are mush's /refine continual harness subsystem.

Your job is to improve the editable continual harness state from the current trajectory. \
This is similar in spirit to context compaction, but instead of summarising the conversation \
you emit precise create, update or delete edits to reusable state. The continual harness is \
the persistent, editable set of prompt notes, memories, skills and subagent specs that lets \
mush improve reusable behaviour outside the token history.

Continual harness components:
- prompt: supplemental prompt notes only. The base system prompt is immutable and MUST NOT be rewritten.
- memory: durable facts, decisions, failures, preferences and outcomes.
- skill: a python call available in the kernel. Skill create/update edits MUST include a \
`reference` object with `{\"type\":\"python\"}`, a python import and a callable or call pattern, \
and an `arguments` object describing accepted inputs, required fields, defaults and constraints. \
Use `{}` for `arguments` only when the callable needs no inputs. Include the call form `await <import>.<callable>(...)`.
- subagent: reusable delegation specs with purpose, instructions and when to invoke. Include the call form: \
compose a concise task prompt and spawn with `handle = await rlm.spawn(\"sub-task\", name=\"worker\")`; \
the handle returns immediately, never the child's answer. Results arrive as agent messages or files; \
children reply with `await agent_message.send(message, receiver_role=\"parent\")`. Do not invent wrappers.

Scope and persistence policy:
- The default store is local to the current session. Use it for session-specific progress, active task \
state, coordination notes, temporary blockers and project facts that should not affect other sessions.
- A caller may request global refinement. Global edits must be stable cross-session lessons, durable user \
preferences, reusable skills or subagents, or tool and environment facts that should affect future sessions.
- Entry ids in the overview may carry a display-only `local:` or `global:` prefix. Always use the bare id in edits.
- All edits in one refinement apply to the requested scope's store only. During a local refinement global \
entries are read-only context: never propose update or delete edits for them, create a local entry instead.
- Project-specific lessons may be persisted globally only when the title, path or content names the project \
and the lesson is likely to be reused for it.
- Use memory for declarative facts and preferences, skill for repeatable procedures exposed as python calls, \
prompt for narrow behavioural policy addendums, and subagent for reusable delegation roles.
- Create or update the smallest relevant component.

Use the trajectory, the current harness state and the prior refinement history. Prefer small \
evidence-backed edits. If prior refinements caused issues, roll back or replace the faulty entries. \
Never edit source files directly. Output JSON only with this exact shape:

{
  \"summary\": \"one sentence\",
  \"rationale\": \"why these edits are justified by trajectory evidence\",
  \"expectedOutcome\": \"what should improve and how to validate it\",
  \"edits\": [
    {
      \"action\": \"create|update|delete\",
      \"kind\": \"prompt|memory|skill|subagent\",
      \"id\": \"stable id for update/delete, optional for create\",
      \"title\": \"required for create/update\",
      \"content\": \"required for create/update\",
      \"path\": \"optional grouping path\",
      \"reference\": {\"type\": \"python\", \"import\": \"package.module\", \"callable\": \"function_name\", \"call_pattern\": \"await function_name(...)\"},
      \"arguments\": {\"name\": {\"type\": \"string\", \"required\": true, \"description\": \"accepted input\"}},
      \"metadata\": {},
      \"reason\": \"why this edit is useful\"
    }
  ]
}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AutoRefineSettings {
    pub enabled: bool,
    pub turn_interval: u32,
    pub compact: bool,
    pub cooldown_ms: u64,
}

impl Default for AutoRefineSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            turn_interval: 25,
            compact: true,
            cooldown_ms: 20 * 60_000,
        }
    }
}

impl AutoRefineSettings {
    #[must_use]
    pub fn cooldown(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.cooldown_ms)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefineRequest {
    pub instructions: Option<String>,
    pub rollback: Option<String>,
    pub global: bool,
}

impl RefineRequest {
    #[must_use]
    pub fn scope(&self) -> Scope {
        if self.global {
            Scope::Global
        } else {
            Scope::Local
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub id: String,
    pub proposal: Proposal,
    pub rollback_of: Option<String>,
    pub scope: Scope,
}

#[must_use]
pub fn auxiliary_thinking(model: &Model, session: &ThinkingLevel) -> Option<ThinkingLevel> {
    if !model.reasoning || *session == ThinkingLevel::Off {
        return None;
    }
    let preferred = match session {
        ThinkingLevel::Minimal => ThinkingLevel::Minimal,
        _ => ThinkingLevel::Low,
    };
    Some(model.clamp_thinking(&preferred)).filter(|level| *level != ThinkingLevel::Off)
}

fn output_budget(model: &Model, thinking: Option<&ThinkingLevel>, cap: u64) -> TokenCount {
    let ceiling = model.max_output_tokens.get();
    TokenCount::new(if thinking.is_some() {
        ceiling
    } else {
        cap.min(ceiling)
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn plan(
    messages: &[Message],
    merged: &State,
    history: &[RefinementResult],
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    request: &RefineRequest,
    session_thinking: &ThinkingLevel,
) -> Result<Plan, String> {
    let id = harness::new_id();
    if let Some(target_id) = &request.rollback {
        let target = history
            .iter()
            .find(|item| &item.id == target_id)
            .ok_or_else(|| format!("refinement {target_id} not found"))?;
        return Ok(Plan {
            id,
            proposal: Proposal::rollback_of(target),
            rollback_of: Some(target.id.clone()),
            scope: target.scope,
        });
    }
    let conversation = tail(&serialize_conversation(messages), CONVERSATION_CHARS);
    let prompt = user_prompt(&conversation, merged, history, request, model);
    let context = LlmContext {
        system_prompt: Some(SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text(prompt))
                .timestamp_ms(Timestamp::now())
                .build(),
        )],
        tools: Vec::new(),
    };
    let mut options = options.clone();
    let thinking = auxiliary_thinking(model, session_thinking);
    options.max_tokens = Some(output_budget(model, thinking.as_ref(), MAX_OUTPUT_TOKENS));
    options.thinking = thinking;
    let text = complete_text(registry, model, &context, &options).await?;
    let proposal = Proposal::parse(&text)?;
    Ok(Plan {
        id,
        proposal,
        rollback_of: None,
        scope: request.scope(),
    })
}

fn user_prompt(
    conversation: &str,
    merged: &State,
    history: &[RefinementResult],
    request: &RefineRequest,
    model: &Model,
) -> String {
    let scope_policy = if request.global {
        "Requested refinement scope: global. Only propose stable cross-session edits, durable user \
         preferences, reusable skills or subagents, or project-qualified facts that should affect future \
         sessions. Do not persist session-only progress, temporary blockers or current-run coordination globally."
    } else {
        "Requested refinement scope: local. Prefer local edits for current task progress, temporary blockers, \
         current-run coordination and project facts that are not clearly reusable across sessions. Global \
         entries in the overview are read-only context: do not propose update or delete edits for them, \
         create a local entry instead if an override is needed."
    };
    let mut parts = vec![
        format!(
            "<current_harness_state>\n{}\n</current_harness_state>",
            merged.overview_for_plan()
        ),
        format!(
            "<refinement_history>\n{}\n</refinement_history>",
            harness::history_for_plan(history)
        ),
    ];
    let fixed_chars: usize =
        parts.iter().map(String::len).sum::<usize>() + SYSTEM_PROMPT.len() + 2_000;
    let budget_chars = (model.compaction_window().get() as usize)
        .saturating_sub(MAX_OUTPUT_TOKENS as usize)
        .saturating_mul(CHARS_PER_TOKEN)
        .saturating_sub(fixed_chars);
    let conversation = if conversation.len() > budget_chars {
        format!(
            "[earlier conversation omitted to fit the model context]\n{}",
            tail(conversation, budget_chars)
        )
    } else {
        conversation.to_string()
    };
    parts.push(format!("<conversation>\n{conversation}\n</conversation>"));
    parts.push(format!("<scope_policy>\n{scope_policy}\n</scope_policy>"));
    if let Some(instructions) = request
        .instructions
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        parts.push(format!(
            "<user_refine_instructions>\n{instructions}\n</user_refine_instructions>"
        ));
    }
    parts.push(
        "Return only JSON edits. If no useful edit is justified, return an empty edits array with a rationale."
            .into(),
    );
    parts.join("\n\n")
}

const REVIEW_SYSTEM_PROMPT: &str = "\
You are mush's automatic /refine review gate.

Decide whether this checkpoint should run /refine. Auto /refine writes local continual harness \
state by default, so approve when the trajectory contains evidence useful to this session's \
future turns. Reject one-off noise, unsupported hypotheses and transient tool output. Ask for \
global refinement only for durable cross-session lessons or explicitly project-qualified lessons \
likely to be reused in future sessions.

Return JSON only:
{
  \"shouldRefine\": true|false,
  \"rationale\": \"short reason\",
  \"instructions\": \"optional concise instructions for /refine if shouldRefine is true\"
}";

const REVIEW_MAX_OUTPUT_TOKENS: u64 = 4_096;
const REVIEW_CONVERSATION_CHARS: usize = 40_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoRefineReason {
    TurnInterval,
    Compact,
}

impl AutoRefineReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnInterval => "turn_interval",
            Self::Compact => "compact",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AutoRefineReview {
    #[serde(default, alias = "shouldRefine")]
    pub should_refine: bool,
    #[serde(default)]
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl AutoRefineReview {
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let candidate = trimmed
            .find('{')
            .and_then(|start| trimmed.rfind('}').map(|end| &trimmed[start..=end]))
            .ok_or_else(|| "the review returned no json object".to_string())?;
        serde_json::from_str(candidate)
            .map_err(|error| format!("review json did not parse: {error}"))
    }

    #[must_use]
    pub fn refine_request(&self, reason: AutoRefineReason) -> RefineRequest {
        let detail = self
            .instructions
            .as_deref()
            .map(|text| format!("\nReviewer instructions: {text}"))
            .unwrap_or_default();
        RefineRequest {
            instructions: Some(format!(
                "Automatic refine review triggered by {}. Only create, update or delete local harness \
                 entries if there is clear evidence that should help this session continue. Prefer an \
                 empty edits array over speculative or one-off memories. Do not promote anything global \
                 unless explicitly requested. Reviewer rationale: {}{detail}",
                reason.as_str(),
                self.rationale
            )),
            rollback: None,
            global: false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn review(
    messages: &[Message],
    merged: &State,
    history: &[RefinementResult],
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    reason: AutoRefineReason,
    turns_since_review: u32,
    session_thinking: &ThinkingLevel,
) -> Result<AutoRefineReview, String> {
    let conversation = tail(&serialize_conversation(messages), REVIEW_CONVERSATION_CHARS);
    let prompt = [
        format!(
            "<trigger>\n{}; {turns_since_review} assistant turns since the last auto-refine review\n</trigger>",
            reason.as_str()
        ),
        format!(
            "<current_harness_state>\n{}\n</current_harness_state>",
            merged.overview_for_plan()
        ),
        format!(
            "<refinement_history>\n{}\n</refinement_history>",
            harness::history_for_plan(history)
        ),
        format!("<conversation>\n{conversation}\n</conversation>"),
        "Return shouldRefine=true when the trajectory contains evidence useful to this session's \
         future turns. Prefer local harness edits for current task progress, temporary blockers and \
         current-run coordination. Ask for global refinement only for durable cross-session lessons \
         or explicitly project-qualified facts likely to be reused in future sessions."
            .to_string(),
    ]
    .join("\n\n");
    let context = LlmContext {
        system_prompt: Some(REVIEW_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text(prompt))
                .timestamp_ms(Timestamp::now())
                .build(),
        )],
        tools: Vec::new(),
    };
    let mut options = options.clone();
    let thinking = auxiliary_thinking(model, session_thinking);
    options.max_tokens = Some(output_budget(
        model,
        thinking.as_ref(),
        REVIEW_MAX_OUTPUT_TOKENS,
    ));
    options.thinking = thinking;
    let text = complete_text(registry, model, &context, &options).await?;
    AutoRefineReview::parse(&text)
}

fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    text.chars().skip(count - max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_auxiliary_thinking_level_follows_the_session_and_the_model() {
        let mut model = mush_ai::models::all_models()
            .into_iter()
            .find(|model| model.reasoning)
            .unwrap();
        model.supported_thinking_levels = Vec::new();
        assert_eq!(
            auxiliary_thinking(&model, &ThinkingLevel::High),
            Some(ThinkingLevel::Low)
        );
        assert_eq!(
            auxiliary_thinking(&model, &ThinkingLevel::Minimal),
            Some(ThinkingLevel::Minimal)
        );
        assert_eq!(auxiliary_thinking(&model, &ThinkingLevel::Off), None);

        model.supported_thinking_levels = vec![ThinkingLevel::Medium, ThinkingLevel::High];
        assert_eq!(
            auxiliary_thinking(&model, &ThinkingLevel::High),
            Some(ThinkingLevel::Medium)
        );

        model.reasoning = false;
        assert_eq!(auxiliary_thinking(&model, &ThinkingLevel::High), None);
    }

    #[test]
    fn thinking_takes_the_whole_output_budget_and_a_plain_call_takes_the_cap() {
        let mut model = mush_ai::models::all_models().into_iter().next().unwrap();
        model.max_output_tokens = TokenCount::new(64_000);
        assert_eq!(
            output_budget(&model, Some(&ThinkingLevel::Low), MAX_OUTPUT_TOKENS).get(),
            64_000
        );
        assert_eq!(
            output_budget(&model, None, MAX_OUTPUT_TOKENS).get(),
            MAX_OUTPUT_TOKENS
        );
        model.max_output_tokens = TokenCount::new(8_000);
        assert_eq!(output_budget(&model, None, MAX_OUTPUT_TOKENS).get(), 8_000);
    }

    #[test]
    fn the_prompt_carries_state_history_conversation_and_scope() {
        let mut state = State::default();
        state.apply(
            &Proposal {
                summary: "s".into(),
                rationale: "r".into(),
                expected_outcome: "o".into(),
                edits: vec![harness::Edit {
                    action: harness::Action::Create,
                    kind: harness::Kind::Memory,
                    id: None,
                    title: Some("tests".into()),
                    content: Some("run nextest".into()),
                    path: None,
                    reference: None,
                    arguments: None,
                    metadata: None,
                    reason: None,
                }],
            },
            "ref_1",
            Scope::Global,
            None,
        );
        let model = mush_ai::models::all_models().into_iter().next().unwrap();
        let prompt = user_prompt(
            "[0] user: hello",
            &state,
            &[],
            &RefineRequest {
                instructions: Some("remember the port".into()),
                rollback: None,
                global: true,
            },
            &model,
        );
        assert!(prompt.contains("[global:tests] tests"), "{prompt}");
        assert!(prompt.contains("no prior refinement history."), "{prompt}");
        assert!(
            prompt.contains("<conversation>\n[0] user: hello\n</conversation>"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Requested refinement scope: global"),
            "{prompt}"
        );
        assert!(
            prompt.contains("<user_refine_instructions>\nremember the port\n"),
            "{prompt}"
        );
    }

    #[test]
    fn a_review_parses_and_turns_into_a_local_refine_request() {
        let review = AutoRefineReview::parse(
            "{\"shouldRefine\": true, \"rationale\": \"the runner came up twice\", \"instructions\": \"remember nextest\"}",
        )
        .unwrap();
        assert!(review.should_refine);
        let request = review.refine_request(AutoRefineReason::TurnInterval);
        assert!(!request.global);
        let text = request.instructions.unwrap();
        assert!(
            text.starts_with("Automatic refine review triggered by turn_interval."),
            "{text}"
        );
        assert!(
            text.contains("Reviewer rationale: the runner came up twice"),
            "{text}"
        );
        assert!(
            text.ends_with("Reviewer instructions: remember nextest"),
            "{text}"
        );
        assert!(AutoRefineReview::parse("nope").is_err());
    }

    #[test]
    fn a_long_conversation_is_cut_from_the_front() {
        let mut model = mush_ai::models::all_models().into_iter().next().unwrap();
        model.context_window = TokenCount::new(40_000);
        model.effective_context_window = None;
        let conversation = "x".repeat(200_000);
        let prompt = user_prompt(
            &conversation,
            &State::default(),
            &[],
            &RefineRequest::default(),
            &model,
        );
        assert!(prompt.contains("[earlier conversation omitted to fit the model context]"));
        assert!(prompt.len() < 100_000, "{}", prompt.len());
    }
}
