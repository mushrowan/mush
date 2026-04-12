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

pub struct AnthropicProvider {
    pub client: reqwest::Client,
}

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
        let client = self.client.clone();

        Box::pin(async move {
            let api_key = options
                .api_key
                .clone()
                .or_else(anthropic_api_key)
                .ok_or_else(|| ProviderError::MissingApiKey(Provider::Anthropic))?;

            let is_oauth = api_key.is_oauth_token();

            let body = build_request_body(
                &model,
                &system_prompt,
                &context_messages,
                &tools,
                &options,
                is_oauth,
            );

            tracing::trace!(
                body = %serde_json::to_string_pretty(&body).unwrap_or_default(),
                "anthropic request body"
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

            let response =
                super::check_response(response, "anthropic", model.id.as_str(), &url).await?;

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
    output_config: Option<OutputConfig>,
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

#[derive(Serialize)]
struct OutputConfig {
    effort: String,
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

    // when caching is active, preserve tool results to avoid busting the prefix cache.
    // trimming old tool results shifts content in the message array, invalidating
    // the cached prefix on every new user message
    let trimming = if cache_control.is_some() {
        ToolResultTrimming::Preserve
    } else {
        ToolResultTrimming::SlidingWindow
    };
    let converted_messages = convert_messages(messages, is_oauth, cache_control.clone(), trimming);

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
                let budget = thinking_budget(level, max_tokens.get());
                Some(ThinkingConfig::Enabled {
                    config_type: "enabled".into(),
                    budget_tokens: budget,
                })
            }
        }
        _ => None,
    };
    let output_config = options
        .thinking
        .and_then(|level| anthropic_effort(&model.id, level))
        .map(|effort| OutputConfig {
            effort: effort.into(),
        });

    // temperature is incompatible with thinking
    let temperature = if thinking.is_none() {
        options.temperature.map(|t| t.value())
    } else {
        None
    };

    // when thinking is enabled with budget, max_tokens must include the budget
    let effective_max_tokens =
        if let Some(ThinkingConfig::Enabled { budget_tokens, .. }) = &thinking {
            max_tokens.get().max(*budget_tokens + 1024)
        } else {
            max_tokens.get()
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
        output_config,
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

/// Claude 4.6 models use adaptive thinking; older Claude 4 models still use
/// enabled+budget thinking. Keep this narrow until reasoning capabilities move
/// into model metadata instead of living in provider code.
fn supports_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("opus-4-6")
        || model_id.contains("opus-4.6")
        || model_id.contains("sonnet-4-6")
        || model_id.contains("sonnet-4.6")
}

fn anthropic_effort(model_id: &str, level: ThinkingLevel) -> Option<&'static str> {
    match (model_id, level) {
        (_, ThinkingLevel::Off) => None,
        (_, ThinkingLevel::Minimal | ThinkingLevel::Low) => Some("low"),
        (_, ThinkingLevel::Medium) => Some("medium"),
        (_, ThinkingLevel::High) => Some("high"),
        // Model-specific effort support should eventually live in model metadata.
        // For now, only the shipped Claude Opus 4.6 id gets Anthropic's `max`.
        ("claude-opus-4-6", ThinkingLevel::Xhigh) => Some("max"),
        (_, ThinkingLevel::Xhigh) => Some("high"),
    }
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
    trimming: ToolResultTrimming,
) -> Vec<RequestMessage> {
    let converted = convert_messages_raw(messages, is_oauth, trimming);
    let mut fixed = fix_orphaned_tool_calls(converted);
    apply_cache_control_to_last_user_message(&mut fixed, cache_control);
    fixed
}

use super::{maybe_trim_tool_output, recent_boundary};

/// raw conversion without orphan fixing
fn convert_messages_raw(
    messages: &[Message],
    is_oauth: bool,
    trimming: ToolResultTrimming,
) -> Vec<RequestMessage> {
    let mut result = Vec::new();
    let boundary = recent_boundary(messages);

    for (msg_idx, msg) in messages.iter().enumerate() {
        let is_old_turn = msg_idx < boundary;
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
                        ToolResultContentPart::Text(t) => {
                            let text = maybe_trim_tool_output(&t.text, is_old_turn, trimming);
                            serde_json::json!({
                                "type": "text",
                                "text": text,
                            })
                        }
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

use super::StreamBlock;

fn parse_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
    is_oauth: bool,
    tools: Vec<ToolDefinition>,
) -> EventStream {
    let event_stream = async_stream::stream! {
        let mut output = AssistantMessage {
            content: vec![],
            model: model_id.clone(),
            provider: provider_name.clone(),
            api,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::now(),
        };

        let mut blocks: Vec<StreamBlock> = Vec::new();
        let mut parser = super::sse::SseParser::new();
        let mut raw_capture: Vec<u8> = Vec::new();
        const MAX_CAPTURE_BYTES: usize = 128 * 1024;

        use futures::TryStreamExt;
        let mut byte_stream = response.bytes_stream();

        loop {
            match byte_stream.try_next().await {
                Ok(Some(chunk)) => {
                    if raw_capture.len() < MAX_CAPTURE_BYTES {
                        let remain = MAX_CAPTURE_BYTES - raw_capture.len();
                        let take = remain.min(chunk.len());
                        raw_capture.extend_from_slice(&chunk[..take]);
                    }
                    let chunk_len = chunk.len();
                    let chunk_preview = super::sse::preview_bytes(&chunk, 240);
                    tracing::trace!(
                        model = %model_id,
                        provider = %provider_name,
                        api = ?api,
                        chunk_len,
                        chunk_preview = %chunk_preview,
                        "anthropic raw stream chunk"
                    );
                    for raw in parser.push(&chunk) {
                        tracing::trace!(
                            model = %model_id,
                            provider = %provider_name,
                            api = ?api,
                            event_name = raw.event.as_deref().unwrap_or("message"),
                            data_preview = %super::sse::preview_text(raw.data.trim(), 240),
                            "anthropic sse event"
                        );
                        match serde_json::from_str::<SseEvent>(&raw.data) {
                            Ok(event) => {
                                for stream_event in process_sse_event(event, &mut output, &mut blocks, is_oauth, &tools) {
                                    yield stream_event;
                                }
                            }
                            Err(e) => {
                                // [DONE] and empty payloads are expected
                                let data = raw.data.trim();
                                if data != "[DONE]" && !data.is_empty() {
                                    tracing::warn!(
                                        model = %model_id,
                                        provider = %provider_name,
                                        api = ?api,
                                        error = %e,
                                        data_preview = %super::sse::preview_text(data, 240),
                                        "anthropic non-parseable sse payload"
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let capture_path = super::openai_responses::write_decode_snapshot(
                        &model_id.to_string(),
                        &provider_name.to_string(),
                        &raw_capture,
                    );
                    tracing::error!(
                        model = %model_id,
                        provider = %provider_name,
                        api = ?api,
                        error = %e,
                        captured_bytes = raw_capture.len(),
                        capture_path = capture_path.as_deref().unwrap_or("<none>"),
                        capture_preview = %super::sse::preview_bytes(&raw_capture, 400),
                        "anthropic body stream decode error"
                    );
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
    blocks: &mut Vec<StreamBlock>,
    is_oauth: bool,
    tools: &[ToolDefinition],
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    match event {
        SseEvent::MessageStart { message } => {
            if let Some(usage) = message.usage {
                output.usage.input_tokens = TokenCount::new(usage.input_tokens);
                output.usage.output_tokens = TokenCount::new(usage.output_tokens);
                output.usage.cache_read_tokens = TokenCount::new(usage.cache_read_input_tokens);
                output.usage.cache_write_tokens =
                    TokenCount::new(usage.cache_creation_input_tokens);
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
                    blocks.push(StreamBlock::Text { text: text.clone() });
                    output
                        .content
                        .push(AssistantContentPart::Text(TextContent { text }));
                    events.push(StreamEvent::TextStart { content_index });
                }
                ContentBlockData::Thinking { thinking } => {
                    blocks.push(StreamBlock::Thinking {
                        text: thinking.clone(),
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
                    blocks.push(StreamBlock::Thinking {
                        text: "[reasoning redacted]".into(),
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
                    blocks.push(StreamBlock::ToolCall {
                        id: id.clone(),
                        name: resolved_name.clone(),
                        args_buf: String::new(),
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
                    if let Some(StreamBlock::Text { text: buf }) = blocks.last_mut() {
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
                    if let Some(StreamBlock::Thinking { text: buf, .. }) = blocks.last_mut() {
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
                    if let Some(StreamBlock::ToolCall { args_buf, .. }) = blocks.last_mut() {
                        args_buf.push_str(&partial_json);
                    }
                    events.push(StreamEvent::ToolCallDelta {
                        content_index,
                        delta: partial_json,
                    });
                }
                DeltaData::SignatureDelta { signature } => {
                    if let Some(StreamBlock::Thinking { signature: sig, .. }) = blocks.last_mut() {
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
                    StreamBlock::Text { text } => {
                        events.push(StreamEvent::TextEnd {
                            content_index,
                            text: text.clone(),
                        });
                    }
                    StreamBlock::Thinking { text: thinking, .. } => {
                        events.push(StreamEvent::ThinkingEnd {
                            content_index,
                            thinking: thinking.clone(),
                        });
                    }
                    StreamBlock::ToolCall { id, name, args_buf } => {
                        let arguments = serde_json::from_str(args_buf)
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
            output.usage.output_tokens = TokenCount::new(usage.output_tokens);
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

#[doc(hidden)]
pub fn benchmark_tool_call_deltas(chunk_count: usize, arg_bytes: usize) -> usize {
    let (_, fragments) = super::bench_support::tool_call_json_fragments(chunk_count, arg_bytes);
    let mut output = AssistantMessage {
        content: vec![],
        model: "bench".into(),
        provider: Provider::Custom("bench".into()),
        api: Api::AnthropicMessages,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp_ms: Timestamp::zero(),
    };
    let mut blocks = Vec::new();

    let _ = process_sse_event(
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

    for fragment in fragments {
        let _ = process_sse_event(
            SseEvent::ContentBlockDelta {
                index: 0,
                delta: DeltaData::InputJsonDelta {
                    partial_json: fragment,
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );
    }

    let _ = process_sse_event(
        SseEvent::ContentBlockStop { index: 0 },
        &mut output,
        &mut blocks,
        false,
        &[],
    );

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

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
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

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
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

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
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

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
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

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
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
    fn opus_4_6_uses_adaptive_thinking_with_effort() {
        let model = Model {
            id: "claude-opus-4-6".into(),
            name: "Claude Opus 4.6".into(),
            reasoning: true,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);

        assert!(matches!(
            body.thinking,
            Some(ThinkingConfig::Adaptive { .. })
        ));
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("max")
        );
    }

    #[test]
    fn legacy_claude_keeps_budget_thinking_with_effort_output_config() {
        let model = Model {
            reasoning: true,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);

        assert!(matches!(
            body.thinking,
            Some(ThinkingConfig::Enabled { .. })
        ));
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("high")
        );
    }

    #[test]
    fn opus_4_5_does_not_send_max_effort() {
        let model = Model {
            id: "claude-opus-4-5".into(),
            name: "Claude Opus 4.5".into(),
            reasoning: true,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);

        assert!(matches!(
            body.thinking,
            Some(ThinkingConfig::Enabled { .. })
        ));
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("high")
        );
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
        assert_eq!(output.usage.input_tokens, TokenCount::new(100));
        assert_eq!(output.usage.cache_read_tokens, TokenCount::new(50));
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

        match &output.content[0] {
            AssistantContentPart::ToolCall(tc) => {
                assert_eq!(tc.arguments, serde_json::json!({}));
            }
            other => panic!("expected tool call, got {other:?}"),
        }

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

        let converted =
            convert_messages(&messages, false, cache, ToolResultTrimming::SlidingWindow);
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

    #[test]
    fn benchmark_tool_call_deltas_returns_payload_size() {
        assert_eq!(benchmark_tool_call_deltas(8, 1024), 1024);
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
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(16384),
        }
    }

    /// build a conversation with enough turns for the sliding window boundary
    /// to kick in, with a large tool result in the oldest turn
    fn messages_with_old_large_tool_result() -> Vec<Message> {
        let large_output = "x".repeat(3000);
        vec![
            // turn 1: user + assistant + tool_call + tool_result (old)
            Message::User(UserMessage {
                content: UserContent::Text("read foo.rs".into()),
                timestamp_ms: Timestamp::zero(),
            }),
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::ToolCall(ToolCall {
                    id: "tc_old".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
                })],
                model: "test".into(),
                provider: Provider::Anthropic,
                api: Api::AnthropicMessages,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp_ms: Timestamp::zero(),
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_old".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: large_output.clone(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
            // turns 2-4: three user messages to push turn 1 past the boundary
            Message::User(UserMessage {
                content: UserContent::Text("thanks".into()),
                timestamp_ms: Timestamp::zero(),
            }),
            Message::User(UserMessage {
                content: UserContent::Text("now do something else".into()),
                timestamp_ms: Timestamp::zero(),
            }),
            Message::User(UserMessage {
                content: UserContent::Text("one more thing".into()),
                timestamp_ms: Timestamp::zero(),
            }),
        ]
    }

    #[test]
    fn sliding_window_trims_old_tool_results() {
        let messages = messages_with_old_large_tool_result();
        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);

        // find the tool result for tc_old
        let tool_result = converted
            .iter()
            .find(|m| {
                m.role == "user"
                    && m.content.is_array()
                    && m.content[0].get("tool_use_id").map(|v| v.as_str()) == Some(Some("tc_old"))
            })
            .expect("should have tool result");

        let text = tool_result.content[0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("trimmed"),
            "old tool result should be trimmed"
        );
        assert!(text.len() < 3000, "trimmed result should be shorter");
    }

    #[test]
    fn preserve_keeps_old_tool_results_intact() {
        let messages = messages_with_old_large_tool_result();
        let converted = convert_messages(&messages, false, None, ToolResultTrimming::Preserve);

        // find the tool result for tc_old
        let tool_result = converted
            .iter()
            .find(|m| {
                m.role == "user"
                    && m.content.is_array()
                    && m.content[0].get("tool_use_id").map(|v| v.as_str()) == Some(Some("tc_old"))
            })
            .expect("should have tool result");

        let text = tool_result.content[0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            !text.contains("trimmed"),
            "preserved tool result should not be trimmed"
        );
        assert_eq!(text.len(), 3000, "preserved result should be full size");
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
