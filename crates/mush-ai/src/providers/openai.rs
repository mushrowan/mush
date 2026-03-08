//! openai chat completions API provider
//!
//! supports the standard /v1/chat/completions endpoint used by openai,
//! openrouter, xai, groq, and many other providers.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::env::env_api_key;
use crate::registry::{
    ApiProvider, EventStream, LlmContext, ProviderError, StreamResult, ToolDefinition,
};
use crate::stream::StreamEvent;
use crate::types::*;

pub struct OpenaiCompletionsProvider;

impl ApiProvider for OpenaiCompletionsProvider {
    fn api(&self) -> Api {
        Api::OpenaiCompletions
    }

    fn stream(&self, model: &Model, context: &LlmContext, options: &StreamOptions) -> StreamResult {
        let model = model.clone();
        let context_messages = context.messages.clone();
        let system_prompt = context.system_prompt.clone();
        let tools = context.tools.clone();
        let options = options.clone();

        Box::pin(async move {
            let api_key = options
                .api_key
                .clone()
                .or_else(|| env_api_key(&model.provider))
                .ok_or_else(|| ProviderError::MissingApiKey(model.provider.clone()))?;
            let api_key_str = api_key.expose();

            let client = reqwest::Client::new();
            let body =
                build_request_body(&model, &system_prompt, &context_messages, &tools, &options);

            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key_str}"))?,
            );

            let base_url = &model.base_url;
            let url = format!("{base_url}/chat/completions");

            tracing::debug!(model = %model.id, %url, "sending openai completions request");

            let response = client
                .post(&url)
                .headers(headers)
                .json(&body)
                .send()
                .await?;

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
                model = %model.id,
                %url,
                %status,
                content_type,
                ?header_names,
                "received openai completions response"
            );
            if content_type == "<missing>" {
                tracing::warn!(model = %model.id, %url, ?header_names, "openai completions response missing content-type header");
            }

            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                tracing::error!(%status, body = %text, "openai completions API error");
                return Err(ProviderError::ApiError {
                    api: "openai completions",
                    status,
                    body: text,
                });
            }

            let model_id = model.id.clone();
            let provider = model.provider.clone();
            let api = model.api;

            Ok(parse_sse_stream(response, model_id, provider, api))
        })
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

    // conversation messages (trim old tool results to save context)
    let boundary = {
        let mut user_count = 0;
        let mut b = 0;
        for (i, msg) in messages.iter().enumerate().rev() {
            if matches!(msg, Message::User(_)) {
                user_count += 1;
                if user_count >= 3 {
                    b = i;
                    break;
                }
            }
        }
        b
    };

    for (msg_idx, msg) in messages.iter().enumerate() {
        let is_old_turn = msg_idx < boundary;
        match msg {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => serde_json::json!(text),
                    UserContent::Parts(parts) => {
                        let blocks: Vec<serde_json::Value> = parts
                            .iter()
                            .map(|part| match part {
                                UserContentPart::Text(t) => serde_json::json!({
                                    "type": "text",
                                    "text": t.text,
                                }),
                                UserContentPart::Image(img) => serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{};base64,{}", img.mime_type, img.data),
                                    }
                                }),
                            })
                            .collect();
                        serde_json::Value::Array(blocks)
                    }
                };
                all_messages.push(serde_json::json!({
                    "role": "user",
                    "content": content,
                }));
            }
            Message::Assistant(asst) => {
                let mut msg_obj = serde_json::json!({ "role": "assistant" });

                // text content
                let text_parts: Vec<&str> = asst
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        AssistantContentPart::Text(t) if !t.text.is_empty() => {
                            Some(t.text.as_str())
                        }
                        _ => None,
                    })
                    .collect();

                if !text_parts.is_empty() {
                    msg_obj["content"] = serde_json::json!(text_parts.join(""));
                } else {
                    msg_obj["content"] = serde_json::Value::Null;
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

                all_messages.push(msg_obj);
            }
            Message::ToolResult(tr) => {
                let raw_text = tr
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                // trim large tool results from older turns
                let text = if is_old_turn && raw_text.len() > 1500 {
                    let preview_end = raw_text.floor_char_boundary(750);
                    let tail_start = raw_text.ceil_char_boundary(raw_text.len().saturating_sub(375));
                    let trimmed = raw_text.len() - preview_end - (raw_text.len() - tail_start);
                    format!(
                        "{}\n\n[... {} chars trimmed from old tool result ...]\n\n{}",
                        &raw_text[..preview_end], trimmed, &raw_text[tail_start..]
                    )
                } else {
                    raw_text
                };

                all_messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tr.tool_call_id.as_str(),
                    "content": text,
                }));
            }
        }
    }

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

    let reasoning_effort = match options.thinking {
        Some(level) if level != ThinkingLevel::Off && model.reasoning => Some(match level {
            ThinkingLevel::Off => unreachable!(),
            ThinkingLevel::Minimal => "low".into(),
            ThinkingLevel::Low => "low".into(),
            ThinkingLevel::Medium => "medium".into(),
            ThinkingLevel::High => "high".into(),
            ThinkingLevel::Xhigh => "high".into(),
        }),
        _ => None,
    };

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

/// what kind of content block we're currently accumulating
#[derive(Debug)]
enum CurrentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        args_buf: String,
    },
}

fn parse_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
) -> EventStream {
    let event_stream = async_stream::stream! {
        let mut output = AssistantMessage {
            content: vec![],
            model: model_id.clone(),
            provider: provider_name.clone(),
            api: api.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::now(),
        };

        let mut current: Option<CurrentBlock> = None;
        let mut parser = super::sse::SseParser::new();

        use futures::TryStreamExt;
        let mut byte_stream = response.bytes_stream();

        yield StreamEvent::Start { partial: output.clone() };

        loop {
            match byte_stream.try_next().await {
                Ok(Some(chunk)) => {
                    let chunk_len = chunk.len();
                    let chunk_preview = super::sse::preview_bytes(&chunk, 240);
                    tracing::trace!(
                        model = %model_id,
                        provider = %provider_name,
                        api = ?api,
                        chunk_len,
                        chunk_preview = %chunk_preview,
                        "openai completions raw stream chunk"
                    );
                    for raw in parser.push(&chunk) {
                        tracing::trace!(
                            model = %model_id,
                            provider = %provider_name,
                            api = ?api,
                            event_name = raw.event.as_deref().unwrap_or("message"),
                            data_preview = %super::sse::preview_text(raw.data.trim(), 240),
                            "openai completions sse event"
                        );
                        if raw.data.trim() == "[DONE]" {
                            continue;
                        }
                        if let Ok(chunk) = serde_json::from_str::<ChunkResponse>(&raw.data) {
                            for event in process_chunk(chunk, &mut output, &mut current) {
                                yield event;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    finish_block(&mut current, &mut output);
                    output.stop_reason = StopReason::Error;
                    output.error_message = Some(e.to_string());
                    yield StreamEvent::Error {
                        reason: StopReason::Error,
                        message: output,
                    };
                    return;
                }
            }
        }

        // finish any remaining open block
        finish_block(&mut current, &mut output);

        yield StreamEvent::Done {
            reason: output.stop_reason,
            message: output,
        };
    };

    Box::pin(event_stream)
}

fn process_chunk(
    chunk: ChunkResponse,
    output: &mut AssistantMessage,
    current: &mut Option<CurrentBlock>,
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
            Some(CurrentBlock::Thinking { text }) => {
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
                *current = Some(CurrentBlock::Thinking {
                    text: reasoning.clone(),
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
            Some(CurrentBlock::Text { text: buf }) => {
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
                *current = Some(CurrentBlock::Text { text: text.clone() });
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
                *current = Some(CurrentBlock::ToolCall {
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
                && let Some(CurrentBlock::ToolCall { args_buf, .. }) = current.as_mut()
            {
                args_buf.push_str(&args);
                let idx = output.content.len().saturating_sub(1);
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args_buf)
                    && let Some(AssistantContentPart::ToolCall(tc)) = output.content.get_mut(idx)
                {
                    tc.arguments = parsed;
                }
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
    current: &mut Option<CurrentBlock>,
    output: &mut AssistantMessage,
    events: &mut Vec<StreamEvent>,
) -> Option<usize> {
    let block = current.take()?;
    let content_index = output.content.len().saturating_sub(1);
    match block {
        CurrentBlock::Text { text } => {
            events.push(StreamEvent::TextEnd {
                content_index,
                text,
            });
        }
        CurrentBlock::Thinking { text } => {
            events.push(StreamEvent::ThinkingEnd {
                content_index,
                thinking: text,
            });
        }
        CurrentBlock::ToolCall { id, name, args_buf } => {
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
fn finish_block(current: &mut Option<CurrentBlock>, output: &mut AssistantMessage) {
    if let Some(block) = current.take()
        && let CurrentBlock::ToolCall { args_buf, .. } = &block
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
        assert!(matches!(current, Some(CurrentBlock::Thinking { .. })));

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
