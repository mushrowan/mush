//! session compaction
//!
//! when a conversation gets too long, older messages are replaced
//! with a summary while keeping recent context intact. the summary
//! is injected as a user message at the start of the compacted history.
//!
//! supports both structured (no LLM) and LLM-based summarisation.

use futures::StreamExt;
use mush_ai::registry::{ApiRegistry, LlmContext, is_retryable_stream_error_message};
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

/// how many recent messages to keep uncompacted
const DEFAULT_KEEP_RECENT: usize = 10;

/// how many recent assistant turns to keep full tool output for
const DEFAULT_KEEP_OBSERVATIONS: usize = 5;

/// maximum output tokens requested for LLM-generated compaction summaries
const COMPACTION_MAX_OUTPUT_TOKENS: u64 = 8192;

/// retry budget for transient compaction provider failures
const COMPACTION_MAX_RETRIES: u32 = 5;

/// maximum exponential backoff between compaction retry attempts
const COMPACTION_MAX_BACKOFF_SECS: u64 = 16;

/// rough chars-per-token ratio for estimation (LLM tokenisers average ~4)
const CHARS_PER_TOKEN: usize = 4;

/// rough char-equivalent for images in token estimation
const IMAGE_CHAR_ESTIMATE: usize = 1000;

/// prefix of the synthetic user message produced by compaction.
/// kept public so display layers can detect and re-render the
/// compaction summary distinctly (not as a user turn)
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:";

/// result of compaction
#[derive(Debug)]
pub struct CompactionResult {
    /// the compacted message list
    pub messages: Vec<Message>,
    /// number of messages that were summarised
    pub summarised_count: usize,
    /// the summary text that was generated
    pub summary: String,
    /// diagnostic information about LLM compaction retries and fallback
    pub diagnostics: CompactionDiagnostics,
}

/// diagnostic information from an LLM compaction attempt.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompactionDiagnostics {
    /// model that generated or attempted the summary
    pub model_id: String,
    /// number of retry attempts after the initial request
    pub retries: u32,
    /// number of oldest messages dropped from the summary request to
    /// make a too-large compaction prompt fit the model context window
    pub trimmed_messages: usize,
    /// fallback reason when LLM summarisation failed
    pub fallback_reason: Option<String>,
}

impl CompactionDiagnostics {
    #[must_use]
    pub fn status_note(&self) -> Option<String> {
        if let Some(reason) = &self.fallback_reason {
            return Some(format!("compaction fallback: {reason}"));
        }
        if self.trimmed_messages > 0 && self.retries > 0 {
            return Some(format!(
                "compaction recovered after trimming {} messages and {} retries",
                self.trimmed_messages, self.retries
            ));
        }
        if self.trimmed_messages > 0 {
            return Some(format!(
                "compaction recovered after trimming {} messages",
                self.trimmed_messages
            ));
        }
        if self.retries > 0 {
            return Some(format!(
                "compaction recovered after {} retries",
                self.retries
            ));
        }
        None
    }
}

/// estimate token count for a message list (rough: chars / CHARS_PER_TOKEN)
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// count the approximate character length of a message's content.
/// images use a fixed estimate since their tokens come from the
/// vision encoder, not character count.
fn message_char_count(msg: &Message) -> usize {
    match msg {
        Message::User(u) => match &u.content {
            UserContent::Text(t) => t.len(),
            UserContent::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    UserContentPart::Text(t) => t.text.len(),
                    UserContentPart::Image(_) => IMAGE_CHAR_ESTIMATE,
                })
                .sum(),
        },
        Message::Assistant(a) => a
            .content
            .iter()
            .map(|p| match p {
                AssistantContentPart::Text(t) => t.text.len(),
                AssistantContentPart::Thinking(t) => t.text().len(),
                AssistantContentPart::ToolCall(tc) => {
                    tc.name.as_str().len() + tc.arguments.to_string().len()
                }
            })
            .sum(),
        Message::ToolResult(tr) => tr
            .content
            .iter()
            .map(|p| match p {
                ToolResultContentPart::Text(t) => t.text.len(),
                ToolResultContentPart::Image(_) => IMAGE_CHAR_ESTIMATE,
            })
            .sum(),
    }
}

fn estimate_message_tokens(msg: &Message) -> usize {
    message_char_count(msg) / CHARS_PER_TOKEN
}

/// mask old tool result outputs while preserving the action history
///
/// walks the message list and replaces tool result content beyond
/// `keep_recent_turns` assistant turns with a brief summary. the
/// tool call itself (in the assistant message) is preserved so the
/// model can see what actions were taken without the bulky output.
///
/// returns the number of tool results that were masked and the
/// estimated token savings
pub fn mask_observations(
    messages: &mut [Message],
    keep_recent_turns: Option<usize>,
) -> ObservationMaskResult {
    let keep = keep_recent_turns.unwrap_or(DEFAULT_KEEP_OBSERVATIONS);

    // find the cutoff: the index of the `keep`th assistant message from
    // the end. everything before this index is "old" and gets masked
    let Some(cutoff_index) = find_recent_boundary(messages, TurnPivot::Assistant, keep) else {
        return ObservationMaskResult {
            masked_count: 0,
            tokens_saved: 0,
        };
    };

    // mask tool results before the cutoff
    let mut masked_count = 0;
    let mut tokens_saved = 0;

    for msg in &mut messages[..cutoff_index] {
        if let Message::ToolResult(tr) = msg {
            let old_tokens = estimate_message_tokens(&Message::ToolResult(tr.clone()));

            // count lines in the original output
            let line_count: usize = tr
                .content
                .iter()
                .map(|p| match p {
                    ToolResultContentPart::Text(t) => t.text.lines().count(),
                    ToolResultContentPart::Image(_) => 1,
                })
                .sum();

            let summary = format!(
                "[tool output hidden: {} lines, tool: {}]",
                line_count, tr.tool_name
            );

            tr.content = vec![ToolResultContentPart::Text(TextContent { text: summary })];

            let new_tokens = estimate_message_tokens(&Message::ToolResult(tr.clone()));
            tokens_saved += old_tokens.saturating_sub(new_tokens);
            masked_count += 1;
        }
    }

    ObservationMaskResult {
        masked_count,
        tokens_saved,
    }
}

/// result of observation masking
#[derive(Debug)]
pub struct ObservationMaskResult {
    /// number of tool results that had their content replaced
    pub masked_count: usize,
    /// estimated tokens saved by masking
    pub tokens_saved: usize,
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    let ellipsis = if max_chars >= 3 { "..." } else { "" };
    let keep = max_chars.saturating_sub(ellipsis.len());
    let mut iter = text.char_indices();

    for _ in 0..keep {
        if iter.next().is_none() {
            return text.to_string();
        }
    }

    let Some((end, _)) = iter.next() else {
        return text.to_string();
    };

    let mut truncated = text[..end].to_string();
    truncated.push_str(ellipsis);
    truncated
}

/// build a summary of messages that will be compacted
///
/// this produces a structured text summary. the actual LLM-based
/// summarisation happens upstream - this function just formats the
/// context for the summary prompt.
pub fn build_compaction_prompt(messages: &[Message]) -> String {
    let mut parts = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        match msg {
            Message::User(u) => {
                let text = u.text();
                parts.push(format!("[{i}] user: {text}"));
            }
            Message::Assistant(a) => {
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        AssistantContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let tools: Vec<&str> = a
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        AssistantContentPart::ToolCall(tc) => Some(tc.name.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_str = if tools.is_empty() {
                    String::new()
                } else {
                    format!(" [tools: {}]", tools.join(", "))
                };
                let truncated = truncate_with_ellipsis(&text, 200);
                parts.push(format!("[{i}] assistant: {truncated}{tool_str}"));
            }
            Message::ToolResult(tr) => {
                let text: String = tr
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let truncated = truncate_with_ellipsis(&text, 100);
                let status = if tr.outcome.is_error() {
                    " (error)"
                } else {
                    ""
                };
                parts.push(format!(
                    "[{i}] tool_result({}){status}: {truncated}",
                    tr.tool_name
                ));
            }
        }
    }

    parts.join("\n")
}

/// build the full LLM prompt body for compaction.
///
/// emits two sections: a `<conversation>` block holding the messages
/// to summarise, and (when non-empty) a `<recent_state>` block holding
/// the kept-recent messages purely as context. the prompt instructs
/// the LLM to summarise only the first section, using the second to
/// understand current state - this prevents recently-completed tasks
/// from being labelled "in progress" in the summary just because the
/// summariser didn't see their resolution.
pub fn build_compaction_request(old: &[Message], recent: &[Message]) -> String {
    let conversation_dump = build_compaction_prompt(old);
    if recent.is_empty() {
        return format!(
            "<conversation>\n{conversation_dump}\n</conversation>\n\n\
             {SUMMARISATION_INSTRUCTIONS}"
        );
    }
    let recent_dump = build_compaction_prompt(recent);
    format!(
        "<conversation>\n{conversation_dump}\n</conversation>\n\n\
         <recent_state>\n{recent_dump}\n</recent_state>\n\n\
         The <recent_state> section is provided as context only - it shows the most \
         recent messages in the live conversation, which are kept verbatim after the \
         compaction summary. Do not summarise the <recent_state> section. Use it only \
         to understand the current state so you don't mark recently-completed tasks \
         as in-progress in the summary.\n\n\
         {SUMMARISATION_INSTRUCTIONS}"
    )
}

/// compact messages by replacing older ones with a pre-built summary.
/// keeps the most recent `keep_recent` messages intact.
pub fn compact_with_summary(
    messages: Vec<Message>,
    summary: &str,
    keep_recent: Option<usize>,
) -> CompactionResult {
    let keep = keep_recent.unwrap_or(DEFAULT_KEEP_RECENT);

    if messages.len() <= keep {
        return CompactionResult {
            summarised_count: 0,
            summary: String::new(),
            messages,
            diagnostics: CompactionDiagnostics::default(),
        };
    }

    let split_at = messages.len() - keep;
    // walk forward to a non-tool-result boundary to avoid orphaned
    // tool_result messages that reference compacted-away tool_use blocks.
    // both User and Assistant messages are valid split points, but
    // ToolResult would create a dangling reference to a compacted tool call
    let split_at = messages[split_at..]
        .iter()
        .position(|m| !matches!(m, Message::ToolResult(_)))
        .map(|offset| split_at + offset);
    let Some(split_at) = split_at else {
        // all kept messages are tool results (shouldn't happen), keep everything
        return CompactionResult {
            summarised_count: 0,
            summary: String::new(),
            messages,
            diagnostics: CompactionDiagnostics::default(),
        };
    };
    let kept = messages[split_at..].to_vec();

    let summary_msg = Message::User(UserMessage {
        content: UserContent::Text(format!(
            "{COMPACTION_SUMMARY_PREFIX}\n\n\
             <summary>\n{summary}\n</summary>"
        )),
        timestamp_ms: Timestamp::now(),
    });

    let mut result = vec![summary_msg];
    result.extend(kept);

    CompactionResult {
        summarised_count: split_at,
        summary: summary.to_string(),
        messages: result,
        diagnostics: CompactionDiagnostics::default(),
    }
}

const COMPACTION_PROMPT: &str = "\
You are a context summarisation assistant. Your task is to read a conversation between \
a user and an AI coding assistant, then produce a structured summary following the exact \
format specified.

Do not continue the conversation. Do not respond to any questions in the conversation. \
Only output the structured summary.";

const SUMMARISATION_INSTRUCTIONS: &str = "\
Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

const FALLBACK_SUMMARY_HEADING: &str = "## Summary of earlier conversation";
/// trigger compaction when context usage exceeds this percentage
const COMPACTION_THRESHOLD: u64 = 95;

/// check whether compaction is needed based on real API token counts
///
/// triggers at 95% of context window. also requires more than
/// `DEFAULT_KEEP_RECENT` messages to avoid compacting tiny conversations.
pub fn needs_compaction_at(
    context_tokens: TokenCount,
    context_window: TokenCount,
    message_count: usize,
) -> bool {
    message_count > DEFAULT_KEEP_RECENT
        && context_tokens.exceeds_fraction(context_window, COMPACTION_THRESHOLD, 100)
}

/// result of the auto_compact escalation
#[derive(Debug)]
pub struct AutoCompactResult {
    /// the compacted message list
    pub messages: Vec<Message>,
    /// number of tool outputs masked (0 = no masking applied)
    pub masked_count: usize,
    /// estimated tokens saved by masking
    pub mask_tokens_saved: usize,
    /// number of messages that were summarised (0 = no LLM compaction)
    pub summarised_count: usize,
}

/// escalating auto-compaction: masking then LLM summarisation
///
/// 1. mask old tool outputs (cheap, no LLM call)
/// 2. if still over budget, LLM summarisation of old turns
///
/// callers handle post-compaction hooks separately since that
/// requires mush-agent types this crate doesn't depend on.
pub async fn auto_compact(
    messages: Vec<Message>,
    context_tokens: TokenCount,
    context_window: TokenCount,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
) -> AutoCompactResult {
    if !needs_compaction_at(context_tokens, context_window, messages.len()) {
        return AutoCompactResult {
            messages,
            masked_count: 0,
            mask_tokens_saved: 0,
            summarised_count: 0,
        };
    }

    // step 1: mask old tool outputs
    let mut messages = messages;
    let mask_result = mask_observations(&mut messages, None);

    if mask_result.masked_count > 0 {
        // re-check if we're still over budget after masking
        let threshold = (context_window.get() as usize) * COMPACTION_THRESHOLD as usize / 100;
        if estimate_tokens(&messages) < threshold {
            return AutoCompactResult {
                messages,
                masked_count: mask_result.masked_count,
                mask_tokens_saved: mask_result.tokens_saved,
                summarised_count: 0,
            };
        }
    }

    // step 2: LLM summarisation
    let result = llm_compact(
        messages,
        registry,
        model,
        options,
        Some(DEFAULT_KEEP_RECENT),
        None,
    )
    .await;

    AutoCompactResult {
        messages: result.messages,
        masked_count: mask_result.masked_count,
        mask_tokens_saved: mask_result.tokens_saved,
        summarised_count: result.summarised_count,
    }
}

/// compact messages using an LLM to generate the summary.
/// falls back to structured compaction if the LLM call fails.
///
/// `context_budget` is an optional target size for the generated
/// summary. when set, it's appended to the system prompt as a soft
/// instruction so the LLM aims to produce output within that budget.
/// the value is a hint, not a hard cap. the fallback path ignores it
/// since the structured fallback summary has a fixed shape
pub async fn llm_compact(
    messages: Vec<Message>,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    keep_recent: Option<usize>,
    context_budget: Option<TokenCount>,
) -> CompactionResult {
    let keep = keep_recent.unwrap_or(DEFAULT_KEEP_RECENT);

    if messages.len() <= keep {
        return CompactionResult {
            summarised_count: 0,
            summary: String::new(),
            messages,
            diagnostics: CompactionDiagnostics {
                model_id: model.id.to_string(),
                ..CompactionDiagnostics::default()
            },
        };
    }

    let split_at = messages.len() - keep;
    let old_messages = &messages[..split_at];
    let recent_messages = &messages[split_at..];

    let compact_options = compaction_stream_options(model, options);
    let mut old_start = 0;
    let mut diagnostics = CompactionDiagnostics {
        model_id: model.id.to_string(),
        ..CompactionDiagnostics::default()
    };
    let summary = loop {
        let old_slice = &old_messages[old_start..];
        let context = compaction_context(old_slice, recent_messages, context_budget);

        match call_for_text(registry, model, &context, &compact_options).await {
            Ok(outcome) => {
                diagnostics.retries += outcome.retries;
                tracing::info!(
                    chars = outcome.text.len(),
                    dropped_messages = old_start,
                    "LLM compaction succeeded"
                );
                break outcome.text;
            }
            Err(error) if error.failure.is_context_overflow() && old_slice.len() > 1 => {
                diagnostics.retries += error.retries;
                let dropped = (old_slice.len() / 4).max(1);
                old_start += dropped;
                diagnostics.trimmed_messages = old_start;
                tracing::warn!(error = %error.failure, dropped, total_dropped = old_start, "compaction request too large, trimming oldest summary input and retrying");
            }
            Err(error) => {
                diagnostics.retries += error.retries;
                diagnostics.fallback_reason = Some(error.failure.to_string());
                tracing::warn!(error = %error.failure, "LLM compaction failed, using structured fallback");
                break fallback_summary_from_messages(
                    old_messages,
                    recent_messages,
                    old_start,
                    &error.failure,
                );
            }
        }
    };

    let mut result = compact_with_summary(messages, &summary, keep_recent);
    result.diagnostics = diagnostics;
    result
}

fn compaction_context(
    old_messages: &[Message],
    recent_messages: &[Message],
    context_budget: Option<TokenCount>,
) -> LlmContext {
    // recent kept messages are passed as context-only so the summariser
    // knows the current state and doesn't mislabel recently-completed
    // tasks as in-progress
    let request_body = build_compaction_request(old_messages, recent_messages);
    let system_prompt = match context_budget {
        Some(budget) => format!(
            "{COMPACTION_PROMPT}\n\n\
             Target a summary length of approximately {} tokens. Trim lower-priority \
             detail before higher-priority detail to fit within this budget. Keep the \
             section structure intact even if individual sections become terser.",
            budget.get()
        ),
        None => COMPACTION_PROMPT.to_string(),
    };

    LlmContext {
        system_prompt: Some(system_prompt),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(request_body),
            timestamp_ms: Timestamp::now(),
        })],
        tools: vec![],
    }
}

fn fallback_summary_from_messages(
    old_messages: &[Message],
    recent_messages: &[Message],
    dropped_messages: usize,
    error: &CompactionFailure,
) -> String {
    let mut out = format!(
        "{FALLBACK_SUMMARY_HEADING}\n\n\
         ## Compaction fallback reason\n\n\
         LLM summarisation failed: {error}\n"
    );

    if dropped_messages > 0 {
        out.push_str(&format!(
            "\nThe LLM compaction request also had to drop the oldest {dropped_messages} messages before falling back.\n"
        ));
    }

    let prior_summaries = fallback_prior_summaries(old_messages);
    if !prior_summaries.is_empty() {
        out.push_str("\n## Prior summaries\n\n");
        for summary in prior_summaries {
            out.push_str(&format!("- {}\n", truncate_with_ellipsis(&summary, 500)));
        }
    }

    let user_requests = fallback_recent_user_requests(old_messages, recent_messages, 8);
    if !user_requests.is_empty() {
        out.push_str("\n## Recent user requests\n\n");
        for request in user_requests {
            out.push_str(&format!("- {request}\n"));
        }
    }

    let assistant_notes = fallback_recent_assistant_notes(old_messages, 8);
    if !assistant_notes.is_empty() {
        out.push_str("\n## Recent assistant notes\n\n");
        for note in assistant_notes {
            out.push_str(&format!("- {note}\n"));
        }
    }

    let tool_activity = fallback_recent_tool_activity(old_messages, recent_messages, 12);
    if !tool_activity.is_empty() {
        out.push_str("\n## Tool and file activity\n\n");
        for activity in tool_activity {
            out.push_str(&format!("- {activity}\n"));
        }
    }

    if !recent_messages.is_empty() {
        out.push_str("\n## Live recent context kept verbatim\n\n");
        out.push_str("The newest messages were not summarised because they remain in the live conversation after this summary.\n");
    }

    out.push_str("\n## Raw compacted-message appendix\n\n");
    out.push_str("This appendix is kept as a safety net because LLM summarisation failed.\n\n");
    out.push_str(&build_compaction_prompt(old_messages));
    out
}

fn fallback_prior_summaries(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => {
                let text = user.text();
                text.strip_prefix(COMPACTION_SUMMARY_PREFIX)
                    .map(str::trim)
                    .filter(|summary| !summary.is_empty())
                    .map(str::to_string)
            }
            _ => None,
        })
        .collect()
}

fn fallback_recent_user_requests(
    old_messages: &[Message],
    recent_messages: &[Message],
    limit: usize,
) -> Vec<String> {
    old_messages
        .iter()
        .chain(recent_messages.iter())
        .rev()
        .filter_map(|message| match message {
            Message::User(user) => {
                let text = user.text();
                if text.starts_with(COMPACTION_SUMMARY_PREFIX) {
                    None
                } else {
                    Some(truncate_with_ellipsis(&text, 240))
                }
            }
            _ => None,
        })
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn fallback_recent_assistant_notes(messages: &[Message], limit: usize) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => assistant_text(assistant)
                .filter(|text| !text.trim().is_empty())
                .map(|text| truncate_with_ellipsis(&text, 240)),
            _ => None,
        })
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn fallback_recent_tool_activity(
    old_messages: &[Message],
    recent_messages: &[Message],
    limit: usize,
) -> Vec<String> {
    old_messages
        .iter()
        .chain(recent_messages.iter())
        .rev()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => {
                let tools = assistant_tool_names(assistant);
                (!tools.is_empty())
                    .then(|| format!("assistant requested tools: {}", tools.join(", ")))
            }
            Message::ToolResult(result) => {
                let status = if result.outcome.is_error() {
                    "error"
                } else {
                    "ok"
                };
                let text = tool_result_text(result);
                Some(format!(
                    "{} result ({status}): {}",
                    result.tool_name,
                    truncate_with_ellipsis(&text, 180)
                ))
            }
            Message::User(_) => None,
        })
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn assistant_text(assistant: &AssistantMessage) -> Option<String> {
    let text = assistant
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn assistant_tool_names(assistant: &AssistantMessage) -> Vec<&str> {
    assistant
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantContentPart::ToolCall(tool) => Some(tool.name.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result_text(result: &ToolResultMessage) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            ToolResultContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn compaction_stream_options(model: &Model, options: &StreamOptions) -> StreamOptions {
    let mut compact_options = options.clone();
    compact_options.max_tokens = Some(TokenCount::new(COMPACTION_MAX_OUTPUT_TOKENS));
    compact_options.thinking = model.reasoning.then_some(ThinkingLevel::Low);
    compact_options
}

/// make a simple LLM call and collect the text response
async fn call_for_text(
    registry: &ApiRegistry,
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> Result<CompactionText, CompactionError> {
    let mut last_error = None;

    for attempt in 0..=COMPACTION_MAX_RETRIES {
        if attempt > 0 {
            let delay = std::time::Duration::from_secs(
                (1u64 << (attempt - 1)).min(COMPACTION_MAX_BACKOFF_SECS),
            );
            tracing::warn!(attempt, ?delay, "retrying compaction after transient error");
            tokio::time::sleep(delay).await;
        }

        let stream = match registry.stream(model, context, options).await {
            Ok(stream) => stream,
            Err(error) if error.is_retryable() && attempt < COMPACTION_MAX_RETRIES => {
                tracing::warn!(%error, "retryable compaction request error");
                last_error = Some(CompactionFailure::Request(error.to_string()));
                continue;
            }
            Err(error) => {
                return Err(CompactionError {
                    failure: CompactionFailure::Request(error.to_string()),
                    retries: attempt,
                });
            }
        };

        match collect_text(stream).await {
            Ok(text) => {
                return Ok(CompactionText {
                    text,
                    retries: attempt,
                });
            }
            Err(error) if error.is_retryable() && attempt < COMPACTION_MAX_RETRIES => {
                tracing::warn!(%error, "retryable compaction stream error");
                last_error = Some(error);
                continue;
            }
            Err(error) => {
                return Err(CompactionError {
                    failure: error,
                    retries: attempt,
                });
            }
        }
    }

    Err(CompactionError {
        failure: last_error.unwrap_or(CompactionFailure::EmptyResponse),
        retries: COMPACTION_MAX_RETRIES,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactionText {
    text: String,
    retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactionError {
    failure: CompactionFailure,
    retries: u32,
}

async fn collect_text(
    mut stream: mush_ai::registry::EventStream,
) -> Result<String, CompactionFailure> {
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
            StreamEvent::Done { reason, message } => {
                if reason == StopReason::Error || message.stop_reason == StopReason::Error {
                    return Err(CompactionFailure::Stream(
                        message
                            .error_message
                            .unwrap_or_else(|| "stream ended with error".into()),
                    ));
                }
                break;
            }
            StreamEvent::Error { message, .. } => {
                return Err(CompactionFailure::Stream(
                    message
                        .error_message
                        .unwrap_or_else(|| "stream emitted error".into()),
                ));
            }
            _ => {}
        }
    }

    if text.is_empty() {
        Err(CompactionFailure::EmptyResponse)
    } else {
        Ok(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum CompactionFailure {
    #[error("request failed: {0}")]
    Request(String),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("stream completed without text")]
    EmptyResponse,
}

impl CompactionFailure {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Request(_) => false,
            Self::Stream(message) => is_retryable_stream_error_message(message),
            Self::EmptyResponse => false,
        }
    }

    fn is_context_overflow(&self) -> bool {
        let message = match self {
            Self::Request(message) | Self::Stream(message) => message,
            Self::EmptyResponse => return false,
        }
        .to_ascii_lowercase();

        [
            "context_length_exceeded",
            "context length exceeded",
            "context window",
            "too many input tokens",
            "input tokens exceed",
            "maximum context",
            "prompt is too long",
        ]
        .iter()
        .any(|pat| message.contains(pat))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use mush_ai::registry::{ApiProvider, EventStream, ProviderError};

    fn user_msg(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: text.into(),
                })])
                .model("test".into())
                .provider(Provider::Custom("test".into()))
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn test_model() -> Model {
        Model {
            id: "test".into(),
            name: "test".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(8192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }

    #[test]
    fn estimate_tokens_basic() {
        let msgs = vec![user_msg("hello world")]; // 11 chars / 4 = 2
        assert_eq!(estimate_tokens(&msgs), 2);
    }

    #[test]
    fn message_char_count_text_message() {
        let msg = user_msg("hello world"); // 11 chars
        assert_eq!(message_char_count(&msg), 11);
    }

    #[test]
    fn message_char_count_image_uses_constant() {
        let msg = Message::User(UserMessage {
            content: UserContent::Parts(vec![UserContentPart::Image(ImageContent {
                mime_type: ImageMimeType::Png,
                data: "base64data".into(),
            })]),
            timestamp_ms: Timestamp::zero(),
        });
        assert_eq!(message_char_count(&msg), IMAGE_CHAR_ESTIMATE);
    }

    #[test]
    fn estimate_uses_chars_per_token_ratio() {
        // 40 chars / CHARS_PER_TOKEN(4) = 10 tokens
        let msg = user_msg(&"a".repeat(40));
        assert_eq!(estimate_message_tokens(&msg), 40 / CHARS_PER_TOKEN);
    }

    #[test]
    fn build_prompt_includes_all_messages() {
        let msgs = vec![
            user_msg("explain traits"),
            assistant_msg("traits are interfaces"),
            user_msg("show an example"),
        ];
        let prompt = build_compaction_prompt(&msgs);
        assert!(prompt.contains("explain traits"));
        assert!(prompt.contains("traits are interfaces"));
        assert!(prompt.contains("show an example"));
    }

    #[test]
    fn build_prompt_handles_multibyte_truncation() {
        let long_text = "—".repeat(300);
        let msgs = vec![assistant_msg(&long_text)];
        let prompt = build_compaction_prompt(&msgs);
        assert!(prompt.contains("..."));
        assert!(!prompt.is_empty());
    }

    #[test]
    fn build_request_includes_old_section_with_messages_to_summarise() {
        let old = vec![
            user_msg("kick off task A"),
            assistant_msg("working on task A"),
        ];
        let recent: Vec<Message> = vec![];
        let request = build_compaction_request(&old, &recent);
        assert!(
            request.contains("<conversation>"),
            "request should mark old messages with the summarisation tag: {request}"
        );
        assert!(
            request.contains("kick off task A"),
            "old messages should be included for summarisation: {request}"
        );
    }

    #[test]
    fn build_request_marks_recent_messages_as_context_only() {
        // the headline fix: recent kept messages must be included in
        // the prompt so the summariser knows what state the conversation
        // is in, and labelled as context-only so the summariser doesn't
        // include them in the summary itself
        let old = vec![
            user_msg("start task"),
            assistant_msg("calling tool to do task"),
        ];
        let recent = vec![
            assistant_msg("task done, here's the result"),
            user_msg("great, what's next?"),
        ];
        let request = build_compaction_request(&old, &recent);

        assert!(
            request.contains("task done"),
            "recent messages must appear in the prompt: {request}"
        );
        assert!(
            request.to_lowercase().contains("context"),
            "prompt must label the recent section as context: {request}"
        );
        // explicit guidance that recent is NOT to be summarised. the
        // exact phrasing can flex, but the intent must be discoverable
        let lower = request.to_lowercase();
        assert!(
            lower.contains("do not summarise")
                || lower.contains("do not include")
                || lower.contains("not to summarise")
                || lower.contains("not in the summary")
                || lower.contains("only summarise"),
            "prompt must instruct the LLM not to summarise the recent section: {request}"
        );
    }

    #[test]
    fn build_request_omits_recent_section_when_empty() {
        // when there are no recent messages (e.g. compacting an entire
        // history), don't emit a context-only section at all so the
        // prompt stays minimal and the LLM doesn't see an empty wrapper
        let old = vec![user_msg("just one message")];
        let request = build_compaction_request(&old, &[]);
        let lower = request.to_lowercase();
        assert!(
            !lower.contains("recent_state") && !lower.contains("recent state"),
            "no recent → no recent section: {request}"
        );
    }

    #[test]
    fn build_request_does_not_double_count_recent_in_old_section() {
        // sanity: old and recent must be disjoint in the rendered
        // output. we observe by ensuring a recent-only marker doesn't
        // appear inside the <conversation> block (a regression where
        // the function accidentally appended recent to old would fail
        // this)
        let old = vec![user_msg("OLD_MARKER")];
        let recent = vec![user_msg("RECENT_MARKER")];
        let request = build_compaction_request(&old, &recent);

        let conv_start = request.find("<conversation>").expect("has open tag");
        let conv_end = request.find("</conversation>").expect("has close tag");
        let conversation_section = &request[conv_start..conv_end];
        assert!(
            !conversation_section.contains("RECENT_MARKER"),
            "recent must not leak into the conversation section: {conversation_section}"
        );
    }

    #[test]
    fn compact_short_conversation_unchanged() {
        let msgs = vec![user_msg("hi"), assistant_msg("hello")];
        let result = compact_with_summary(msgs.clone(), "summary", None);
        assert_eq!(result.summarised_count, 0);
        assert_eq!(result.messages.len(), 2);
    }

    #[test]
    fn compact_long_conversation() {
        let msgs: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    user_msg(&format!("question {i}"))
                } else {
                    assistant_msg(&format!("answer {i}"))
                }
            })
            .collect();

        let result = compact_with_summary(msgs, "this is the summary", Some(5));
        // split_at starts at 15 (assistant), which is not a ToolResult
        // so split happens there: 1 summary + 5 kept = 6
        assert_eq!(result.messages.len(), 6);
        assert_eq!(result.summarised_count, 15);

        // first message should be the summary
        if let Message::User(u) = &result.messages[0] {
            if let UserContent::Text(t) = &u.content {
                assert!(t.contains("summary"));
            } else {
                panic!("expected text content");
            }
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn compact_preserves_recent_messages() {
        let msgs = vec![
            user_msg("old"),
            assistant_msg("old answer"),
            user_msg("recent"),
            assistant_msg("recent answer"),
        ];

        let result = compact_with_summary(msgs, "old stuff", Some(2));
        assert_eq!(result.messages.len(), 3); // 1 summary + 2 kept
        // last two should be the recent messages
        if let Message::User(u) = &result.messages[1] {
            if let UserContent::Text(t) = &u.content {
                assert_eq!(t, "recent");
            } else {
                panic!("expected text");
            }
        }
    }

    #[test]
    fn needs_compaction_below_threshold() {
        // 100 tokens used, 200k window, well under 95%
        assert!(!needs_compaction_at(
            TokenCount::new(100),
            TokenCount::new(200_000),
            20,
        ));
    }

    #[test]
    fn needs_compaction_above_threshold() {
        // 9600 tokens used, 10k window = 96%, over 95% threshold
        assert!(needs_compaction_at(
            TokenCount::new(9600),
            TokenCount::new(10_000),
            20,
        ));
    }

    #[test]
    fn needs_compaction_too_few_messages() {
        // high usage but only 8 messages, below DEFAULT_KEEP_RECENT
        assert!(!needs_compaction_at(
            TokenCount::new(9600),
            TokenCount::new(10_000),
            8,
        ));
    }

    #[test]
    fn compact_skips_orphaned_tool_results() {
        // simulate: user, assistant+tool_use, tool_result, user, assistant
        // with keep=2, naive split would start at the tool_result,
        // causing an API error. should advance past tool_result to user msg.
        let msgs = vec![
            user_msg("first"),
            assistant_msg("thinking"),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "output".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
            user_msg("second"),
            assistant_msg("response"),
        ];
        let result = compact_with_summary(msgs, "summary", Some(2));
        // should skip past the tool_result and start at "second" (user msg)
        assert!(matches!(result.messages[0], Message::User(_))); // summary
        assert!(matches!(result.messages[1], Message::User(_))); // "second"
        assert_eq!(result.summarised_count, 3);
    }

    #[test]
    fn compact_works_when_kept_range_has_no_user_messages() {
        // this was the bug: if the last `keep` messages are all assistant+tool_result
        // pairs (common in long tool-use chains), the old code returned unchanged
        // because it only looked for User messages as boundaries
        let mut msgs = vec![user_msg("start"), assistant_msg("thinking")];
        // add 10 assistant+tool_result pairs (20 messages)
        for i in 0..10 {
            msgs.push(assistant_msg(&format!("calling tool {i}")));
            msgs.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: format!("tc_{i}").into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: format!("output {i}"),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }));
        }
        assert_eq!(msgs.len(), 22);
        let result = compact_with_summary(msgs, "summary of work", Some(5));
        // split_at = 22-5 = 17. messages[17] is a ToolResult, so advances to
        // 18 (assistant), which is not a ToolResult. kept = 4 + 1 summary = 5
        assert!(result.summarised_count > 0, "should have compacted");
        assert!(result.messages.len() < 22, "should have fewer messages");
        // first message is the summary
        assert!(matches!(result.messages[0], Message::User(_)));
        // no orphaned tool results: first non-summary message should be assistant
        assert!(matches!(result.messages[1], Message::Assistant(_)));
    }

    #[tokio::test]
    async fn llm_compact_fallback_on_no_provider() {
        // with an empty registry, the LLM call will fail and it should
        // fall back to structured compaction
        let registry = ApiRegistry::new();
        let model = Model {
            id: "test".into(),
            name: "test".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(8192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        };
        let options = StreamOptions::default();

        let msgs: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    user_msg(&format!("question {i}"))
                } else {
                    assistant_msg(&format!("answer {i}"))
                }
            })
            .collect();

        let result = llm_compact(msgs, &registry, &model, &options, Some(5), None).await;
        // split_at = 15, which is an assistant msg (not ToolResult), so stays at 15
        assert_eq!(result.summarised_count, 15);
        assert_eq!(result.messages.len(), 6); // 1 summary + 5 kept
        assert!(result.summary.contains("Summary of earlier conversation"));
    }

    #[tokio::test]
    async fn llm_compact_fallback_includes_failure_reason() {
        let registry = ApiRegistry::new();
        let model = test_model();
        let options = StreamOptions::default();
        let msgs: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    user_msg(&format!("question {i}"))
                } else {
                    assistant_msg(&format!("answer {i}"))
                }
            })
            .collect();

        let result = llm_compact(msgs, &registry, &model, &options, Some(5), None).await;

        assert!(
            result.summary.contains("Compaction fallback reason"),
            "fallback should explain why the LLM summary failed: {}",
            result.summary
        );
        assert!(
            result.summary.contains("no provider registered"),
            "fallback should preserve the provider error: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn llm_compact_fallback_keeps_recent_context_first() {
        let registry = ApiRegistry::new();
        let model = test_model();
        let options = StreamOptions::default();
        let mut msgs: Vec<Message> = (0..12)
            .map(|i| {
                if i % 2 == 0 {
                    user_msg(&format!("older request {i}"))
                } else {
                    assistant_msg(&format!("older answer {i}"))
                }
            })
            .collect();
        msgs.push(user_msg("important recent task: preserve this"));
        msgs.push(assistant_msg("recent decision: keep the new parser"));

        let result = llm_compact(msgs, &registry, &model, &options, Some(2), None).await;

        assert!(result.summary.contains("## Recent user requests"));
        assert!(
            result
                .summary
                .contains("important recent task: preserve this")
        );
        assert!(
            result
                .summary
                .contains("## Live recent context kept verbatim")
        );
        assert!(result.summary.contains("## Raw compacted-message appendix"));
        let recent_section = result.summary.find("## Recent user requests").unwrap();
        let raw_appendix = result
            .summary
            .find("## Raw compacted-message appendix")
            .unwrap();
        assert!(recent_section < raw_appendix);
    }

    #[tokio::test]
    async fn llm_compact_retries_retryable_stream_errors() {
        struct FlakyProvider {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiProvider for FlakyProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }

            async fn stream(
                &self,
                model: &Model,
                _context: &LlmContext,
                _options: &StreamOptions,
            ) -> Result<EventStream, ProviderError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let message = AssistantMessage::builder()
                    .model(model.id.clone())
                    .provider(model.provider.clone())
                    .api(model.api)
                    .timestamp_ms(Timestamp::zero())
                    .build();

                if call == 0 {
                    let mut errored = message.clone();
                    errored.stop_reason = StopReason::Error;
                    errored.error_message = Some("overloaded_error: try again".into());
                    Ok(Box::pin(futures::stream::iter(vec![StreamEvent::Done {
                        reason: StopReason::Error,
                        message: errored,
                    }])))
                } else {
                    Ok(Box::pin(futures::stream::iter(vec![
                        StreamEvent::TextDelta {
                            content_index: 0,
                            delta: "real summary".into(),
                        },
                        StreamEvent::Done {
                            reason: StopReason::Stop,
                            message,
                        },
                    ])))
                }
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(FlakyProvider {
            calls: calls.clone(),
        }));
        let model = test_model();
        let options = StreamOptions::default();
        let msgs: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    user_msg(&format!("question {i}"))
                } else {
                    assistant_msg(&format!("answer {i}"))
                }
            })
            .collect();

        let result = llm_compact(msgs, &registry, &model, &options, Some(5), None).await;

        assert_eq!(result.summary, "real summary");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn llm_compact_trims_and_retries_context_overflow() {
        struct OverflowOnceProvider {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl ApiProvider for OverflowOnceProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }

            async fn stream(
                &self,
                model: &Model,
                _context: &LlmContext,
                _options: &StreamOptions,
            ) -> Result<EventStream, ProviderError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    return Err(ProviderError::Other(
                        "context_length_exceeded: too many input tokens".into(),
                    ));
                }
                let message = AssistantMessage::builder()
                    .model(model.id.clone())
                    .provider(model.provider.clone())
                    .api(model.api)
                    .timestamp_ms(Timestamp::zero())
                    .build();
                Ok(Box::pin(futures::stream::iter(vec![
                    StreamEvent::TextDelta {
                        content_index: 0,
                        delta: "summary after trim".into(),
                    },
                    StreamEvent::Done {
                        reason: StopReason::Stop,
                        message,
                    },
                ])))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(OverflowOnceProvider {
            calls: calls.clone(),
        }));
        let model = test_model();
        let options = StreamOptions::default();
        let msgs: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    user_msg(&format!("question {i}"))
                } else {
                    assistant_msg(&format!("answer {i}"))
                }
            })
            .collect();

        let result = llm_compact(msgs, &registry, &model, &options, Some(5), None).await;

        assert_eq!(result.summary, "summary after trim");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn compaction_options_request_low_reasoning_for_reasoning_models() {
        let model = Model {
            id: "openai/gpt-5.4-nano".into(),
            name: "GPT 5.4 Nano".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(400_000),
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        };
        let options = StreamOptions {
            thinking: None,
            ..StreamOptions::default()
        };

        let compact_options = compaction_stream_options(&model, &options);

        assert_eq!(compact_options.max_tokens, Some(TokenCount::new(8192)));
        assert_eq!(compact_options.thinking, Some(ThinkingLevel::Low));
    }

    #[test]
    fn recompaction_triggers_after_compacted_conversation_grows() {
        let keep = 4;

        let mut msgs: Vec<Message> = (0..20)
            .flat_map(|i| {
                vec![
                    user_msg(&format!("question {i}: here is a detailed question with enough text to accumulate significant token count")),
                    assistant_msg(&format!("answer {i}: here is a correspondingly detailed response with enough text to push the total")),
                ]
            })
            .collect();

        // simulate token counts: 40 messages, lots of tokens
        let estimated = estimate_tokens(&msgs);
        let context_window = TokenCount::new((estimated * 100 / 96) as u64); // just barely over 95%
        assert!(
            needs_compaction_at(
                TokenCount::new(estimated as u64),
                context_window,
                msgs.len()
            ),
            "should need compaction: {estimated} tokens",
        );

        // compact with a short summary
        let result = compact_with_summary(msgs, "brief summary", Some(keep));
        msgs = result.messages;

        // right after compaction, shouldn't need it again
        let post_estimated = estimate_tokens(&msgs);
        assert!(
            !needs_compaction_at(
                TokenCount::new(post_estimated as u64),
                context_window,
                msgs.len()
            ),
            "shouldn't need compaction right after: {post_estimated} tokens in {} msgs",
            msgs.len(),
        );

        // add more messages until we exceed threshold again
        for i in 0..20 {
            msgs.push(user_msg(&format!(
                "follow-up {i}: another detailed question to push the conversation back over the threshold"
            )));
            msgs.push(assistant_msg(&format!(
                "follow-up answer {i}: another detailed response that inflates the total context usage"
            )));
        }

        let regrown = estimate_tokens(&msgs);
        assert!(
            needs_compaction_at(TokenCount::new(regrown as u64), context_window, msgs.len()),
            "should need re-compaction: {regrown} tokens in {} msgs",
            msgs.len(),
        );

        // compact again
        let result2 =
            compact_with_summary(msgs, "updated summary including follow-ups", Some(keep));
        assert!(result2.summarised_count > 0);
        assert!(result2.summary.contains("updated summary"));
    }

    #[test]
    fn compaction_summary_replaces_prior_summary() {
        // verify that re-compaction replaces the old summary, not stacks them
        let keep = 4;

        let msgs: Vec<Message> = (0..20)
            .flat_map(|i| {
                vec![
                    user_msg(&format!("msg {i} padding padding padding padding padding")),
                    assistant_msg(&format!(
                        "reply {i} padding padding padding padding padding"
                    )),
                ]
            })
            .collect();

        // first compaction
        let r1 = compact_with_summary(msgs, "first summary", Some(keep));
        let r1_first = match &r1.messages[0] {
            Message::User(u) => match &u.content {
                mush_ai::types::UserContent::Text(t) => t.as_str(),
                _ => panic!("expected text content"),
            },
            _ => panic!("expected user message"),
        };
        assert!(
            r1_first.contains("first summary"),
            "first compaction should contain summary: {r1_first}"
        );

        // add more messages
        let mut msgs2 = r1.messages;
        for i in 0..20 {
            msgs2.push(user_msg(&format!(
                "more {i} padding padding padding padding padding"
            )));
            msgs2.push(assistant_msg(&format!(
                "more reply {i} padding padding padding padding"
            )));
        }

        // second compaction
        let r2 = compact_with_summary(msgs2, "second summary covering everything", Some(keep));

        // should only have one summary message at the start
        let first_text = match &r2.messages[0] {
            Message::User(u) => match &u.content {
                mush_ai::types::UserContent::Text(t) => Some(t.as_str()),
                mush_ai::types::UserContent::Parts(parts) => parts.iter().find_map(|p| {
                    if let mush_ai::types::UserContentPart::Text(t) = p {
                        Some(t.text.as_str())
                    } else {
                        None
                    }
                }),
            },
            _ => None,
        };
        assert!(
            first_text.is_some_and(|t| t.contains("second summary")),
            "should have second summary, got: {first_text:?}"
        );
        assert!(
            !first_text.is_some_and(|t| t.contains("first summary")),
            "should not contain first summary anymore"
        );
    }

    fn tool_result_msg(tool_name: &str, output: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "tc_1".into(),
            tool_name: tool_name.into(),
            content: vec![ToolResultContentPart::Text(TextContent {
                text: output.into(),
            })],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
        })
    }

    #[test]
    fn mask_observations_basic() {
        // 3 turns: old tool result should be masked, recent two kept
        let big_output = (0..50)
            .map(|i| format!("line {i}: some output text here"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut msgs = vec![
            user_msg("q1"),
            assistant_msg("calling tool"),
            tool_result_msg("bash", &big_output),
            user_msg("q2"),
            assistant_msg("calling another tool"),
            tool_result_msg("read", "file contents\nline2"),
            user_msg("q3"),
            assistant_msg("final answer"),
        ];

        let result = mask_observations(&mut msgs, Some(2));

        // first tool result (turn 1) should be masked
        assert_eq!(result.masked_count, 1);
        assert!(result.tokens_saved > 0);

        if let Message::ToolResult(tr) = &msgs[2] {
            let text = &tr.content[0];
            if let ToolResultContentPart::Text(t) = text {
                assert!(
                    t.text.contains("[tool output hidden:"),
                    "should be masked: {}",
                    t.text
                );
                assert!(t.text.contains("bash"), "should mention tool name");
            }
        } else {
            panic!("expected tool result at index 2");
        }

        // second tool result (turn 2) should be kept
        if let Message::ToolResult(tr) = &msgs[5] {
            let text = &tr.content[0];
            if let ToolResultContentPart::Text(t) = text {
                assert!(
                    t.text.contains("file contents"),
                    "recent tool result should be preserved: {}",
                    t.text
                );
            }
        }
    }

    #[test]
    fn mask_observations_nothing_to_mask() {
        let mut msgs = vec![user_msg("hi"), assistant_msg("hello")];

        let result = mask_observations(&mut msgs, Some(5));
        assert_eq!(result.masked_count, 0);
        assert_eq!(result.tokens_saved, 0);
    }

    #[test]
    fn mask_observations_preserves_structure() {
        let mut msgs = vec![
            user_msg("q1"),
            assistant_msg("a1"),
            tool_result_msg("bash", "output 1"),
            user_msg("q2"),
            assistant_msg("a2"),
            tool_result_msg("read", "output 2"),
            user_msg("q3"),
            assistant_msg("a3"),
            tool_result_msg("edit", "output 3"),
        ];

        mask_observations(&mut msgs, Some(1));

        // message count unchanged
        assert_eq!(msgs.len(), 9);
        // types preserved
        assert!(matches!(msgs[0], Message::User(_)));
        assert!(matches!(msgs[1], Message::Assistant(_)));
        assert!(matches!(msgs[2], Message::ToolResult(_)));
    }

    #[test]
    fn mask_observations_counts_lines() {
        let mut msgs = vec![
            user_msg("q"),
            assistant_msg("a"),
            tool_result_msg("bash", "line1\nline2\nline3"),
            user_msg("q2"),
            assistant_msg("a2"),
        ];

        mask_observations(&mut msgs, Some(1));

        if let Message::ToolResult(tr) = &msgs[2]
            && let ToolResultContentPart::Text(t) = &tr.content[0]
        {
            assert!(t.text.contains("3 lines"), "should count lines: {}", t.text);
        }
    }

    #[test]
    fn mask_observations_default_threshold() {
        // with default keep (5), need >5 assistant turns to trigger masking
        let mut msgs: Vec<Message> = Vec::new();
        for i in 0..7 {
            msgs.push(user_msg(&format!("q{i}")));
            msgs.push(assistant_msg(&format!("a{i}")));
            msgs.push(tool_result_msg("bash", &format!("output {i}")));
        }

        let result = mask_observations(&mut msgs, None);
        // 7 turns, keep 5, so 2 should be masked
        assert_eq!(result.masked_count, 2);
    }
}
