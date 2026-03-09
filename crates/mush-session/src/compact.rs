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

/// estimate token count for a message list (rough: 4 chars per token)
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

fn estimate_message_tokens(msg: &Message) -> usize {
    let chars = match msg {
        Message::User(u) => match &u.content {
            UserContent::Text(t) => t.len(),
            UserContent::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    UserContentPart::Text(t) => t.text.len(),
                    UserContentPart::Image(_) => 1000, // rough estimate for images
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
                ToolResultContentPart::Image(_) => 1000,
            })
            .sum(),
    };
    chars / 4
}

fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let keep = max_chars.saturating_sub(3);
    let truncated: String = text.chars().take(keep).collect();
    format!("{truncated}...")
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

/// check whether compaction is needed (estimated tokens > 75% of context window)
pub fn needs_compaction(messages: &[Message], context_window: usize) -> bool {
    let threshold = context_window * 3 / 4;
    messages.len() > DEFAULT_KEEP_RECENT && estimate_tokens(messages) > threshold
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
        let msgs = vec![user_msg("hi"), assistant_msg("hello")];
        // small messages, huge window — no compaction needed
        assert!(!needs_compaction(&msgs, 200_000));
    }

    #[test]
    fn needs_compaction_above_threshold() {
        // each message ~250 tokens (1000 chars / 4), 20 messages = ~5000 tokens
        let msgs: Vec<Message> = (0..20)
            .map(|i| {
                if i % 2 == 0 {
                    user_msg(&"x".repeat(1000))
                } else {
                    assistant_msg(&"y".repeat(1000))
                }
            })
            .collect();
        // context window of 4000 tokens, 75% threshold = 3000
        // 20 msgs * 250 tokens = 5000, which exceeds 3000
        assert!(needs_compaction(&msgs, 4000));
    }

    #[test]
    fn needs_compaction_too_few_messages() {
        // even if tokens are high, don't compact if <= keep_recent (10)
        let msgs: Vec<Message> = (0..8).map(|_| user_msg(&"x".repeat(10_000))).collect();
        assert!(!needs_compaction(&msgs, 100));
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
        // context window sized so that kept messages fit comfortably,
        // but a full conversation exceeds 75%
        let context_window = 1000;
        let keep = 4; // keep fewer messages to make post-compaction smaller

        // build enough messages to exceed 75% threshold (750 tokens)
        let mut msgs: Vec<Message> = (0..20)
            .flat_map(|i| {
                vec![
                    user_msg(&format!("question {i}: here is a detailed question with enough text to accumulate significant token count")),
                    assistant_msg(&format!("answer {i}: here is a correspondingly detailed response with enough text to push the total")),
                ]
            })
            .collect();

        assert!(
            needs_compaction(&msgs, context_window),
            "should need compaction: {} tokens, threshold {}",
            estimate_tokens(&msgs),
            context_window * 3 / 4,
        );

        // compact with a short summary
        let result = compact_with_summary(msgs, "brief summary", Some(keep));
        msgs = result.messages;

        // right after compaction, shouldn't need it again
        // (summary + few kept messages should be well under threshold)
        assert!(
            !needs_compaction(&msgs, context_window),
            "shouldn't need compaction right after: {} tokens in {} msgs, threshold {}",
            estimate_tokens(&msgs),
            msgs.len(),
            context_window * 3 / 4,
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

        assert!(
            needs_compaction(&msgs, context_window),
            "should need re-compaction: {} tokens in {} msgs, threshold {}",
            estimate_tokens(&msgs),
            msgs.len(),
            context_window * 3 / 4,
        );

        // compact again
        let result2 =
            compact_with_summary(msgs, "updated summary including follow-ups", Some(keep));
        assert!(
            result2.summarised_count > 0,
            "should have summarised some messages"
        );
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
}
