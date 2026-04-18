//! anthropic messages API provider
//!
//! streams responses via SSE from the anthropic messages endpoint.
//! supports extended thinking, tool use, and image inputs.

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::env::anthropic_api_key;
use crate::registry::{ApiProvider, EventStream, LlmContext, ProviderError, ToolDefinition};
use crate::stream::StreamEvent;
use crate::types::*;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const ANTHROPIC_DIRECT_API: &str = "api.anthropic.com";
/// full `user-agent` value sent on oauth requests. bump the version when
/// matching a newer claude-code release; keeping it close to upstream helps
/// with any rate-limit or fingerprint-based treatment
const CLAUDE_CLI_USER_AGENT: &str = "claude-cli/2.1.111";

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
    let normalised = super::normalize_tool_name(name);
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|t| t.to_lowercase() == normalised)
        .map(|t| t.to_string())
        .unwrap_or_else(|| name.to_string())
}

/// convert claude code tool name back to our tool name using the tool definitions
fn from_claude_code_name(name: &str, tools: &[ToolDefinition]) -> String {
    let normalised = super::normalize_tool_name(name);
    tools
        .iter()
        .find(|t| super::normalize_tool_name(&t.name) == normalised)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| name.to_string())
}

pub struct AnthropicProvider {
    pub client: reqwest::Client,
}

#[async_trait::async_trait]
impl ApiProvider for AnthropicProvider {
    fn api(&self) -> Api {
        Api::AnthropicMessages
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
            .or_else(anthropic_api_key)
            .ok_or_else(|| ProviderError::MissingApiKey(Provider::Anthropic))?;

        let is_oauth = api_key.is_oauth_token();

        let body = build_request_body(
            model,
            &context.system_prompt,
            &context.messages,
            &context.tools,
            options,
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
            let betas = options.anthropic_betas.clone().unwrap_or_default();
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_str(&betas.to_header_value())?,
            );
            headers.insert(
                "user-agent",
                HeaderValue::from_static(CLAUDE_CLI_USER_AGENT),
            );
            headers.insert("x-app", HeaderValue::from_static("cli"));
            // claude-code sends this to bypass browser-env CORS checks on oauth
            headers.insert(
                "anthropic-dangerous-direct-browser-access",
                HeaderValue::from_static("true"),
            );
            // include session id for anthropic-side diagnostic correlation.
            // format isn't validated by the server but we mirror claude code
            if let Some(sid) = &options.session_id
                && let Ok(hv) = HeaderValue::from_str(sid.as_ref())
            {
                headers.insert("x-claude-code-session-id", hv);
            }
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

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;

        let response =
            super::check_response(response, "anthropic", model.id.as_str(), &url).await?;

        let sse_stream = parse_sse_stream(
            response,
            model.id.clone(),
            model.provider.clone(),
            model.api,
            is_oauth,
            context.tools.clone(),
        );

        Ok(sse_stream)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ThinkingConfig {
    Enabled {
        #[serde(rename = "type")]
        config_type: String,
        budget_tokens: u64,
        display: String,
    },
    Adaptive {
        #[serde(rename = "type")]
        config_type: String,
        display: String,
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
    let cache_control =
        anthropic_cache_control(model.base_url.as_str(), options.cache_retention, is_oauth);

    // system blocks get scope:"global" on oauth so the system prompt cache
    // is shared across sessions (mirroring claude code behaviour)
    let system_cache_control = cache_control.clone().map(|mut cc| {
        if is_oauth {
            cc.scope = Some("global".into());
        }
        cc
    });

    // oauth requires claude code identity as first system block
    let system = if is_oauth {
        let mut blocks = vec![SystemBlock {
            block_type: "text".into(),
            text: CLAUDE_CODE_IDENTITY.into(),
            cache_control: system_cache_control.clone(),
        }];
        if let Some(prompt) = system_prompt {
            blocks.push(SystemBlock {
                block_type: "text".into(),
                text: prompt.clone(),
                cache_control: system_cache_control.clone(),
            });
        }
        Some(blocks)
    } else {
        system_prompt.as_ref().map(|prompt| {
            vec![SystemBlock {
                block_type: "text".into(),
                text: prompt.clone(),
                cache_control: system_cache_control.clone(),
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
                    display: "summarized".into(),
                })
            } else {
                let budget = thinking_budget(level, max_tokens.get());
                Some(ThinkingConfig::Enabled {
                    config_type: "enabled".into(),
                    budget_tokens: budget,
                    display: "summarized".into(),
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
    is_oauth: bool,
) -> Option<CacheControl> {
    let retention = retention.unwrap_or(CacheRetention::Short);
    if retention == CacheRetention::None {
        return None;
    }

    let is_direct = base_url.contains(ANTHROPIC_DIRECT_API);
    // on claude.ai (oauth), 1h cache writes are included in the subscription
    // so always use 1h. on API keys, only use 1h when explicitly requested
    let ttl = if is_direct && (is_oauth || retention == CacheRetention::Long) {
        Some("1h".to_string())
    } else {
        None
    };

    Some(CacheControl {
        control_type: "ephemeral".into(),
        ttl,
        scope: None,
    })
}

/// Claude 4.6+ Opus models and Sonnet 4.6 use adaptive thinking. Older Claude
/// 4 models still use enabled+budget thinking. Keep this narrow until
/// reasoning capabilities move into model metadata instead of living here.
fn supports_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("opus-4-7")
        || model_id.contains("opus-4.7")
        || model_id.contains("opus-4-6")
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
        (id, ThinkingLevel::Xhigh) if id.contains("opus-4-7") || id.contains("opus-4.7") => {
            Some("xhigh")
        }
        // Model-specific effort support should eventually live in model metadata.
        // Claude Opus 4.6 lacks `xhigh`, so map the top visible level to `max`.
        (id, ThinkingLevel::Xhigh) if id.contains("opus-4-6") || id.contains("opus-4.6") => {
            Some("max")
        }
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
                // always emit as a content block array, never a bare JSON string.
                // apply_cache_control_to_last_user_message inserts a cache_control
                // field into the last block of the target message. if we emitted
                // text as a string here, that function would have to wrap it in an
                // array, then when cache_control moves to a newer message on the
                // next request the old message reverts to a bare string. that
                // format change alters the serialised prefix bytes and busts the
                // anthropic prompt cache (which does byte-exact prefix matching)
                let content = match &user.content {
                    UserContent::Text(text) => serde_json::json!([{
                        "type": "text",
                        "text": text,
                    }]),
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
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": img.mime_type,
                                        "data": img.data,
                                    }
                                })),
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

/// ensure tool_use/tool_result pairing is consistent.
///
/// handles both directions:
/// - tool_use without tool_result: injects synthetic error results
/// - tool_result without tool_use: strips the orphaned result
///
/// orphaned tool_results can appear after compaction removes an assistant
/// message while its tool_results survive at the boundary, or from other
/// message reordering during context transforms.
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
            let is_tool_result = msg
                .content
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                })
                .unwrap_or(false);

            if is_tool_result {
                // check each tool_result references a known tool_use
                if let Some(id) = msg
                    .content
                    .as_array()
                    .and_then(|blocks| blocks.first())
                    .and_then(|b| b.get("tool_use_id"))
                    .and_then(|i| i.as_str())
                {
                    if pending_tool_ids.contains(&id.to_string()) {
                        seen_result_ids.insert(id.to_string());
                        result.push(msg);
                    } else {
                        tracing::warn!(
                            tool_use_id = id,
                            "stripping orphaned tool_result (no matching tool_use)"
                        );
                    }
                } else {
                    result.push(msg);
                }
            } else {
                // plain user message: flush any pending orphans first
                if !pending_tool_ids.is_empty() {
                    inject_synthetic_results(&mut result, &pending_tool_ids, &seen_result_ids);
                    pending_tool_ids.clear();
                    seen_result_ids.clear();
                }
                result.push(msg);
            }
        } else {
            result.push(msg);
        }
    }

    // flush any remaining orphans at the end
    inject_synthetic_results(&mut result, &pending_tool_ids, &seen_result_ids);

    result
}

/// apply cache control to the last user text message (not tool_results).
/// tool_result messages are also role="user" in anthropic format, but placing
/// cache_control on them causes prefix invalidation: each new tool_result moves
/// the marker, changing the serialised content of the previous holder. by targeting
/// only actual user text messages, the cache_control position stays stable within
/// an agent turn's tool-call loop.
///
/// user text content is always emitted as a content block array by
/// convert_messages_raw, so this only needs the Array arm. the String arm
/// is kept as a defensive fallback but should never trigger in practice
fn apply_cache_control_to_last_user_message(
    messages: &mut [RequestMessage],
    cache_control: Option<CacheControl>,
) {
    let Some(cache) = cache_control else {
        return;
    };

    let is_tool_result_msg = |msg: &RequestMessage| {
        msg.content.as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        })
    };

    let Some(last_user) = messages
        .iter_mut()
        .rfind(|m| m.role == "user" && !is_tool_result_msg(m))
    else {
        return;
    };

    match &mut last_user.content {
        serde_json::Value::Array(blocks) => {
            if let Some(last) = blocks.last_mut()
                && let Some(obj) = last.as_object_mut()
                && let Ok(cache_json) = serde_json::to_value(cache)
            {
                obj.insert("cache_control".into(), cache_json);
            }
        }
        // defensive: convert_messages_raw always emits arrays for user text,
        // but if something upstream changes, wrap rather than silently skip
        serde_json::Value::String(text) => {
            let text = std::mem::take(text);
            last_user.content = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": cache,
            }]);
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

struct AnthropicProcessor {
    blocks: Vec<StreamBlock>,
    is_oauth: bool,
    tools: Vec<ToolDefinition>,
}

impl super::sse::SseProcessor for AnthropicProcessor {
    fn process(
        &mut self,
        raw: &super::sse::SseRawEvent,
        output: &mut AssistantMessage,
    ) -> super::sse::ProcessResult {
        match serde_json::from_str::<SseEvent>(&raw.data) {
            Ok(event) => {
                let events =
                    process_sse_event(event, output, &mut self.blocks, self.is_oauth, &self.tools);
                super::sse::ProcessResult::Events(events)
            }
            Err(_) => {
                // [DONE] and empty payloads are expected
                super::sse::ProcessResult::Skip
            }
        }
    }

    fn finish(&mut self, _output: &mut AssistantMessage) {
        // anthropic sends explicit message_stop events, no cleanup needed
    }

    fn emit_start(&self) -> bool {
        // anthropic emits Start from within process() on message_start
        false
    }

    fn label(&self) -> &'static str {
        "anthropic"
    }
}

fn parse_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
    is_oauth: bool,
    tools: Vec<ToolDefinition>,
) -> EventStream {
    let processor = AnthropicProcessor {
        blocks: Vec::new(),
        is_oauth,
        tools,
    };
    super::sse::run_sse_stream(response, model_id, provider_name, api, processor)
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
    fn oauth_beta_default_includes_expected_flags() {
        let header = AnthropicBetas::default().to_header_value();
        // persistent flags always present
        assert!(header.contains("claude-code-20250219"), "header={header}");
        assert!(header.contains("oauth-2025-04-20"), "header={header}");
        assert!(header.contains("interleaved-thinking-2025-05-14"));
        assert!(header.contains("prompt-caching-scope-2026-01-05"));
        // opt-in defaults per spec
        assert!(header.contains("context-1m-2025-08-07"));
        assert!(header.contains("effort-2025-11-24"));
        assert!(header.contains("context-management-2025-06-27"));
        // off by default per spec
        assert!(!header.contains("redact-thinking-2026-02-12"));
        assert!(!header.contains("advisor-tool-2026-03-01"));
        assert!(!header.contains("advanced-tool-use-2025-11-20"));
    }

    #[test]
    fn oauth_beta_toggles_respect_config() {
        let betas = AnthropicBetas {
            context_1m: false,
            effort: false,
            context_management: false,
            redact_thinking: true,
            advisor: true,
            advanced_tool_use: true,
        };
        let header = betas.to_header_value();
        assert!(!header.contains("context-1m-2025-08-07"));
        assert!(!header.contains("effort-2025-11-24"));
        assert!(!header.contains("context-management-2025-06-27"));
        assert!(header.contains("redact-thinking-2026-02-12"));
        assert!(header.contains("advisor-tool-2026-03-01"));
        assert!(header.contains("advanced-tool-use-2025-11-20"));
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
        assert_eq!(
            converted[0].content,
            serde_json::json!([{"type": "text", "text": "hello"}])
        );
    }

    #[test]
    fn convert_image_only_message_omits_empty_text() {
        // when user pastes an image with no text, the Parts vec contains
        // an empty Text("") + Image. the empty text block must be filtered
        // out or anthropic rejects with "text content blocks must be non-empty"
        let messages = vec![Message::User(UserMessage {
            content: UserContent::Parts(vec![
                UserContentPart::Text(TextContent { text: "".into() }),
                UserContentPart::Image(ImageContent {
                    data: "iVBOR...".into(),
                    mime_type: ImageMimeType::Png,
                }),
            ]),
            timestamp_ms: Timestamp::zero(),
        })];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        assert_eq!(converted.len(), 1);
        let blocks = converted[0].content.as_array().expect("should be array");
        // only the image block, no empty text
        assert_eq!(blocks.len(), 1, "empty text block should be filtered out");
        assert_eq!(blocks[0]["type"], "image");
    }

    #[test]
    fn convert_tool_result_message() {
        let messages = vec![
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::ToolCall(ToolCall {
                    id: "tc_123".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
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
                tool_call_id: "tc_123".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "file contents".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");

        let content = &converted[1].content;
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
        assert_eq!(
            converted[2].content,
            serde_json::json!([{"type": "text", "text": "stop! undo that"}])
        );
    }

    #[test]
    fn orphaned_tool_result_stripped() {
        // tool_result referencing a tool_use that was compacted away.
        // the tool_result should be stripped to prevent the API from
        // rejecting the request with "unexpected tool_use_id"
        let messages = vec![
            // after compaction: summary user message
            Message::User(UserMessage {
                content: UserContent::Text("compacted summary".into()),
                timestamp_ms: Timestamp::zero(),
            }),
            // kept assistant with tool_use A
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::ToolCall(ToolCall {
                    id: "tc_A".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
                })],
                model: "test".into(),
                provider: Provider::Custom("test".into()),
                api: Api::AnthropicMessages,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp_ms: Timestamp::zero(),
            }),
            // orphaned tool_result: references tc_GONE which was compacted away
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_GONE".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "stale output".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
            // valid tool_result for tc_A
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_A".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "file contents".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);

        // the orphaned tool_result (tc_GONE) should be stripped entirely.
        // result: user(summary), assistant(tool_use A), user(tool_result A)
        assert_eq!(converted.len(), 3, "orphaned tool_result should be dropped");
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
        assert_eq!(converted[2].content[0]["tool_use_id"], "tc_A");
    }

    #[test]
    fn orphaned_tool_result_all_blocks_stripped() {
        // when ALL tool_results after an assistant are orphaned, the entire
        // message group should be stripped (not just individual blocks)
        let messages = vec![
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::Text(TextContent {
                    text: "done".into(),
                })],
                model: "test".into(),
                provider: Provider::Custom("test".into()),
                api: Api::AnthropicMessages,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp_ms: Timestamp::zero(),
            }),
            // orphaned: the preceding assistant has no tool_uses
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_stale".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "stale".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
            }),
            Message::User(UserMessage {
                content: UserContent::Text("continue".into()),
                timestamp_ms: Timestamp::zero(),
            }),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);

        // the orphaned tool_result should be dropped.
        // result: assistant(text), user("continue")
        assert_eq!(converted.len(), 2, "orphaned tool_result should be dropped");
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(
            converted[1].content,
            serde_json::json!([{"type": "text", "text": "continue"}])
        );
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
    fn opus_4_7_uses_adaptive_thinking_with_xhigh_effort() {
        let model = Model {
            id: "claude-opus-4-7".into(),
            name: "Claude Opus 4.7".into(),
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
            Some(ThinkingConfig::Adaptive {
                ref display,
                ..
            }) if display == "summarized"
        ));
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("xhigh")
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
        let cc = anthropic_cache_control("https://api.anthropic.com", None, false).unwrap();
        assert_eq!(cc.control_type, "ephemeral");
        assert!(cc.ttl.is_none());
    }

    #[test]
    fn anthropic_cache_control_long_only_on_direct_api() {
        let direct = anthropic_cache_control(
            "https://api.anthropic.com",
            Some(CacheRetention::Long),
            false,
        )
        .unwrap();
        assert_eq!(direct.ttl.as_deref(), Some("1h"));

        let proxied = anthropic_cache_control(
            "https://openrouter.ai/api/v1",
            Some(CacheRetention::Long),
            false,
        )
        .unwrap();
        assert!(proxied.ttl.is_none());
    }

    #[test]
    fn anthropic_cache_control_oauth_always_1h() {
        // on claude.ai (oauth), 1h cache writes are included in the
        // subscription so we should always use ttl=1h
        let default = anthropic_cache_control("https://api.anthropic.com", None, true).unwrap();
        assert_eq!(default.ttl.as_deref(), Some("1h"));

        let short = anthropic_cache_control(
            "https://api.anthropic.com",
            Some(CacheRetention::Short),
            true,
        )
        .unwrap();
        assert_eq!(short.ttl.as_deref(), Some("1h"));
    }

    #[test]
    fn oauth_system_blocks_get_global_scope() {
        // mirroring claude code: system prompt cache_control gets
        // scope:"global" so it's shared across sessions
        let options = StreamOptions {
            cache_retention: None,
            ..Default::default()
        };
        let system = Some("you are helpful".to_string());
        let body = build_request_body(&test_model(), &system, &[], &[], &options, true);
        let system_blocks = body.system.unwrap();
        // oauth adds identity block + system prompt = 2 blocks
        assert_eq!(system_blocks.len(), 2);
        let cc = system_blocks[1].cache_control.as_ref().unwrap();
        assert_eq!(cc.scope.as_deref(), Some("global"));
        assert_eq!(cc.ttl.as_deref(), Some("1h"));
    }

    #[test]
    fn non_oauth_system_blocks_no_scope() {
        let options = StreamOptions {
            cache_retention: None,
            ..Default::default()
        };
        let system = Some("you are helpful".to_string());
        let body = build_request_body(&test_model(), &system, &[], &[], &options, false);
        let system_blocks = body.system.unwrap();
        assert_eq!(system_blocks.len(), 1);
        let cc = system_blocks[0].cache_control.as_ref().unwrap();
        assert!(cc.scope.is_none());
        assert!(cc.ttl.is_none());
    }

    #[test]
    fn convert_user_message_adds_cache_control_block() {
        let cache = Some(CacheControl {
            control_type: "ephemeral".into(),
            ttl: None,
            scope: None,
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

    /// the prefix sent to anthropic must be byte-stable across requests.
    /// when cache_control moves from an old user message to a new one,
    /// the old message's content format must not change. if UserContent::Text
    /// is emitted as a bare JSON string and then wrapped into an array when
    /// cache_control is applied, removing cache_control later (when the
    /// marker moves forward) reverts the format from array to string,
    /// changing the serialised prefix bytes and busting the cache.
    ///
    /// simulates two consecutive requests where cache_control moves from
    /// M_old to M_new, then asserts M_old's content structure is identical.
    #[test]
    fn user_text_content_format_stable_across_cache_control_moves() {
        let cache = Some(CacheControl {
            control_type: "ephemeral".into(),
            ttl: None,
            scope: None,
        });

        // request 1: only M_old, it gets cache_control
        let messages_r1 = vec![Message::User(UserMessage {
            content: UserContent::Text("hello".into()),
            timestamp_ms: Timestamp::zero(),
        })];
        let converted_r1 = convert_messages(
            &messages_r1,
            false,
            cache.clone(),
            ToolResultTrimming::Preserve,
        );

        // request 2: M_old + assistant + M_new, cache_control moves to M_new
        let messages_r2 = vec![
            Message::User(UserMessage {
                content: UserContent::Text("hello".into()),
                timestamp_ms: Timestamp::zero(),
            }),
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::Text(TextContent {
                    text: "hi there".into(),
                })],
                ..test_output()
            }),
            Message::User(UserMessage {
                content: UserContent::Text("goodbye".into()),
                timestamp_ms: Timestamp::zero(),
            }),
        ];
        let converted_r2 =
            convert_messages(&messages_r2, false, cache, ToolResultTrimming::Preserve);

        // strip cache_control from r1's M_old to compare content structure only
        let mut r1_content = converted_r1[0].content.clone();
        if let serde_json::Value::Array(blocks) = &mut r1_content {
            for block in blocks.iter_mut() {
                if let Some(obj) = block.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
        let r2_content = &converted_r2[0].content;

        assert_eq!(
            &r1_content, r2_content,
            "M_old content structure must be identical whether or not it has \
             cache_control. format change from array to string busts the \
             anthropic prefix cache (byte-exact prefix matching).\n\
             with cache_control (stripped): {r1_content}\n\
             without cache_control: {r2_content}"
        );
    }

    #[test]
    fn cache_control_on_user_text_not_tool_result() {
        // when tool_results follow a user message, cache_control must stay on the
        // user text message, not jump to the tool_result. otherwise the user message
        // content changes between requests (loses cache_control), invalidating the
        // entire cached prefix
        let cache = Some(CacheControl {
            control_type: "ephemeral".into(),
            ttl: None,
            scope: None,
        });
        let messages = vec![
            Message::User(UserMessage {
                content: UserContent::Text("do the thing".into()),
                timestamp_ms: Timestamp::zero(),
            }),
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

        let converted = convert_messages(&messages, false, cache, ToolResultTrimming::Preserve);

        // user text message (index 0) should have cache_control.
        // content is always an array of blocks; cache_control is added
        // as a field on the last block
        let has_cc_on_user = match &converted[0].content {
            serde_json::Value::Array(blocks) => blocks
                .first()
                .is_some_and(|b| b.get("cache_control").is_some()),
            _ => false,
        };
        assert!(
            has_cc_on_user,
            "cache_control should be on the user text message, not the tool_result"
        );

        // tool_result (index 2) should NOT have cache_control
        let has_cc_on_tr = match &converted[2].content {
            serde_json::Value::Array(blocks) => {
                blocks.iter().any(|b| b.get("cache_control").is_some())
            }
            _ => false,
        };
        assert!(!has_cc_on_tr, "tool_result should not have cache_control");
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
