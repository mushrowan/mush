//! openai responses API provider
//!
//! supports both direct openai `/responses` and codex subscription
//! (`chatgpt.com/backend-api/codex/responses`) style endpoints.

use std::collections::HashMap;

use base64::Engine;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde::Serialize;

use crate::env::env_api_key;
use crate::registry::{
    ApiProvider, EventStream, LlmContext, ProviderError, StreamResult, ToolDefinition,
};
use crate::stream::StreamEvent;
use crate::types::*;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const OPENAI_AUTH_CLAIM_PATH: &str = "https://api.openai.com/auth";

pub struct OpenaiResponsesProvider;

impl ApiProvider for OpenaiResponsesProvider {
    fn api(&self) -> Api {
        Api::OpenaiResponses
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
                .ok_or_else(|| ProviderError::MissingApiKey(model.provider.to_string()))?;

            let is_codex = is_codex_provider(&model);
            let body = build_request_body(
                &model,
                &system_prompt,
                &context_messages,
                &tools,
                &options,
                is_codex,
            );

            let url = resolve_url(&model, is_codex);
            let headers = build_headers(&api_key, &options, is_codex)?;

            let client = reqwest::Client::new();
            let response = client
                .post(&url)
                .headers(headers)
                .json(&body)
                .send()
                .await?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(ProviderError::Other(format!(
                    "openai responses API returned {status}: {text}"
                )));
            }

            let model_id = model.id.clone();
            let provider = model.provider.clone();
            let api = model.api;

            Ok(parse_sse_stream(response, model_id, provider, api))
        })
    }
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    input: Vec<serde_json::Value>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<RequestTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<RequestReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
}

#[derive(Serialize)]
struct RequestTool {
    #[serde(rename = "type")]
    tool_type: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
    strict: bool,
}

#[derive(Serialize)]
struct RequestReasoning {
    effort: String,
    summary: String,
}

fn build_request_body(
    model: &Model,
    system_prompt: &Option<String>,
    messages: &[Message],
    tools: &[ToolDefinition],
    options: &StreamOptions,
    is_codex: bool,
) -> RequestBody {
    let input = convert_input_messages(model, system_prompt, messages);

    let converted_tools = if tools.is_empty() {
        None
    } else {
        Some(
            tools
                .iter()
                .map(|t| RequestTool {
                    tool_type: "function".into(),
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                    strict: false,
                })
                .collect(),
        )
    };

    let reasoning_effort = match options.thinking {
        Some(level) if level != ThinkingLevel::Off && model.reasoning => Some(match level {
            ThinkingLevel::Off => unreachable!(),
            ThinkingLevel::Minimal => "minimal".into(),
            ThinkingLevel::Low => "low".into(),
            ThinkingLevel::Medium => "medium".into(),
            ThinkingLevel::High => "high".into(),
            ThinkingLevel::Xhigh => "high".into(),
        }),
        _ => None,
    };

    let reasoning = reasoning_effort.map(|effort| RequestReasoning {
        effort,
        summary: "auto".into(),
    });

    let include = if reasoning.is_some() {
        Some(vec!["reasoning.encrypted_content".into()])
    } else {
        None
    };

    let cache_retention = options.cache_retention.unwrap_or(CacheRetention::Short);
    let prompt_cache_key = if cache_retention == CacheRetention::None {
        None
    } else {
        options.session_id.clone()
    };

    let prompt_cache_retention = match cache_retention {
        CacheRetention::Long if !is_codex && model.base_url.contains("api.openai.com") => {
            Some("24h".into())
        }
        _ => None,
    };

    let tool_fields = if converted_tools.is_some() {
        (Some("auto".into()), Some(true))
    } else {
        (None, None)
    };

    RequestBody {
        model: model.id.to_string(),
        input,
        stream: true,
        store: false,
        max_output_tokens: options.max_tokens.or(Some(model.max_output_tokens)),
        temperature: if reasoning.is_none() {
            options.temperature.map(|t| t.value())
        } else {
            None
        },
        tools: converted_tools,
        tool_choice: tool_fields.0,
        parallel_tool_calls: tool_fields.1,
        reasoning,
        include,
        prompt_cache_key,
        prompt_cache_retention,
    }
}

fn convert_input_messages(
    model: &Model,
    system_prompt: &Option<String>,
    messages: &[Message],
) -> Vec<serde_json::Value> {
    let mut converted = Vec::new();

    if let Some(prompt) = system_prompt {
        let role = if model.reasoning {
            "developer"
        } else {
            "system"
        };
        converted.push(serde_json::json!({
            "role": role,
            "content": [{ "type": "input_text", "text": prompt }],
        }));
    }

    for msg in messages {
        match msg {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => {
                    converted.push(serde_json::json!({
                        "role": "user",
                        "content": [{ "type": "input_text", "text": text }],
                    }));
                }
                UserContent::Parts(parts) => {
                    let mut content = Vec::new();
                    for part in parts {
                        match part {
                            UserContentPart::Text(text) => {
                                content.push(serde_json::json!({
                                    "type": "input_text",
                                    "text": text.text,
                                }));
                            }
                            UserContentPart::Image(img)
                                if model.input.contains(&InputModality::Image) =>
                            {
                                content.push(serde_json::json!({
                                    "type": "input_image",
                                    "detail": "auto",
                                    "image_url": format!(
                                        "data:{};base64,{}",
                                        img.mime_type, img.data
                                    ),
                                }));
                            }
                            UserContentPart::Image(_) => {}
                        }
                    }

                    if !content.is_empty() {
                        converted.push(serde_json::json!({
                            "role": "user",
                            "content": content,
                        }));
                    }
                }
            },
            Message::Assistant(asst) => {
                for part in &asst.content {
                    match part {
                        AssistantContentPart::Text(text) if !text.text.is_empty() => {
                            converted.push(serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text.text }],
                            }));
                        }
                        AssistantContentPart::ToolCall(tc) => {
                            converted.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": normalized_call_id(tc.id.as_str()),
                                "name": tc.name.as_str(),
                                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(tr) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let has_images = tr
                    .content
                    .iter()
                    .any(|p| matches!(p, ToolResultContentPart::Image(_)));

                converted.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": normalized_call_id(tr.tool_call_id.as_str()),
                    "output": if text.is_empty() && has_images {
                        "(see attached image)"
                    } else {
                        text.as_str()
                    },
                }));

                if has_images && model.input.contains(&InputModality::Image) {
                    let mut content = vec![serde_json::json!({
                        "type": "input_text",
                        "text": "attached image(s) from tool result:",
                    })];
                    for part in &tr.content {
                        if let ToolResultContentPart::Image(img) = part {
                            content.push(serde_json::json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!(
                                    "data:{};base64,{}",
                                    img.mime_type, img.data
                                ),
                            }));
                        }
                    }
                    converted.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
        }
    }

    converted
}

fn normalized_call_id(id: &str) -> &str {
    id.split('|').next().unwrap_or(id)
}

fn is_codex_provider(model: &Model) -> bool {
    matches!(&model.provider, Provider::Custom(name) if name == "openai-codex")
        || model.base_url.contains("chatgpt.com/backend-api")
}

fn resolve_url(model: &Model, is_codex: bool) -> String {
    if is_codex {
        let raw = if model.base_url.trim().is_empty() {
            DEFAULT_CODEX_BASE_URL
        } else {
            model.base_url.trim()
        };
        let base = raw.trim_end_matches('/');
        if base.ends_with("/codex/responses") {
            base.to_string()
        } else if base.ends_with("/codex") {
            format!("{base}/responses")
        } else {
            format!("{base}/codex/responses")
        }
    } else {
        let base = model.base_url.trim_end_matches('/');
        if base.ends_with("/responses") {
            base.to_string()
        } else {
            format!("{base}/responses")
        }
    }
}

fn build_headers(
    api_key: &ApiKey,
    options: &StreamOptions,
    is_codex: bool,
) -> Result<HeaderMap, ProviderError> {
    let key: &str = api_key;
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|e| ProviderError::Other(e.to_string()))?,
    );

    if is_codex {
        let account_id = options
            .account_id
            .clone()
            .or_else(|| extract_account_id(api_key));

        if let Some(account_id) = account_id {
            headers.insert(
                HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_str(&account_id)
                    .map_err(|e| ProviderError::Other(e.to_string()))?,
            );
        }
        headers.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("responses=experimental"),
        );
        headers.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static("mush"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("mush"));
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );

        if let Some(session_id) = &options.session_id {
            headers.insert(
                HeaderName::from_static("session_id"),
                HeaderValue::from_str(session_id)
                    .map_err(|e| ProviderError::Other(e.to_string()))?,
            );
        }
    }

    Ok(headers)
}

fn extract_account_id(token: &str) -> Option<String> {
    let payload = decode_jwt_payload(token)?;

    payload
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            payload
                .get(OPENAI_AUTH_CLAIM_PATH)
                .and_then(|v| v.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            payload
                .get("organizations")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|org| org.get("id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload_b64 = token.split('.').nth(1)?;

    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .ok()?;

    serde_json::from_slice::<serde_json::Value>(&decoded).ok()
}

#[derive(Debug, Clone)]
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

#[derive(Debug)]
struct ActiveBlock {
    content_index: usize,
    block: CurrentBlock,
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
            model: model_id,
            provider: provider_name,
            api,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::now(),
        };

        let mut active: HashMap<u64, ActiveBlock> = HashMap::new();
        let mut chunk_buf = Vec::new();
        let mut line_buf = String::new();
        let mut event_name: Option<String> = None;

        use futures::TryStreamExt;
        let mut byte_stream = response.bytes_stream();

        yield StreamEvent::Start { partial: output.clone() };

        loop {
            match byte_stream.try_next().await {
                Ok(Some(chunk)) => {
                    chunk_buf.extend_from_slice(&chunk);

                    while let Some(newline_pos) = chunk_buf.iter().position(|&b| b == b'\n') {
                        let line = String::from_utf8_lossy(&chunk_buf[..newline_pos]).to_string();
                        chunk_buf.drain(..=newline_pos);
                        let line = line.trim_end_matches('\r');

                        if line.is_empty() {
                            if !line_buf.is_empty() {
                                let data = line_buf.trim();
                                if data != "[DONE]"
                                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(data)
                                {
                                    for event in process_sse_event(
                                        event_name.as_deref(),
                                        json,
                                        &mut output,
                                        &mut active,
                                    ) {
                                        let is_error = matches!(event, StreamEvent::Error { .. });
                                        yield event;
                                        if is_error {
                                            return;
                                        }
                                    }
                                }
                            }
                            line_buf.clear();
                            event_name = None;
                            continue;
                        }

                        if let Some(rest) = line.strip_prefix("event: ") {
                            event_name = Some(rest.to_string());
                            continue;
                        }

                        if let Some(rest) = line.strip_prefix("data: ") {
                            if !line_buf.is_empty() {
                                line_buf.push('\n');
                            }
                            line_buf.push_str(rest);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    finish_all_blocks(&mut active, &mut output);
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

        finish_all_blocks(&mut active, &mut output);
        if output.stop_reason == StopReason::Stop
            && output
                .content
                .iter()
                .any(|p| matches!(p, AssistantContentPart::ToolCall(_)))
        {
            output.stop_reason = StopReason::ToolUse;
        }

        yield StreamEvent::Done {
            reason: output.stop_reason,
            message: output,
        };
    };

    Box::pin(event_stream)
}

fn process_sse_event(
    event_name: Option<&str>,
    json: serde_json::Value,
    output: &mut AssistantMessage,
    active: &mut HashMap<u64, ActiveBlock>,
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    let event_type = event_name.map(str::to_string).or_else(|| {
        json.get("type")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });

    let Some(event_type) = event_type else {
        return events;
    };

    match event_type.as_str() {
        "response.output_item.added" => {
            let item = json.get("item").cloned().unwrap_or_default();
            let output_index = json
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            match item.get("type").and_then(|v| v.as_str()) {
                Some("message") => {
                    let content_index = output.content.len();
                    output.content.push(AssistantContentPart::Text(TextContent {
                        text: String::new(),
                    }));
                    active.insert(
                        output_index,
                        ActiveBlock {
                            content_index,
                            block: CurrentBlock::Text {
                                text: String::new(),
                            },
                        },
                    );
                    events.push(StreamEvent::TextStart { content_index });
                }
                Some("reasoning") => {
                    let content_index = output.content.len();
                    output
                        .content
                        .push(AssistantContentPart::Thinking(ThinkingContent {
                            thinking: String::new(),
                            signature: None,
                            redacted: false,
                        }));
                    active.insert(
                        output_index,
                        ActiveBlock {
                            content_index,
                            block: CurrentBlock::Thinking {
                                text: String::new(),
                            },
                        },
                    );
                    events.push(StreamEvent::ThinkingStart { content_index });
                }
                Some("function_call") => {
                    let call_id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    let id = if item_id.is_empty() {
                        call_id.to_string()
                    } else {
                        format!("{call_id}|{item_id}")
                    };
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let args_buf = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();

                    let content_index = output.content.len();
                    output
                        .content
                        .push(AssistantContentPart::ToolCall(ToolCall {
                            id: ToolCallId::from(id.clone()),
                            name: ToolName::from(name.clone()),
                            arguments: serde_json::from_str::<serde_json::Value>(&args_buf)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        }));

                    active.insert(
                        output_index,
                        ActiveBlock {
                            content_index,
                            block: CurrentBlock::ToolCall { id, name, args_buf },
                        },
                    );
                    events.push(StreamEvent::ToolCallStart { content_index });
                }
                _ => {}
            }
        }
        "response.output_text.delta" | "response.refusal.delta" => {
            let delta = json
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let output_index = json
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            if let Some(active_block) = active.get_mut(&output_index)
                && let CurrentBlock::Text { text } = &mut active_block.block
            {
                text.push_str(&delta);
                if let Some(AssistantContentPart::Text(content)) =
                    output.content.get_mut(active_block.content_index)
                {
                    content.text.push_str(&delta);
                }
                events.push(StreamEvent::TextDelta {
                    content_index: active_block.content_index,
                    delta,
                });
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let delta = json
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let output_index = json
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            if let Some(active_block) = active.get_mut(&output_index)
                && let CurrentBlock::Thinking { text } = &mut active_block.block
            {
                text.push_str(&delta);
                if let Some(AssistantContentPart::Thinking(content)) =
                    output.content.get_mut(active_block.content_index)
                {
                    content.thinking.push_str(&delta);
                }
                events.push(StreamEvent::ThinkingDelta {
                    content_index: active_block.content_index,
                    delta,
                });
            }
        }
        "response.function_call_arguments.delta" => {
            let delta = json
                .get("delta")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let output_index = json
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            if let Some(active_block) = active.get_mut(&output_index)
                && let CurrentBlock::ToolCall { args_buf, .. } = &mut active_block.block
            {
                args_buf.push_str(&delta);
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args_buf)
                    && let Some(AssistantContentPart::ToolCall(tc)) =
                        output.content.get_mut(active_block.content_index)
                {
                    tc.arguments = parsed;
                }
                events.push(StreamEvent::ToolCallDelta {
                    content_index: active_block.content_index,
                    delta,
                });
            }
        }
        "response.output_item.done" => {
            let output_index = json
                .get("output_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if let Some(active_block) = active.remove(&output_index) {
                let item = json.get("item");
                finish_block(item, active_block, output, &mut events);
            }
        }
        "response.completed" => {
            if let Some(response) = json.get("response") {
                if let Some(usage) = response.get("usage") {
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read_tokens = usage
                        .get("input_tokens_details")
                        .and_then(|v| v.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    output.usage = Usage {
                        input_tokens: input_tokens.saturating_sub(cache_read_tokens),
                        output_tokens,
                        cache_read_tokens,
                        cache_write_tokens: 0,
                    };
                }

                let status = response
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("completed");
                output.stop_reason = map_response_status(status);
            }
        }
        "response.incomplete" => {
            output.stop_reason = StopReason::Length;
        }
        "response.failed" | "error" => {
            finish_all_blocks(active, output);
            output.stop_reason = StopReason::Error;
            output.error_message = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    json.get("message")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .or_else(|| Some("openai responses stream failed".into()));
            events.push(StreamEvent::Error {
                reason: StopReason::Error,
                message: output.clone(),
            });
        }
        _ => {}
    }

    events
}

fn finish_block(
    item: Option<&serde_json::Value>,
    active_block: ActiveBlock,
    output: &mut AssistantMessage,
    events: &mut Vec<StreamEvent>,
) {
    match active_block.block {
        CurrentBlock::Text { text } => {
            let final_text = item
                .and_then(|i| i.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.get("text").or_else(|| v.get("refusal")))
                .and_then(|v| v.as_str())
                .unwrap_or(&text)
                .to_string();

            if let Some(AssistantContentPart::Text(content)) =
                output.content.get_mut(active_block.content_index)
            {
                content.text = final_text.clone();
            }

            events.push(StreamEvent::TextEnd {
                content_index: active_block.content_index,
                text: final_text,
            });
        }
        CurrentBlock::Thinking { text } => {
            if let Some(AssistantContentPart::Thinking(content)) =
                output.content.get_mut(active_block.content_index)
            {
                content.thinking = text.clone();
            }

            events.push(StreamEvent::ThinkingEnd {
                content_index: active_block.content_index,
                thinking: text,
            });
        }
        CurrentBlock::ToolCall {
            mut id,
            mut name,
            mut args_buf,
        } => {
            if let Some(item) = item {
                if let Some(call_id) = item.get("call_id").and_then(|v| v.as_str()) {
                    let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                    id = if item_id.is_empty() {
                        call_id.to_string()
                    } else {
                        format!("{call_id}|{item_id}")
                    };
                }
                if let Some(item_name) = item.get("name").and_then(|v| v.as_str()) {
                    name = item_name.to_string();
                }
                if let Some(arguments) = item.get("arguments").and_then(|v| v.as_str()) {
                    args_buf = arguments.to_string();
                }
            }

            let arguments =
                serde_json::from_str(&args_buf).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(AssistantContentPart::ToolCall(tc)) =
                output.content.get_mut(active_block.content_index)
            {
                tc.id = ToolCallId::from(id.clone());
                tc.name = ToolName::from(name.clone());
                tc.arguments = arguments.clone();
            }

            events.push(StreamEvent::ToolCallEnd {
                content_index: active_block.content_index,
                id,
                name,
                arguments,
            });
        }
    }
}

fn finish_all_blocks(active: &mut HashMap<u64, ActiveBlock>, output: &mut AssistantMessage) {
    let mut remaining = active.drain().map(|(_, block)| block).collect::<Vec<_>>();
    remaining.sort_by_key(|b| b.content_index);

    for block in remaining {
        let mut sink = Vec::new();
        finish_block(None, block, output, &mut sink);
    }
}

fn map_response_status(status: &str) -> StopReason {
    match status {
        "completed" => StopReason::Stop,
        "incomplete" => StopReason::Length,
        "failed" | "cancelled" => StopReason::Error,
        "queued" | "in_progress" => StopReason::Stop,
        _ => StopReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_standard_url() {
        let model = Model {
            id: "gpt-5.2".into(),
            name: "GPT-5.2".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai".into()),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_output_tokens: 32768,
        };

        assert_eq!(
            resolve_url(&model, false),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn resolve_codex_url() {
        let model = Model {
            id: "gpt-5.2-codex".into(),
            name: "GPT-5.2 Codex".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai-codex".into()),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: true,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_output_tokens: 32768,
        };

        assert_eq!(
            resolve_url(&model, true),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn parse_account_id_from_jwt_claim() {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc_123",
            }
        });
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
        let token = format!("header.{payload_b64}.sig");

        assert_eq!(extract_account_id(&token), Some("acc_123".into()));
    }

    #[test]
    fn auth_header_contains_actual_key() {
        let options = StreamOptions::default();
        let key = ApiKey::new("sk-test-secret-key-123").unwrap();
        let headers = build_headers(&key, &options, false).expect("headers should build");
        let auth = headers.get("authorization").unwrap().to_str().unwrap();
        assert_eq!(auth, "Bearer sk-test-secret-key-123");
    }

    #[test]
    fn codex_headers_allow_missing_account_id() {
        let options = StreamOptions::default();
        let key = ApiKey::new("not-a-jwt").unwrap();
        let headers = build_headers(&key, &options, true).expect("headers should build");
        assert!(headers.get("chatgpt-account-id").is_none());
    }

    #[test]
    fn build_body_respects_cache_retention() {
        let model = Model {
            id: "gpt-5.2".into(),
            name: "GPT-5.2".into(),
            api: Api::OpenaiResponses,
            provider: Provider::Custom("openai".into()),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_output_tokens: 32768,
        };

        let options = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            session_id: Some("sess_abc".into()),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);
        assert_eq!(body.prompt_cache_key.as_deref(), Some("sess_abc"));
        assert_eq!(body.prompt_cache_retention.as_deref(), Some("24h"));
    }

    #[test]
    fn map_statuses() {
        assert_eq!(map_response_status("completed"), StopReason::Stop);
        assert_eq!(map_response_status("incomplete"), StopReason::Length);
        assert_eq!(map_response_status("failed"), StopReason::Error);
    }
}
