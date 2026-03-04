//! anthropic messages API provider
//!
//! streams responses via SSE from the anthropic messages endpoint.
//! supports extended thinking, tool use, and image inputs.

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::env::anthropic_api_key;
use crate::registry::{
    ApiProvider, EventStream, LlmContext, ProviderError, StreamResult, ToolDefinition,
};
use crate::stream::StreamEvent;
use crate::types::*;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const ANTHROPIC_DIRECT_API: &str = "api.anthropic.com";

// stealth mode: mimic claude code's identity for oauth
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

// claude code tool names (canonical casing)
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "Find",
    "Ls",
    "Batch",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// convert tool name to claude code canonical casing if it matches (case-insensitive)
fn to_claude_code_name(name: &str) -> String {
    // strip underscores for comparison (web_search → websearch → WebSearch)
    let normalised = name.to_lowercase().replace('_', "");
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|t| t.to_lowercase() == normalised)
        .map(|t| t.to_string())
        .unwrap_or_else(|| name.to_string())
}

/// convert claude code tool name back to our tool name using the tool definitions
fn from_claude_code_name(name: &str, tools: &[ToolDefinition]) -> String {
    // strip underscores for comparison (WebSearch → websearch, web_search → websearch)
    let normalised = name.to_lowercase().replace('_', "");
    tools
        .iter()
        .find(|t| t.name.to_lowercase().replace('_', "") == normalised)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| name.to_string())
}

pub struct AnthropicProvider;

impl ApiProvider for AnthropicProvider {
    fn api(&self) -> Api {
        Api::AnthropicMessages
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
                .or_else(anthropic_api_key)
                .ok_or_else(|| ProviderError::MissingApiKey(Provider::Anthropic))?;

            let is_oauth = api_key.is_oauth_token();
            let client = reqwest::Client::new();

            let body = build_request_body(
                &model,
                &system_prompt,
                &context_messages,
                &tools,
                &options,
                is_oauth,
            );

            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
            headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));

            if is_oauth {
                headers.insert(
                    "anthropic-beta",
                    HeaderValue::from_static(
                        "claude-code-20250219,oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14",
                    ),
                );
                headers.insert(
                    "user-agent",
                    HeaderValue::from_static(concat!("claude-cli/", "2.1.62")),
                );
                headers.insert("x-app", HeaderValue::from_static("cli"));
                let key = api_key.expose();
                headers.insert(
                    "authorization",
                    HeaderValue::from_str(&format!("Bearer {key}"))?,
                );
            } else {
                let key = api_key.expose();
                headers.insert(
                    "anthropic-beta",
                    HeaderValue::from_static(
                        "fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14",
                    ),
                );
                headers.insert("x-api-key", HeaderValue::from_str(key)?);
            }

            let base_url = if model.base_url.is_empty() {
                DEFAULT_BASE_URL
            } else {
                &model.base_url
            };
            let url = format!("{base_url}/v1/messages");

            tracing::debug!(model = %model.id, %url, oauth = is_oauth, "sending anthropic request");

            let response = client
                .post(&url)
                .headers(headers)
                .json(&body)
                .send()
                .await?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                tracing::error!(%status, body = %text, "anthropic API error");
                return Err(ProviderError::ApiError {
                    api: "anthropic",
                    status,
                    body: text,
                });
            }

            let model_id = model.id.clone();
            let provider_name = model.provider.clone();
            let api = model.api;
            let sse_stream =
                parse_sse_stream(response, model_id, provider_name, api, is_oauth, tools);

            Ok(sse_stream)
        })
    }
}

// -- request body construction --

#[derive(Serialize)]
struct RequestBody {
    model: String,
    messages: Vec<RequestMessage>,
    max_tokens: u64,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<RequestTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    control_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ThinkingConfig {
    Enabled {
        #[serde(rename = "type")]
        config_type: String,
        budget_tokens: u64,
    },
    Adaptive {
        #[serde(rename = "type")]
        config_type: String,
    },
}

#[derive(Serialize)]
struct RequestMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Serialize)]
struct RequestTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

fn build_request_body(
    model: &Model,
    system_prompt: &Option<String>,
    messages: &[Message],
    tools: &[ToolDefinition],
    options: &StreamOptions,
    is_oauth: bool,
) -> RequestBody {
    let max_tokens = options.max_tokens.unwrap_or(model.max_output_tokens);
    let cache_control = anthropic_cache_control(model.base_url.as_str(), options.cache_retention);

    // oauth requires claude code identity as first system block
    let system = if is_oauth {
        let mut blocks = vec![SystemBlock {
            block_type: "text".into(),
            text: CLAUDE_CODE_IDENTITY.into(),
            cache_control: cache_control.clone(),
        }];
        if let Some(prompt) = system_prompt {
            blocks.push(SystemBlock {
                block_type: "text".into(),
                text: prompt.clone(),
                cache_control: cache_control.clone(),
            });
        }
        Some(blocks)
    } else {
        system_prompt.as_ref().map(|prompt| {
            vec![SystemBlock {
                block_type: "text".into(),
                text: prompt.clone(),
                cache_control: cache_control.clone(),
            }]
        })
    };

    let converted_messages = convert_messages(messages, is_oauth, cache_control.clone());

    let converted_tools = if tools.is_empty() {
        None
    } else {
        Some(
            tools
                .iter()
                .map(|t| RequestTool {
                    name: if is_oauth {
                        to_claude_code_name(&t.name)
                    } else {
                        t.name.clone()
                    },
                    description: t.description.clone(),
                    input_schema: t.parameters.clone(),
                })
                .collect(),
        )
    };

    let thinking = match options.thinking {
        Some(level) if level != ThinkingLevel::Off && model.reasoning => {
            if supports_adaptive_thinking(&model.id) {
                Some(ThinkingConfig::Adaptive {
                    config_type: "adaptive".into(),
                })
            } else {
                let budget = thinking_budget(level, max_tokens);
                Some(ThinkingConfig::Enabled {
                    config_type: "enabled".into(),
                    budget_tokens: budget,
                })
            }
        }
        _ => None,
    };

    // temperature is incompatible with thinking
    let temperature = if thinking.is_none() {
        options.temperature.map(|t| t.value())
    } else {
        None
    };

    // when thinking is enabled with budget, max_tokens must include the budget
    let effective_max_tokens =
        if let Some(ThinkingConfig::Enabled { budget_tokens, .. }) = &thinking {
            max_tokens.max(*budget_tokens + 1024)
        } else {
            max_tokens
        };

    RequestBody {
        model: model.id.to_string(),
        messages: converted_messages,
        max_tokens: effective_max_tokens,
        stream: true,
        system,
        tools: converted_tools,
        temperature,
        thinking,
        cache_control,
    }
}

fn anthropic_cache_control(
    base_url: &str,
    retention: Option<CacheRetention>,
) -> Option<CacheControl> {
    let retention = retention.unwrap_or(CacheRetention::Short);
    if retention == CacheRetention::None {
        return None;
    }

    let allow_ttl = base_url.contains(ANTHROPIC_DIRECT_API);
    let ttl = if retention == CacheRetention::Long && allow_ttl {
        Some("1h".to_string())
    } else {
        None
    };

    Some(CacheControl {
        control_type: "ephemeral".into(),
        ttl,
    })
}

/// opus 4.6 and sonnet 4.6 use adaptive thinking instead of budget tokens
fn supports_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("opus-4-6")
        || model_id.contains("opus-4.6")
        || model_id.contains("sonnet-4-6")
        || model_id.contains("sonnet-4.6")
}

fn thinking_budget(level: ThinkingLevel, max_tokens: u64) -> u64 {
    let base = max_tokens.max(4096);
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => base / 4,
        ThinkingLevel::Medium => base / 2,
        ThinkingLevel::High => base,
        ThinkingLevel::Xhigh => base * 2,
    }
}

fn convert_messages(
    messages: &[Message],
    is_oauth: bool,
    cache_control: Option<CacheControl>,
) -> Vec<RequestMessage> {
    let converted = convert_messages_raw(messages, is_oauth);
    let mut fixed = fix_orphaned_tool_calls(converted);
    apply_cache_control_to_last_user_message(&mut fixed, cache_control);
    fixed
}

/// raw conversion without orphan fixing
fn convert_messages_raw(messages: &[Message], is_oauth: bool) -> Vec<RequestMessage> {
    let mut result = Vec::new();

    for msg in messages {
        match msg {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => serde_json::Value::String(text.clone()),
                    UserContent::Parts(parts) => {
                        let blocks: Vec<serde_json::Value> = parts
                            .iter()
                            .map(|part| match part {
                                UserContentPart::Text(t) => serde_json::json!({
                                    "type": "text",
                                    "text": t.text,
                                }),
                                UserContentPart::Image(img) => serde_json::json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": img.mime_type,
                                        "data": img.data,
                                    }
                                }),
                            })
                            .collect();
                        serde_json::Value::Array(blocks)
                    }
                };
                result.push(RequestMessage {
                    role: "user".into(),
                    content,
                });
            }
            Message::Assistant(asst) => {
                let blocks: Vec<serde_json::Value> = asst
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContentPart::Text(t) => {
                            if t.text.is_empty() {
                                None
                            } else {
                                Some(serde_json::json!({
                                    "type": "text",
                                    "text": t.text,
                                }))
                            }
                        }
                        AssistantContentPart::Thinking(t) => match t {
                            ThinkingContent::Redacted { data } => Some(serde_json::json!({
                                "type": "redacted_thinking",
                                "data": data,
                            })),
                            ThinkingContent::Thinking {
                                thinking,
                                signature,
                            } => {
                                if thinking.is_empty() {
                                    None
                                } else if let Some(sig) = signature {
                                    Some(serde_json::json!({
                                        "type": "thinking",
                                        "thinking": thinking,
                                        "signature": sig,
                                    }))
                                } else {
                                    // no signature (eg aborted stream), send as text
                                    Some(serde_json::json!({
                                        "type": "text",
                                        "text": thinking,
                                    }))
                                }
                            }
                        },
                        AssistantContentPart::ToolCall(tc) => {
                            let name = if is_oauth {
                                to_claude_code_name(tc.name.as_str())
                            } else {
                                tc.name.as_str().to_string()
                            };
                            Some(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id.as_str(),
                                "name": name,
                                "input": tc.arguments,
                            }))
                        }
                    })
                    .collect();

                if !blocks.is_empty() {
                    result.push(RequestMessage {
                        role: "assistant".into(),
                        content: serde_json::Value::Array(blocks),
                    });
                }
            }
            Message::ToolResult(tr) => {
                let content_blocks: Vec<serde_json::Value> = tr
                    .content
                    .iter()
                    .map(|part| match part {
                        ToolResultContentPart::Text(t) => serde_json::json!({
                            "type": "text",
                            "text": t.text,
                        }),
                        ToolResultContentPart::Image(img) => serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": img.mime_type,
                                "data": img.data,
                            }
                        }),
                    })
                    .collect();

                let content = serde_json::json!([{
                    "type": "tool_result",
                    "tool_use_id": tr.tool_call_id.as_str(),
                    "content": content_blocks,
                    "is_error": tr.outcome.is_error(),
                }]);
                result.push(RequestMessage {
                    role: "user".into(),
                    content,
                });
            }
        }
    }

    result
}

/// ensure every tool_use block in an assistant message has a matching tool_result.
/// inserts synthetic error results for orphaned tool calls (from aborts, steering,
/// compaction, etc) so the API doesn't reject the conversation.
fn fix_orphaned_tool_calls(messages: Vec<RequestMessage>) -> Vec<RequestMessage> {
    let mut result: Vec<RequestMessage> = Vec::new();
    let mut pending_tool_ids: Vec<String> = Vec::new();
    let mut seen_result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in messages {
        if msg.role == "assistant" {
            // flush any orphaned tool calls from a previous assistant message
            inject_synthetic_results(&mut result, &pending_tool_ids, &seen_result_ids);
            pending_tool_ids.clear();
            seen_result_ids.clear();

            // collect tool_use IDs from this assistant message
            if let serde_json::Value::Array(ref blocks) = msg.content {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        && let Some(id) = block.get("id").and_then(|i| i.as_str())
                    {
                        pending_tool_ids.push(id.to_string());
                    }
                }
            }
            result.push(msg);
        } else if msg.role == "user" {
            // check if this is a tool_result
            if let serde_json::Value::Array(ref blocks) = msg.content {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        && let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str())
                    {
                        seen_result_ids.insert(id.to_string());
                    }
                }
            }

            // if this is a plain user message (not tool_result), flush orphans first
            let is_tool_result = msg
                .content
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                })
                .unwrap_or(false);

            if !is_tool_result && !pending_tool_ids.is_empty() {
                inject_synthetic_results(&mut result, &pending_tool_ids, &seen_result_ids);
                pending_tool_ids.clear();
                seen_result_ids.clear();
            }

            result.push(msg);
        } else {
            result.push(msg);
        }
    }

    // flush any remaining orphans at the end
    inject_synthetic_results(&mut result, &pending_tool_ids, &seen_result_ids);

    result
}

/// apply cache control to the last cacheable block in a user message.
/// this mirrors anthropic automatic caching semantics for multi-turn chat.
fn apply_cache_control_to_last_user_message(
    messages: &mut [RequestMessage],
    cache_control: Option<CacheControl>,
) {
    let Some(cache) = cache_control else {
        return;
    };

    let Some(last_user) = messages.iter_mut().rfind(|m| m.role == "user") else {
        return;
    };

    match &mut last_user.content {
        serde_json::Value::String(text) => {
            let text = std::mem::take(text);
            last_user.content = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": cache,
            }]);
        }
        serde_json::Value::Array(blocks) => {
            if let Some(last) = blocks.last_mut()
                && let Some(obj) = last.as_object_mut()
                && let Ok(cache_json) = serde_json::to_value(cache)
            {
                obj.insert("cache_control".into(), cache_json);
            }
        }
        _ => {}
    }
}

fn inject_synthetic_results(
    result: &mut Vec<RequestMessage>,
    pending_ids: &[String],
    seen_ids: &std::collections::HashSet<String>,
) {
    for id in pending_ids {
        if !seen_ids.contains(id) {
            result.push(RequestMessage {
                role: "user".into(),
                content: serde_json::json!([{
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": [{"type": "text", "text": "no result provided"}],
                    "is_error": true,
                }]),
            });
        }
    }
}

// -- SSE parsing --

/// raw SSE event from the anthropic stream
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[expect(dead_code)]
enum SseEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageStartData },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockData,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: DeltaData },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDeltaData,
        usage: UsageDeltaData,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error { error: ErrorData },
}

#[derive(Debug, Deserialize)]
struct MessageStartData {
    usage: Option<UsageData>,
}

#[derive(Debug, Deserialize)]
struct UsageData {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlockData {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[expect(clippy::enum_variant_names)]
enum DeltaData {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
}

#[derive(Debug, Deserialize)]
struct MessageDeltaData {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageDeltaData {
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ErrorData {
    #[serde(default)]
    message: String,
}

/// tracks state for a content block being streamed
#[derive(Debug, Clone)]
enum BlockState {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        json_buf: String,
    },
}

fn parse_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
    is_oauth: bool,
    tools: Vec<ToolDefinition>,
) -> EventStream {
    let byte_stream = response.bytes_stream();

    let event_stream = async_stream::stream! {
        let mut output = AssistantMessage {
            content: vec![],
            model: model_id,
            provider: provider_name,
            api,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::now(),
        };

        let mut blocks: Vec<BlockState> = Vec::new();
        let mut buf = String::new();

        // collect the full response body as SSE lines
        use futures::TryStreamExt;
        let mut byte_stream = byte_stream;
        let mut chunk_buf = Vec::new();

        loop {
            match byte_stream.try_next().await {
                Ok(Some(chunk)) => {
                    chunk_buf.extend_from_slice(&chunk);

                    // process complete lines
                    while let Some(newline_pos) = chunk_buf.iter().position(|&b| b == b'\n') {
                        let line = String::from_utf8_lossy(&chunk_buf[..newline_pos]).to_string();
                        chunk_buf.drain(..=newline_pos);

                        let line = line.trim_end_matches('\r');

                        if line.is_empty() {
                            // empty line = end of SSE event, parse the buffered data
                            if !buf.is_empty() {
                                if let Some(data) = buf.strip_prefix("data: ") {
                                    match serde_json::from_str::<SseEvent>(data) {
                                        Ok(event) => {
                                            for stream_event in process_sse_event(event, &mut output, &mut blocks, is_oauth, &tools) {
                                                yield stream_event;
                                            }
                                        }
                                        Err(_) => {
                                            // skip unparseable events (eg [DONE])
                                        }
                                    }
                                }
                                buf.clear();
                            }
                            continue;
                        }

                        if line.starts_with("event:") {
                            // we only care about the data lines
                            continue;
                        }

                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(line);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(error = %e, "SSE stream read error");
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

        // emit done if we haven't errored
        if output.stop_reason != StopReason::Error && output.stop_reason != StopReason::Aborted {
            yield StreamEvent::Done {
                reason: output.stop_reason,
                message: output,
            };
        }
    };

    Box::pin(event_stream)
}

fn process_sse_event(
    event: SseEvent,
    output: &mut AssistantMessage,
    blocks: &mut Vec<BlockState>,
    is_oauth: bool,
    tools: &[ToolDefinition],
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    match event {
        SseEvent::MessageStart { message } => {
            if let Some(usage) = message.usage {
                output.usage.input_tokens = usage.input_tokens;
                output.usage.output_tokens = usage.output_tokens;
                output.usage.cache_read_tokens = usage.cache_read_input_tokens;
                output.usage.cache_write_tokens = usage.cache_creation_input_tokens;
            }
            events.push(StreamEvent::Start {
                partial: output.clone(),
            });
        }
        SseEvent::ContentBlockStart {
            index: _,
            content_block,
        } => {
            let content_index = blocks.len();
            match content_block {
                ContentBlockData::Text { text } => {
                    blocks.push(BlockState::Text { text: text.clone() });
                    output
                        .content
                        .push(AssistantContentPart::Text(TextContent { text }));
                    events.push(StreamEvent::TextStart { content_index });
                }
                ContentBlockData::Thinking { thinking } => {
                    blocks.push(BlockState::Thinking {
                        thinking: thinking.clone(),
                        signature: None,
                    });
                    output.content.push(AssistantContentPart::Thinking(
                        ThinkingContent::Thinking {
                            thinking,
                            signature: None,
                        },
                    ));
                    events.push(StreamEvent::ThinkingStart { content_index });
                }
                ContentBlockData::RedactedThinking { data } => {
                    blocks.push(BlockState::Thinking {
                        thinking: "[reasoning redacted]".into(),
                        signature: Some(data.clone()),
                    });
                    output.content.push(AssistantContentPart::Thinking(
                        ThinkingContent::Redacted { data },
                    ));
                    events.push(StreamEvent::ThinkingStart { content_index });
                }
                ContentBlockData::ToolUse { id, name } => {
                    // map claude code tool names back to our names
                    let resolved_name = if is_oauth {
                        from_claude_code_name(&name, tools)
                    } else {
                        name.clone()
                    };
                    blocks.push(BlockState::ToolCall {
                        id: id.clone(),
                        name: resolved_name.clone(),
                        json_buf: String::new(),
                    });
                    output
                        .content
                        .push(AssistantContentPart::ToolCall(ToolCall {
                            id: ToolCallId::from(id),
                            name: ToolName::from(resolved_name),
                            arguments: serde_json::Value::Object(Default::default()),
                        }));
                    events.push(StreamEvent::ToolCallStart { content_index });
                }
            }
        }
        SseEvent::ContentBlockDelta { index: _, delta } => {
            // find the block by checking from the end (anthropic sends index, but
            // we track by our own vec position which should match)
            let content_index = blocks.len().saturating_sub(1);

            match delta {
                DeltaData::TextDelta { text } => {
                    if let Some(BlockState::Text { text: buf }) = blocks.last_mut() {
                        buf.push_str(&text);
                    }
                    if let Some(AssistantContentPart::Text(tc)) =
                        output.content.get_mut(content_index)
                    {
                        tc.text.push_str(&text);
                    }
                    events.push(StreamEvent::TextDelta {
                        content_index,
                        delta: text,
                    });
                }
                DeltaData::ThinkingDelta { thinking } => {
                    if let Some(BlockState::Thinking { thinking: buf, .. }) = blocks.last_mut() {
                        buf.push_str(&thinking);
                    }
                    if let Some(AssistantContentPart::Thinking(tc)) =
                        output.content.get_mut(content_index)
                        && let Some(buf) = tc.text_mut()
                    {
                        buf.push_str(&thinking);
                    }
                    events.push(StreamEvent::ThinkingDelta {
                        content_index,
                        delta: thinking,
                    });
                }
                DeltaData::InputJsonDelta { partial_json } => {
                    if let Some(BlockState::ToolCall { json_buf, .. }) = blocks.last_mut() {
                        json_buf.push_str(&partial_json);
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_buf)
                            && let Some(AssistantContentPart::ToolCall(tc)) =
                                output.content.get_mut(content_index)
                        {
                            tc.arguments = parsed;
                        }
                    }
                    events.push(StreamEvent::ToolCallDelta {
                        content_index,
                        delta: partial_json,
                    });
                }
                DeltaData::SignatureDelta { signature } => {
                    if let Some(BlockState::Thinking { signature: sig, .. }) = blocks.last_mut() {
                        sig.get_or_insert_with(String::new).push_str(&signature);
                    }
                    if let Some(AssistantContentPart::Thinking(tc)) =
                        output.content.get_mut(content_index)
                        && let Some(sig) = tc.signature_mut()
                    {
                        sig.get_or_insert_with(String::new).push_str(&signature);
                    }
                }
            }
        }
        SseEvent::ContentBlockStop { index: _ } => {
            if let Some(block) = blocks.last() {
                let content_index = blocks.len() - 1;
                match block {
                    BlockState::Text { text } => {
                        events.push(StreamEvent::TextEnd {
                            content_index,
                            text: text.clone(),
                        });
                    }
                    BlockState::Thinking { thinking, .. } => {
                        events.push(StreamEvent::ThinkingEnd {
                            content_index,
                            thinking: thinking.clone(),
                        });
                    }
                    BlockState::ToolCall { id, name, json_buf } => {
                        let arguments = serde_json::from_str(json_buf)
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        // update the output with final parsed args
                        if let Some(AssistantContentPart::ToolCall(tc)) =
                            output.content.get_mut(content_index)
                        {
                            tc.arguments = arguments.clone();
                        }
                        events.push(StreamEvent::ToolCallEnd {
                            content_index,
                            id: id.clone(),
                            name: name.clone(),
                            arguments,
                        });
                    }
                }
            }
        }
        SseEvent::MessageDelta { delta, usage } => {
            output.usage.output_tokens = usage.output_tokens;
            if let Some(reason) = delta.stop_reason {
                output.stop_reason = map_stop_reason(&reason);
            }
        }
        SseEvent::MessageStop => {}
        SseEvent::Ping => {}
        SseEvent::Error { error } => {
            tracing::error!(error = %error.message, "anthropic SSE error event");
            output.stop_reason = StopReason::Error;
            output.error_message = Some(error.message);
        }
    }

    events
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_stop_reasons() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::Stop);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::Length);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("unknown"), StopReason::Error);
    }

    #[test]
    fn thinking_budget_levels() {
        assert_eq!(thinking_budget(ThinkingLevel::Off, 8192), 0);
        assert_eq!(thinking_budget(ThinkingLevel::Minimal, 8192), 1024);
        assert_eq!(thinking_budget(ThinkingLevel::Low, 8192), 2048);
        assert_eq!(thinking_budget(ThinkingLevel::Medium, 8192), 4096);
        assert_eq!(thinking_budget(ThinkingLevel::High, 8192), 8192);
    }

    #[test]
    fn thinking_budget_respects_minimum() {
        // even with low max_tokens, budget should use base of 4096
        assert_eq!(thinking_budget(ThinkingLevel::Low, 1024), 1024);
        assert_eq!(thinking_budget(ThinkingLevel::Medium, 1024), 2048);
    }

    #[test]
    fn convert_simple_user_message() {
        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text("hello".into()),
            timestamp_ms: Timestamp::zero(),
        })];

        let converted = convert_messages(&messages, false, None);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[0].content, serde_json::json!("hello"));
    }

    #[test]
    fn convert_tool_result_message() {
        let messages = vec![Message::ToolResult(ToolResultMessage {
            tool_call_id: "tc_123".into(),
            tool_name: "read".into(),
            content: vec![ToolResultContentPart::Text(TextContent {
                text: "file contents".into(),
            })],
            outcome: ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
        })];

        let converted = convert_messages(&messages, false, None);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");

        let content = &converted[0].content;
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "tc_123");
    }

    #[test]
    fn convert_assistant_with_tool_call() {
        let messages = vec![Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContentPart::Text(TextContent {
                    text: "let me read that".into(),
                }),
                AssistantContentPart::ToolCall(ToolCall {
                    id: "tc_1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
                }),
            ],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::AnthropicMessages,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        })];

        let converted = convert_messages(&messages, false, None);
        // assistant + synthetic tool_result for the orphaned tool_use
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "assistant");

        let blocks = converted[0].content.as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "tc_1");

        // synthetic result
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content[0]["type"], "tool_result");
        assert_eq!(converted[1].content[0]["tool_use_id"], "tc_1");
        assert_eq!(converted[1].content[0]["is_error"], true);
    }

    #[test]
    fn tool_use_with_matching_result_no_synthetic() {
        let messages = vec![
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::ToolCall(ToolCall {
                    id: "tc_1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                })],
                model: "test".into(),
                provider: Provider::Custom("test".into()),
                api: Api::AnthropicMessages,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp_ms: Timestamp::zero(),
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "hi".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
        ];

        let converted = convert_messages(&messages, false, None);
        // assistant + real tool_result, no synthetic
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content[0]["tool_use_id"], "tc_1");
        // the real result, not synthetic error
        assert_eq!(converted[1].content[0]["is_error"], false);
    }

    #[test]
    fn orphaned_tool_use_gets_synthetic_result() {
        // assistant with tool_use followed by a user message (steering) - no tool_result
        let messages = vec![
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::ToolCall(ToolCall {
                    id: "tc_orphan".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "rm -rf /"}),
                })],
                model: "test".into(),
                provider: Provider::Custom("test".into()),
                api: Api::AnthropicMessages,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp_ms: Timestamp::zero(),
            }),
            Message::User(UserMessage {
                content: UserContent::Text("stop! undo that".into()),
                timestamp_ms: Timestamp::zero(),
            }),
        ];

        let converted = convert_messages(&messages, false, None);
        // assistant + synthetic tool_result + user
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content[0]["type"], "tool_result");
        assert_eq!(converted[1].content[0]["tool_use_id"], "tc_orphan");
        assert_eq!(converted[1].content[0]["is_error"], true);
        assert_eq!(converted[2].role, "user");
        assert_eq!(converted[2].content, serde_json::json!("stop! undo that"));
    }

    #[test]
    fn build_body_without_thinking() {
        let model = test_model();
        let options = StreamOptions::default();

        let body = build_request_body(
            &model,
            &Some("you are helpful".into()),
            &[],
            &[],
            &options,
            false,
        );

        assert_eq!(body.model, "claude-sonnet-4-20250514");
        assert!(body.thinking.is_none());
        assert!(body.system.is_some());
    }

    #[test]
    fn build_body_with_thinking() {
        let model = Model {
            reasoning: true,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);

        assert!(body.thinking.is_some());
        // temperature should be None when thinking is enabled
        assert!(body.temperature.is_none());
    }

    #[test]
    fn process_message_start_event() {
        let event = SseEvent::MessageStart {
            message: MessageStartData {
                usage: Some(UsageData {
                    input_tokens: 100,
                    output_tokens: 0,
                    cache_read_input_tokens: 50,
                    cache_creation_input_tokens: 25,
                }),
            },
        };

        let mut output = test_output();
        let mut blocks = Vec::new();
        let events = process_sse_event(event, &mut output, &mut blocks, false, &[]);

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Start { .. }));
        assert_eq!(output.usage.input_tokens, 100);
        assert_eq!(output.usage.cache_read_tokens, 50);
    }

    #[test]
    fn process_text_block_lifecycle() {
        let mut output = test_output();
        let mut blocks = Vec::new();

        // start
        let events = process_sse_event(
            SseEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockData::Text {
                    text: String::new(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );
        assert!(matches!(
            events[0],
            StreamEvent::TextStart { content_index: 0 }
        ));

        // delta
        let events = process_sse_event(
            SseEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaData::TextDelta {
                    text: "hello ".into(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );
        assert!(matches!(
            events[0],
            StreamEvent::TextDelta {
                content_index: 0,
                ..
            }
        ));

        // another delta
        process_sse_event(
            SseEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaData::TextDelta {
                    text: "world".into(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );

        // stop
        let events = process_sse_event(
            SseEvent::ContentBlockStop { index: 0 },
            &mut output,
            &mut blocks,
            false,
            &[],
        );
        match &events[0] {
            StreamEvent::TextEnd { text, .. } => assert_eq!(text, "hello world"),
            other => panic!("expected TextEnd, got {other:?}"),
        }

        // check output was updated
        match &output.content[0] {
            AssistantContentPart::Text(t) => assert_eq!(t.text, "hello world"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn process_tool_call_lifecycle() {
        let mut output = test_output();
        let mut blocks = Vec::new();

        process_sse_event(
            SseEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockData::ToolUse {
                    id: "tc_1".into(),
                    name: "read".into(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );

        process_sse_event(
            SseEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaData::InputJsonDelta {
                    partial_json: r#"{"path":"#.into(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );

        process_sse_event(
            SseEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaData::InputJsonDelta {
                    partial_json: r#""foo.rs"}"#.into(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );

        let events = process_sse_event(
            SseEvent::ContentBlockStop { index: 0 },
            &mut output,
            &mut blocks,
            false,
            &[],
        );

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
    fn claude_code_name_mapping() {
        assert_eq!(to_claude_code_name("read"), "Read");
        assert_eq!(to_claude_code_name("Write"), "Write");
        assert_eq!(to_claude_code_name("bash"), "Bash");
        assert_eq!(to_claude_code_name("webfetch"), "WebFetch");
        assert_eq!(to_claude_code_name("websearch"), "WebSearch");
        // underscored names normalised to match claude code names
        assert_eq!(to_claude_code_name("web_fetch"), "WebFetch");
        assert_eq!(to_claude_code_name("web_search"), "WebSearch");
        // unknown tools passed through as-is
        assert_eq!(to_claude_code_name("custom_tool"), "custom_tool");
    }

    #[test]
    fn claude_code_name_reverse_mapping() {
        let tools = vec![
            ToolDefinition {
                name: "read".into(),
                description: "read files".into(),
                parameters: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_fetch".into(),
                description: "fetch urls".into(),
                parameters: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".into(),
                description: "search web".into(),
                parameters: serde_json::json!({}),
            },
        ];
        assert_eq!(from_claude_code_name("Read", &tools), "read");
        assert_eq!(from_claude_code_name("WebFetch", &tools), "web_fetch");
        assert_eq!(from_claude_code_name("WebSearch", &tools), "web_search");
        assert_eq!(from_claude_code_name("Unknown", &tools), "Unknown");
    }

    #[test]
    fn oauth_system_prompt_has_identity() {
        let model = test_model();
        let options = StreamOptions::default();
        let body = build_request_body(&model, &Some("be helpful".into()), &[], &[], &options, true);
        let system = body.system.unwrap();
        assert_eq!(system.len(), 2);
        assert!(system[0].text.contains("Claude Code"));
        assert_eq!(system[1].text, "be helpful");
    }

    #[test]
    fn anthropic_cache_control_defaults_to_short() {
        let cc = anthropic_cache_control("https://api.anthropic.com", None).unwrap();
        assert_eq!(cc.control_type, "ephemeral");
        assert!(cc.ttl.is_none());
    }

    #[test]
    fn anthropic_cache_control_long_only_on_direct_api() {
        let direct =
            anthropic_cache_control("https://api.anthropic.com", Some(CacheRetention::Long))
                .unwrap();
        assert_eq!(direct.ttl.as_deref(), Some("1h"));

        let proxied =
            anthropic_cache_control("https://openrouter.ai/api/v1", Some(CacheRetention::Long))
                .unwrap();
        assert!(proxied.ttl.is_none());
    }

    #[test]
    fn convert_user_message_adds_cache_control_block() {
        let cache = Some(CacheControl {
            control_type: "ephemeral".into(),
            ttl: None,
        });
        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text("hello".into()),
            timestamp_ms: Timestamp::zero(),
        })];

        let converted = convert_messages(&messages, false, cache);
        let blocks = converted[0].content.as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn non_oauth_system_prompt_no_identity() {
        let model = test_model();
        let options = StreamOptions::default();
        let body = build_request_body(
            &model,
            &Some("be helpful".into()),
            &[],
            &[],
            &options,
            false,
        );
        let system = body.system.unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0].text, "be helpful");
    }

    #[test]
    fn oauth_tool_names_mapped() {
        let model = test_model();
        let options = StreamOptions::default();
        let tools = vec![ToolDefinition {
            name: "read".into(),
            description: "read files".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let body = build_request_body(&model, &None, &[], &tools, &options, true);
        let tools = body.tools.unwrap();
        assert_eq!(tools[0].name, "Read");
    }

    fn test_model() -> Model {
        Model {
            id: "claude-sonnet-4-20250514".into(),
            name: "Claude Sonnet 4".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: DEFAULT_BASE_URL.into(),
            reasoning: false,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_output_tokens: 16384,
        }
    }

    fn test_output() -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::AnthropicMessages,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        }
    }
}
