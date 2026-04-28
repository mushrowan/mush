//! openai chat completions API provider
//!
//! supports the standard /v1/chat/completions endpoint used by openai,
//! openrouter, xai, groq, and many other providers.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::env::env_api_key;
use crate::registry::{ApiProvider, EventStream, LlmContext, ProviderError, ToolDefinition};
use crate::stream::StreamEvent;
use crate::types::*;

pub struct OpenaiCompletionsProvider {
    pub client: reqwest::Client,
}

#[async_trait::async_trait]
impl ApiProvider for OpenaiCompletionsProvider {
    fn api(&self) -> Api {
        Api::OpenaiCompletions
    }

    async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Result<EventStream, ProviderError> {
        let api_key = options
            .api_key
            .clone()
            .or_else(|| env_api_key(&model.provider))
            .ok_or_else(|| ProviderError::MissingApiKey(model.provider.clone()))?;
        let api_key_str = api_key.expose();
        let body = build_request_body(
            model,
            &context.system_prompt,
            &context.messages,
            &context.tools,
            options,
        );

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let mut auth = HeaderValue::from_str(&format!("Bearer {api_key_str}"))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);

        let base_url = &model.base_url;
        let url = format!("{base_url}/chat/completions");

        tracing::debug!(model = %model.id, %url, "sending openai completions request");

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;

        let response =
            super::check_response(response, "openai completions", model.id.as_str(), &url).await?;

        Ok(parse_sse_stream(
            response,
            model.id.clone(),
            model.provider.clone(),
            model.api,
        ))
    }
}

// -- request body --

#[derive(Serialize)]
struct RequestBody {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<RequestTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    stream_options: StreamOpts,
}

#[derive(Serialize)]
struct StreamOpts {
    include_usage: bool,
}

#[derive(Serialize)]
struct RequestTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: RequestToolFunction,
}

#[derive(Serialize)]
struct RequestToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

fn build_request_body(
    model: &Model,
    system_prompt: &Option<String>,
    messages: &[Message],
    tools: &[ToolDefinition],
    options: &StreamOptions,
) -> RequestBody {
    let mut all_messages = Vec::new();

    // system message
    if let Some(prompt) = system_prompt {
        all_messages.push(serde_json::json!({
            "role": "system",
            "content": prompt,
        }));
    }

    // openai has automatic caching so always use SlidingWindow
    // (their caching handles prefix changes transparently)
    let mut visitor = OpenaiVisitor {
        trimming: ToolResultTrimming::SlidingWindow,
        out: &mut all_messages,
    };
    super::walk_messages(messages, &mut visitor);

    let converted_tools = if tools.is_empty() {
        None
    } else {
        Some(
            tools
                .iter()
                .map(|t| RequestTool {
                    tool_type: "function".into(),
                    function: RequestToolFunction {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.parameters.clone(),
                    },
                })
                .collect(),
        )
    };

    let reasoning_effort = options
        .thinking
        .and_then(|level| super::openai_reasoning_effort(model, level));

    RequestBody {
        model: model.id.to_string(),
        messages: all_messages,
        stream: true,
        max_completion_tokens: Some(options.max_tokens.unwrap_or(model.max_output_tokens).get()),
        temperature: if reasoning_effort.is_none() {
            options.temperature.map(|t| t.value())
        } else {
            None
        },
        tools: converted_tools,
        reasoning_effort,
        stream_options: StreamOpts {
            include_usage: true,
        },
    }
}

struct OpenaiVisitor<'a> {
    trimming: ToolResultTrimming,
    out: &'a mut Vec<serde_json::Value>,
}

impl super::MessageVisitor for OpenaiVisitor<'_> {
    fn on_user(&mut self, user: &UserMessage) {
        let content = match &user.content {
            UserContent::Text(text) => serde_json::json!(text),
            UserContent::Parts(parts) => {
                let blocks: Vec<serde_json::Value> = parts
                    .iter()
                    .filter_map(|part| match part {
                        UserContentPart::Text(t) if t.text.is_empty() => None,
                        UserContentPart::Text(t) => Some(serde_json::json!({
                            "type": "text",
                            "text": t.text,
                        })),
                        UserContentPart::Image(img) => Some(serde_json::json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", img.mime_type, img.data),
                            }
                        })),
                    })
                    .collect();
                serde_json::Value::Array(blocks)
            }
        };
        self.out.push(serde_json::json!({
            "role": "user",
            "content": content,
        }));
    }

    fn on_assistant(&mut self, asst: &AssistantMessage) {
        let mut msg_obj = serde_json::json!({ "role": "assistant" });

        // text content
        let text_parts: Vec<&str> = asst
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantContentPart::Text(t) if !t.text.is_empty() => Some(t.text.as_str()),
                _ => None,
            })
            .collect();

        if text_parts.is_empty() {
            msg_obj["content"] = serde_json::Value::Null;
        } else {
            msg_obj["content"] = serde_json::json!(text_parts.join(""));
        }

        // tool calls
        let tool_calls: Vec<serde_json::Value> = asst
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantContentPart::ToolCall(tc) => Some(serde_json::json!({
                    "id": tc.id.as_str(),
                    "type": "function",
                    "function": {
                        "name": tc.name.as_str(),
                        "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default(),
                    }
                })),
                _ => None,
            })
            .collect();

        if !tool_calls.is_empty() {
            msg_obj["tool_calls"] = serde_json::Value::Array(tool_calls);
        }

        self.out.push(msg_obj);
    }

    fn on_tool_result(&mut self, tr: &ToolResultMessage, is_old_turn: bool) {
        let raw_text = tr
            .content
            .iter()
            .filter_map(|p| match p {
                ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let text = super::maybe_trim_tool_output(&raw_text, is_old_turn, self.trimming);

        self.out.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": tr.tool_call_id.as_str(),
            "content": text,
        }));
    }
}

// -- SSE parsing --

#[derive(Debug, Deserialize)]
struct ChunkResponse {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<ChunkUsage>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ChunkToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChunkToolFunction>,
}

#[derive(Debug, Deserialize)]
struct ChunkToolFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

use super::StreamBlock;

struct OpenAiCompletionsProcessor {
    current: Option<StreamBlock>,
}

impl super::sse::SseProcessor for OpenAiCompletionsProcessor {
    fn process(
        &mut self,
        raw: &super::sse::SseRawEvent,
        output: &mut AssistantMessage,
    ) -> super::sse::ProcessResult {
        if raw.data.trim() == "[DONE]" {
            return super::sse::ProcessResult::Skip;
        }
        match serde_json::from_str::<ChunkResponse>(&raw.data) {
            Ok(chunk) => {
                let events = process_chunk(chunk, output, &mut self.current);
                super::sse::ProcessResult::Events(events)
            }
            Err(_) => super::sse::ProcessResult::Skip,
        }
    }

    fn finish(&mut self, output: &mut AssistantMessage) {
        finish_block(&mut self.current, output);
    }

    fn label(&self) -> &'static str {
        "openai completions"
    }
}

fn parse_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
) -> EventStream {
    let processor = OpenAiCompletionsProcessor { current: None };
    super::sse::run_sse_stream(response, model_id, provider_name, api, processor)
}

fn process_chunk(
    chunk: ChunkResponse,
    output: &mut AssistantMessage,
    current: &mut Option<StreamBlock>,
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    // handle usage
    if let Some(usage) = chunk.usage {
        let cached = usage
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens);
        output.usage = Usage {
            input_tokens: TokenCount::new(usage.prompt_tokens.saturating_sub(cached)),
            output_tokens: TokenCount::new(usage.completion_tokens),
            cache_read_tokens: TokenCount::new(cached),
            cache_write_tokens: TokenCount::ZERO,
        };
    }

    let Some(choice) = chunk.choices.into_iter().next() else {
        return events;
    };

    if let Some(reason) = choice.finish_reason {
        output.stop_reason = map_stop_reason(&reason);
    }

    // handle reasoning/thinking content
    if let Some(reasoning) = choice.delta.reasoning_content
        && !reasoning.is_empty()
    {
        match current {
            Some(StreamBlock::Thinking { text, .. }) => {
                text.push_str(&reasoning);
                let idx = output.content.len().saturating_sub(1);
                if let Some(AssistantContentPart::Thinking(tc)) = output.content.get_mut(idx)
                    && let Some(buf) = tc.text_mut()
                {
                    buf.push_str(&reasoning);
                }
                events.push(StreamEvent::ThinkingDelta {
                    content_index: idx,
                    delta: reasoning,
                });
            }
            _ => {
                let idx = finish_block_events(current, output, &mut events);
                let content_index = idx.unwrap_or(output.content.len());
                *current = Some(StreamBlock::Thinking {
                    text: reasoning.clone(),
                    signature: None,
                });
                output
                    .content
                    .push(AssistantContentPart::Thinking(ThinkingContent::Thinking {
                        thinking: reasoning.clone(),
                        signature: None,
                    }));
                events.push(StreamEvent::ThinkingStart { content_index });
                events.push(StreamEvent::ThinkingDelta {
                    content_index,
                    delta: reasoning,
                });
            }
        }
    }

    // handle text content
    if let Some(text) = choice.delta.content
        && !text.is_empty()
    {
        match current {
            Some(StreamBlock::Text { text: buf }) => {
                buf.push_str(&text);
                let idx = output.content.len().saturating_sub(1);
                if let Some(AssistantContentPart::Text(tc)) = output.content.get_mut(idx) {
                    tc.text.push_str(&text);
                }
                events.push(StreamEvent::TextDelta {
                    content_index: idx,
                    delta: text,
                });
            }
            _ => {
                let idx = finish_block_events(current, output, &mut events);
                let content_index = idx.unwrap_or(output.content.len());
                *current = Some(StreamBlock::Text { text: text.clone() });
                output.content.push(AssistantContentPart::Text(TextContent {
                    text: text.clone(),
                }));
                events.push(StreamEvent::TextStart { content_index });
                events.push(StreamEvent::TextDelta {
                    content_index,
                    delta: text,
                });
            }
        }
    }

    // handle tool calls
    if let Some(tool_calls) = choice.delta.tool_calls {
        for tc in tool_calls {
            let new_tool = tc.id.is_some();
            if new_tool {
                finish_block_events(current, output, &mut events);
                let id = tc.id.unwrap_or_default();
                let name = tc
                    .function
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .unwrap_or_default();
                let content_index = output.content.len();
                *current = Some(StreamBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    args_buf: String::new(),
                });
                output
                    .content
                    .push(AssistantContentPart::ToolCall(ToolCall {
                        id: ToolCallId::from(id),
                        name: ToolName::from(name),
                        arguments: serde_json::Value::Object(Default::default()),
                    }));
                events.push(StreamEvent::ToolCallStart { content_index });
            }

            if let Some(func) = tc.function
                && let Some(args) = func.arguments
                && let Some(StreamBlock::ToolCall { args_buf, .. }) = current.as_mut()
            {
                args_buf.push_str(&args);
                let idx = output.content.len().saturating_sub(1);
                events.push(StreamEvent::ToolCallDelta {
                    content_index: idx,
                    delta: args,
                });
            }
        }
    }

    events
}

/// finish the current block, emitting end events and returning the content index
fn finish_block_events(
    current: &mut Option<StreamBlock>,
    output: &mut AssistantMessage,
    events: &mut Vec<StreamEvent>,
) -> Option<usize> {
    let block = current.take()?;
    let content_index = output.content.len().saturating_sub(1);
    match block {
        StreamBlock::Text { text } => {
            events.push(StreamEvent::TextEnd {
                content_index,
                text,
            });
        }
        StreamBlock::Thinking { text, .. } => {
            events.push(StreamEvent::ThinkingEnd {
                content_index,
                thinking: text,
            });
        }
        StreamBlock::ToolCall { id, name, args_buf } => {
            let arguments = serde_json::from_str(&args_buf)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            if let Some(AssistantContentPart::ToolCall(tc)) = output.content.get_mut(content_index)
            {
                tc.arguments = arguments.clone();
            }
            events.push(StreamEvent::ToolCallEnd {
                content_index,
                id,
                name,
                arguments,
            });
        }
    }
    Some(content_index)
}

/// finish the current block without producing events (for stream end)
fn finish_block(current: &mut Option<StreamBlock>, output: &mut AssistantMessage) {
    if let Some(block) = current.take()
        && let StreamBlock::ToolCall { args_buf, .. } = &block
    {
        let idx = output.content.len().saturating_sub(1);
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args_buf)
            && let Some(AssistantContentPart::ToolCall(tc)) = output.content.get_mut(idx)
        {
            tc.arguments = parsed;
        }
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Stop,
        "length" => StopReason::Length,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

#[doc(hidden)]
pub fn benchmark_tool_call_deltas(chunk_count: usize, arg_bytes: usize) -> usize {
    let (_, fragments) = super::bench_support::tool_call_json_fragments(chunk_count, arg_bytes);
    let mut output = AssistantMessage {
        content: vec![],
        model: "bench".into(),
        provider: Provider::Custom("bench".into()),
        api: Api::OpenaiCompletions,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp_ms: Timestamp::zero(),
    };
    let mut current = None;

    for (index, fragment) in fragments.into_iter().enumerate() {
        let chunk = ChunkResponse {
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    tool_calls: Some(vec![ChunkToolCall {
                        id: (index == 0).then(|| "tc_1".into()),
                        function: Some(ChunkToolFunction {
                            name: (index == 0).then(|| "read".into()),
                            arguments: Some(fragment),
                        }),
                    }]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let _ = process_chunk(chunk, &mut output, &mut current);
    }

    let mut sink = Vec::new();
    let _ = finish_block_events(&mut current, &mut output, &mut sink);

    match output.content.first() {
        Some(AssistantContentPart::ToolCall(tc)) => tc
            .arguments
            .get("payload")
            .and_then(|value| value.as_str())
            .map_or(0, str::len),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason("stop"), StopReason::Stop);
        assert_eq!(map_stop_reason("length"), StopReason::Length);
        assert_eq!(map_stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("content_filter"), StopReason::Error);
        assert_eq!(map_stop_reason("unknown"), StopReason::Stop);
    }

    #[test]
    fn build_body_basic() {
        let model = test_model();
        let options = StreamOptions::default();

        let body = build_request_body(
            &model,
            &Some("you are helpful".into()),
            &[Message::User(UserMessage {
                content: UserContent::Text("hi".into()),
                timestamp_ms: Timestamp::zero(),
            })],
            &[],
            &options,
        );

        assert_eq!(body.model, model.id.as_str());
        assert!(body.stream);
        // system + user = 2 messages
        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0]["role"], "system");
        assert_eq!(body.messages[1]["role"], "user");
        assert!(body.reasoning_effort.is_none());
    }

    #[test]
    fn build_body_with_reasoning() {
        let model = Model {
            reasoning: true,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options);
        assert_eq!(body.reasoning_effort, Some("high".into()));
        assert!(body.temperature.is_none());
    }

    #[test]
    fn build_body_with_tools() {
        let model = test_model();
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        }];

        let body = build_request_body(&model, &None, &[], &tools, &StreamOptions::default());
        assert!(body.tools.is_some());
        let tools = body.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "read");
    }

    #[test]
    fn convert_tool_result_in_messages() {
        let model = test_model();
        let messages = vec![
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::ToolCall(ToolCall {
                    id: "tc_1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
                })],
                model: "test".into(),
                provider: Provider::Custom("test".into()),
                api: Api::OpenaiCompletions,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp_ms: Timestamp::zero(),
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_1".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "file contents here".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
        ];

        let body = build_request_body(&model, &None, &messages, &[], &StreamOptions::default());
        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0]["role"], "assistant");
        assert!(body.messages[0]["tool_calls"].is_array());
        assert_eq!(body.messages[1]["role"], "tool");
        assert_eq!(body.messages[1]["tool_call_id"], "tc_1");
    }

    #[test]
    fn process_text_chunk() {
        let mut output = test_output();
        let mut current = None;

        let chunk = ChunkResponse {
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    content: Some("hello".into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = process_chunk(chunk, &mut output, &mut current);
        assert!(events.len() >= 2); // TextStart + TextDelta
        assert!(matches!(events[0], StreamEvent::TextStart { .. }));
        assert!(matches!(events[1], StreamEvent::TextDelta { .. }));
        assert!(current.is_some());
    }

    #[test]
    fn process_reasoning_then_text() {
        let mut output = test_output();
        let mut current = None;

        // reasoning chunk
        let chunk = ChunkResponse {
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    reasoning_content: Some("thinking...".into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        process_chunk(chunk, &mut output, &mut current);
        assert!(matches!(current, Some(StreamBlock::Thinking { .. })));

        // then text
        let chunk = ChunkResponse {
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    content: Some("answer".into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = process_chunk(chunk, &mut output, &mut current);
        // should have ThinkingEnd, TextStart, TextDelta
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ThinkingEnd { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextStart { .. }))
        );
        assert_eq!(output.content.len(), 2);
    }

    #[test]
    fn process_tool_call_chunk() {
        let mut output = test_output();
        let mut current = None;

        // tool call start
        let chunk = ChunkResponse {
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    tool_calls: Some(vec![ChunkToolCall {
                        id: Some("tc_1".into()),
                        function: Some(ChunkToolFunction {
                            name: Some("read".into()),
                            arguments: Some(r#"{"path":"#.into()),
                        }),
                    }]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = process_chunk(chunk, &mut output, &mut current);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        );

        // tool call arg continuation
        let chunk = ChunkResponse {
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    tool_calls: Some(vec![ChunkToolCall {
                        id: None,
                        function: Some(ChunkToolFunction {
                            name: None,
                            arguments: Some(r#""foo.rs"}"#.into()),
                        }),
                    }]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        process_chunk(chunk, &mut output, &mut current);

        match &output.content[0] {
            AssistantContentPart::ToolCall(tc) => {
                assert_eq!(tc.arguments, serde_json::json!({}));
            }
            other => panic!("expected tool call, got {other:?}"),
        }

        // finish
        let mut events = Vec::new();
        finish_block_events(&mut current, &mut output, &mut events);

        match &events[0] {
            StreamEvent::ToolCallEnd {
                name, arguments, ..
            } => {
                assert_eq!(name, "read");
                assert_eq!(arguments["path"], "foo.rs");
            }
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
    }

    #[test]
    fn process_usage_chunk() {
        let mut output = test_output();
        let mut current = None;

        let chunk = ChunkResponse {
            choices: vec![],
            usage: Some(ChunkUsage {
                prompt_tokens: 150,
                completion_tokens: 50,
                prompt_tokens_details: Some(PromptTokenDetails { cached_tokens: 100 }),
            }),
        };

        process_chunk(chunk, &mut output, &mut current);
        assert_eq!(output.usage.input_tokens, TokenCount::new(50)); // 150 - 100 cached
        assert_eq!(output.usage.output_tokens, TokenCount::new(50));
        assert_eq!(output.usage.cache_read_tokens, TokenCount::new(100));
    }

    #[test]
    fn benchmark_tool_call_deltas_returns_payload_size() {
        assert_eq!(benchmark_tool_call_deltas(8, 1024), 1024);
    }

    fn test_model() -> Model {
        Model {
            id: "anthropic/claude-sonnet-4".into(),
            name: "Claude Sonnet 4 (OpenRouter)".into(),
            api: Api::OpenaiCompletions,
            provider: Provider::OpenRouter,
            base_url: "https://openrouter.ai/api/v1".into(),
            reasoning: false,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(16384),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        }
    }

    fn test_output() -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::OpenaiCompletions,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        }
    }
}
