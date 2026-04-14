//! session compaction
//!
//! when a conversation gets too long, older messages are replaced
//! with a summary while keeping recent context intact. the summary
//! is injected as a user message at the start of the compacted history.
//!
//! supports both structured (no LLM) and LLM-based summarisation.

use futures::StreamExt;
use mush_ai::registry::{ApiRegistry, LlmContext};
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

/// how many recent messages to keep uncompacted
const DEFAULT_KEEP_RECENT: usize = 10;

/// how many recent assistant turns to keep full tool output for
const DEFAULT_KEEP_OBSERVATIONS: usize = 5;

/// rough chars-per-token ratio for estimation (LLM tokenisers average ~4)
const CHARS_PER_TOKEN: usize = 4;

/// rough char-equivalent for images in token estimation
const IMAGE_CHAR_ESTIMATE: usize = 1000;

/// result of compaction
#[derive(Debug)]
pub struct CompactionResult {
    /// the compacted message list
    pub messages: Vec<Message>,
    /// number of messages that were summarised
    pub summarised_count: usize,
    /// the summary text that was generated
    pub summary: String,
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
    // the end. everything before this index is "old" and gets masked.
    let mut assistant_count = 0;
    let mut cutoff_index = 0;
    let mut found = false;
    for (i, msg) in messages.iter().enumerate().rev() {
        if matches!(msg, Message::Assistant(_)) {
            assistant_count += 1;
            if assistant_count == keep {
                cutoff_index = i;
                found = true;
                break;
            }
        }
    }

    if !found {
        return ObservationMaskResult {
            masked_count: 0,
            tokens_saved: 0,
        };
    }

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
        };
    };
    let kept = messages[split_at..].to_vec();

    let summary_msg = Message::User(UserMessage {
        content: UserContent::Text(format!(
            "The conversation history before this point was compacted into the following summary:\n\n\
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
pub async fn llm_compact(
    messages: Vec<Message>,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    keep_recent: Option<usize>,
) -> CompactionResult {
    let keep = keep_recent.unwrap_or(DEFAULT_KEEP_RECENT);

    if messages.len() <= keep {
        return CompactionResult {
            summarised_count: 0,
            summary: String::new(),
            messages,
        };
    }

    let split_at = messages.len() - keep;
    let old_messages = &messages[..split_at];

    // build the prompt for the LLM
    let conversation_dump = build_compaction_prompt(old_messages);

    let context = LlmContext {
        system_prompt: Some(COMPACTION_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(format!(
                "<conversation>\n{conversation_dump}\n</conversation>\n\n{SUMMARISATION_INSTRUCTIONS}"
            )),
            timestamp_ms: Timestamp::now(),
        })],
        tools: vec![],
    };

    // use reduced max_tokens for the summary, no thinking
    let mut compact_options = options.clone();
    compact_options.max_tokens = Some(mush_ai::types::TokenCount::new(4096));
    compact_options.thinking = None;

    let summary = match call_for_text(registry, model, &context, &compact_options).await {
        Some(text) => {
            tracing::info!(chars = text.len(), "LLM compaction succeeded");
            text
        }
        None => {
            tracing::warn!("LLM compaction failed, using structured fallback");
            // fallback: structured dump (no LLM)
            let prompt = build_compaction_prompt(old_messages);
            format!(
                "## Summary of earlier conversation\n\n\
                 The following is a condensed summary of the conversation so far:\n\n\
                 {prompt}"
            )
        }
    };

    compact_with_summary(messages, &summary, keep_recent)
}

/// make a simple LLM call and collect the text response
async fn call_for_text(
    registry: &ApiRegistry,
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> Option<String> {
    let stream_future = registry.stream(model, context, options).ok()?;
    let mut stream = stream_future.await.ok()?;

    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta, .. } => text.push_str(&delta),
            StreamEvent::Done { .. } => break,
            StreamEvent::Error { .. } => return None,
            _ => {}
        }
    }

    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp_ms: Timestamp::zero(),
        })
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![AssistantContentPart::Text(TextContent {
                text: text.into(),
            })],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::AnthropicMessages,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        })
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

        let result = llm_compact(msgs, &registry, &model, &options, Some(5)).await;
        // split_at = 15, which is an assistant msg (not ToolResult), so stays at 15
        assert_eq!(result.summarised_count, 15);
        assert_eq!(result.messages.len(), 6); // 1 summary + 5 kept
        assert!(result.summary.contains("Summary of earlier conversation"));
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
