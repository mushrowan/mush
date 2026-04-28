pub mod anthropic;
pub(crate) mod bench_support;
pub mod openai;
pub mod openai_responses;
pub mod sse;

use crate::registry::ApiRegistry;
use crate::types::{
    AssistantMessage, Message, Model, ThinkingLevel, ToolResultMessage, ToolResultTrimming,
    TurnPivot, UserMessage, find_recent_boundary,
};

/// max chars for tool results in older turns
const TRIM_TOOL_OUTPUT_CHARS: usize = 1500;
/// number of recent user messages whose tool results are kept at full size
const RECENT_TURNS_TO_KEEP: usize = 3;

/// format an error with its full source chain (e.g. "outer: middle: root cause")
pub(crate) fn format_error_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        msg.push_str(": ");
        msg.push_str(&cause.to_string());
        source = cause.source();
    }
    msg
}

/// find the message index at which "recent" turns begin (sliding window boundary)
fn recent_boundary(messages: &[Message]) -> usize {
    find_recent_boundary(messages, TurnPivot::User, RECENT_TURNS_TO_KEEP).unwrap_or(0)
}

/// visitor over a slice of `Message` values, used by provider request
/// builders to share the iteration + boundary tracking logic while keeping
/// provider-specific emit decisions in focused trait impls
pub(crate) trait MessageVisitor {
    fn on_user(&mut self, msg: &UserMessage);
    fn on_assistant(&mut self, msg: &AssistantMessage);
    fn on_tool_result(&mut self, msg: &ToolResultMessage, is_old_turn: bool);
}

/// walk messages dispatching to a visitor, passing `is_old_turn=true` for
/// tool results before the sliding window boundary (see `recent_boundary`)
pub(crate) fn walk_messages<V: MessageVisitor + ?Sized>(messages: &[Message], visitor: &mut V) {
    let boundary = recent_boundary(messages);
    for (i, msg) in messages.iter().enumerate() {
        let is_old_turn = i < boundary;
        match msg {
            Message::User(user) => visitor.on_user(user),
            Message::Assistant(asst) => visitor.on_assistant(asst),
            Message::ToolResult(tr) => visitor.on_tool_result(tr, is_old_turn),
        }
    }
}

/// trim a tool result string, keeping head and tail previews
pub(crate) fn trim_tool_output(text: &str) -> String {
    if text.len() <= TRIM_TOOL_OUTPUT_CHARS {
        return text.to_string();
    }
    let preview_end = text.floor_char_boundary(TRIM_TOOL_OUTPUT_CHARS / 2);
    let tail_start = text.len().saturating_sub(TRIM_TOOL_OUTPUT_CHARS / 4);
    let tail_start = text.ceil_char_boundary(tail_start);
    let trimmed = text.len() - preview_end - (text.len() - tail_start);
    format!(
        "{}\n\n[... {} chars trimmed from old tool result ...]\n\n{}",
        &text[..preview_end],
        trimmed,
        &text[tail_start..]
    )
}

/// conditionally trim a tool result based on whether it's in an old turn
/// and the active trimming strategy
pub(crate) fn maybe_trim_tool_output(
    text: &str,
    is_old_turn: bool,
    trimming: ToolResultTrimming,
) -> String {
    if is_old_turn && trimming == ToolResultTrimming::SlidingWindow {
        trim_tool_output(text)
    } else {
        text.to_string()
    }
}

/// normalise a tool name for case-insensitive, underscore-insensitive matching.
/// `"web_search"` → `"websearch"`, `"WebSearch"` → `"websearch"`.
/// shared by the anthropic provider (claude code name mapping) and
/// `ToolKey` in mush-agent.
pub fn normalize_tool_name(name: &str) -> String {
    name.to_lowercase().replace('_', "")
}

/// tracks state for a content block being streamed.
/// shared across all three providers. the `signature` field on `Thinking`
/// is only used by anthropic but harmless as `None` for openai providers.
#[derive(Debug, Clone)]
pub(crate) enum StreamBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        args_buf: String,
    },
}

/// map a thinking level to an openai-style reasoning effort string.
/// shared by the openai completions and openai responses providers.
pub(crate) fn openai_reasoning_effort(model: &Model, level: ThinkingLevel) -> Option<String> {
    if level == ThinkingLevel::Off || !model.reasoning {
        return None;
    }

    Some(match level {
        ThinkingLevel::Off => unreachable!(),
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low".into(),
        ThinkingLevel::Medium => "medium".into(),
        ThinkingLevel::High => "high".into(),
        ThinkingLevel::Xhigh => "xhigh".into(),
    })
}

/// check an HTTP response for errors, logging metadata and returning a typed
/// error on non-success status. returns the response unchanged on success.
/// shared by all three providers to avoid ~30 lines of identical boilerplate.
pub(crate) async fn check_response(
    response: reqwest::Response,
    api_label: &'static str,
    model_id: &str,
    url: &str,
) -> Result<reqwest::Response, crate::registry::ProviderError> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<missing>")
        .to_string();
    let header_names: Vec<_> = response
        .headers()
        .keys()
        .map(reqwest::header::HeaderName::as_str)
        .collect();

    tracing::debug!(
        model = %model_id,
        %url,
        %status,
        content_type,
        ?header_names,
        "received {api_label} response"
    );
    if content_type == "<missing>" {
        tracing::warn!(
            model = %model_id,
            %url,
            ?header_names,
            "{api_label} response missing content-type header"
        );
    }

    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        tracing::error!(%status, %content_type, body = %text, "{api_label} API error");
        return Err(crate::registry::ProviderError::ApiError {
            api: api_label,
            status,
            content_type,
            body: crate::registry::truncate_error_body(&text),
        });
    }

    Ok(response)
}

/// register all built-in api providers
pub fn register_builtins(registry: &mut ApiRegistry, client: reqwest::Client) {
    registry.register(Box::new(anthropic::AnthropicProvider {
        client: client.clone(),
    }));
    registry.register(Box::new(openai::OpenaiCompletionsProvider {
        client: client.clone(),
    }));
    registry.register(Box::new(openai_responses::OpenaiResponsesProvider {
        client,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn reasoning_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::Custom("test".into()),
            base_url: "https://example.com".into(),
            reasoning: true,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(16384),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }

    #[test]
    fn normalize_tool_name_strips_case_and_underscores() {
        assert_eq!(super::normalize_tool_name("WebSearch"), "websearch");
        assert_eq!(super::normalize_tool_name("web_search"), "websearch");
        assert_eq!(super::normalize_tool_name("Read"), "read");
        assert_eq!(super::normalize_tool_name("BASH"), "bash");
    }

    #[test]
    fn openai_reasoning_effort_maps_levels() {
        let model = reasoning_model();
        assert_eq!(openai_reasoning_effort(&model, ThinkingLevel::Off), None);
        assert_eq!(
            openai_reasoning_effort(&model, ThinkingLevel::Low),
            Some("low".into())
        );
        assert_eq!(
            openai_reasoning_effort(&model, ThinkingLevel::Medium),
            Some("medium".into())
        );
        assert_eq!(
            openai_reasoning_effort(&model, ThinkingLevel::High),
            Some("high".into())
        );
        assert_eq!(
            openai_reasoning_effort(&model, ThinkingLevel::Xhigh),
            Some("xhigh".into())
        );
    }

    #[test]
    fn openai_reasoning_effort_none_for_non_reasoning_model() {
        let model = Model {
            reasoning: false,
            ..reasoning_model()
        };
        assert_eq!(openai_reasoning_effort(&model, ThinkingLevel::High), None);
    }

    #[tokio::test]
    async fn check_response_passes_success() {
        let http_resp = http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(bytes::Bytes::new())
            .unwrap();
        let resp = reqwest::Response::from(http_resp);
        let result = check_response(resp, "test", "model-1", "http://example.com").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_response_returns_error_for_failure() {
        let http_resp = http::Response::builder()
            .status(400)
            .header("content-type", "application/json")
            .body(bytes::Bytes::from(r#"{"error": "bad request"}"#))
            .unwrap();
        let resp = reqwest::Response::from(http_resp);
        let result = check_response(resp, "test-api", "model-1", "http://example.com").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::registry::ProviderError::ApiError { api, status, .. } => {
                assert_eq!(api, "test-api");
                assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
            }
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    #[test]
    fn walk_messages_dispatches_and_marks_old_turns() {
        fn u(text: &str) -> Message {
            Message::User(UserMessage {
                content: UserContent::Text(text.into()),
                timestamp_ms: Timestamp::now(),
            })
        }
        fn a() -> Message {
            Message::Assistant(AssistantMessage {
                content: vec![],
                model: "x".into(),
                provider: Provider::Custom("x".into()),
                api: Api::OpenaiCompletions,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp_ms: Timestamp::now(),
            })
        }
        fn tr(id: &str) -> Message {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "tool".into(),
                content: vec![],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::now(),
            })
        }

        // 4 user turns with tool results between them; boundary keeps the last
        // 3 user turns "recent" so tr("t1") is the only old tool result
        let messages = vec![
            u("first"),
            a(),
            tr("t1"),
            u("second"),
            a(),
            tr("t2"),
            u("third"),
            a(),
            tr("t3"),
            u("fourth"),
        ];

        #[derive(Default)]
        struct Recorder {
            events: Vec<String>,
        }
        impl MessageVisitor for Recorder {
            fn on_user(&mut self, msg: &UserMessage) {
                self.events.push(format!("user:{}", msg.text()));
            }
            fn on_assistant(&mut self, _msg: &AssistantMessage) {
                self.events.push("asst".into());
            }
            fn on_tool_result(&mut self, msg: &ToolResultMessage, is_old_turn: bool) {
                self.events.push(format!(
                    "tr:{}:old={is_old_turn}",
                    msg.tool_call_id.as_str()
                ));
            }
        }

        let mut rec = Recorder::default();
        walk_messages(&messages, &mut rec);

        assert_eq!(
            rec.events,
            vec![
                "user:first",
                "asst",
                "tr:t1:old=true",
                "user:second",
                "asst",
                "tr:t2:old=false",
                "user:third",
                "asst",
                "tr:t3:old=false",
                "user:fourth",
            ]
        );
    }

    #[test]
    fn format_error_chain_includes_sources() {
        let inner = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "unexpected eof");
        let outer: Box<dyn std::error::Error> = "decoding response body".to_string().into();
        // simulate a chain by using a custom error
        #[derive(Debug)]
        struct Chained {
            msg: String,
            source: std::io::Error,
        }
        impl std::fmt::Display for Chained {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.msg)
            }
        }
        impl std::error::Error for Chained {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.source)
            }
        }

        let chained = Chained {
            msg: "error decoding response body".into(),
            source: inner,
        };
        let result = super::format_error_chain(&chained);
        assert_eq!(result, "error decoding response body: unexpected eof");

        // single error with no source
        let result = super::format_error_chain(&*outer);
        assert_eq!(result, "decoding response body");
    }
}
