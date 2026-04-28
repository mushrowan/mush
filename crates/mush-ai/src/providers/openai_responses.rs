//! openai responses API provider
//!
//! supports both direct openai `/responses` and codex subscription
//! (`chatgpt.com/backend-api/codex/responses`) style endpoints.

use base64::Engine;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde::Serialize;

use crate::env::env_api_key;
use crate::registry::{ApiProvider, EventStream, LlmContext, ProviderError, ToolDefinition};
use crate::stream::StreamEvent;
use crate::types::*;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const OPENAI_AUTH_CLAIM_PATH: &str = "https://api.openai.com/auth";

pub struct OpenaiResponsesProvider {
    pub client: reqwest::Client,
}

#[async_trait::async_trait]
impl ApiProvider for OpenaiResponsesProvider {
    fn api(&self) -> Api {
        Api::OpenaiResponses
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

        let is_codex = is_codex_provider(model);
        let body = build_request_body(
            model,
            &context.system_prompt,
            &context.messages,
            &context.tools,
            options,
            is_codex,
        );

        let url = resolve_url(model, is_codex);
        let headers = build_headers(&api_key, options, is_codex)?;
        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .send()
            .await?;

        let response =
            super::check_response(response, "openai responses", model.id.as_str(), &url).await?;

        Ok(parse_sse_stream(
            response,
            model.id.clone(),
            model.provider.clone(),
            model.api,
        ))
    }
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    input: Vec<serde_json::Value>,
    stream: bool,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
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
    /// auto-truncate input when it exceeds the model's context window
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation: Option<String>,
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
    // codex: system prompt goes in top-level `instructions`, not in input
    let input_system_prompt = if is_codex { &None } else { system_prompt };
    // openai responses API: always use sliding window (no explicit cache control)
    let input = convert_input_messages(
        model,
        input_system_prompt,
        messages,
        ToolResultTrimming::SlidingWindow,
    );
    // codex endpoint requires `instructions` to be present, even if empty
    let instructions = if is_codex {
        Some(system_prompt.clone().unwrap_or_default())
    } else {
        None
    };

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

    let reasoning_effort = options
        .thinking
        .and_then(|level| super::openai_reasoning_effort(model, level));

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
        options.session_id.as_ref().map(ToString::to_string)
    };

    let prompt_cache_retention = match cache_retention {
        CacheRetention::Long if !is_codex && model.base_url.contains("api.openai.com") => {
            Some("24h".into())
        }
        _ => None,
    };

    let tool_fields = if converted_tools.is_some() {
        // codex endpoint (chatgpt.com/backend-api) doesn't support parallel_tool_calls
        (
            Some("auto".into()),
            if is_codex { None } else { Some(true) },
        )
    } else {
        (None, None)
    };

    RequestBody {
        model: model.id.to_string(),
        input,
        stream: true,
        store: false,
        instructions,
        // chatgpt.com/backend-api doesn't support max_output_tokens
        max_output_tokens: if is_codex {
            None
        } else {
            Some(options.max_tokens.unwrap_or(model.max_output_tokens).get())
        },
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
        // auto-truncate input to avoid context overflow errors
        // codex endpoint (chatgpt.com/backend-api) doesn't support this param
        truncation: if is_codex { None } else { Some("auto".into()) },
    }
}

use super::{MessageVisitor, maybe_trim_tool_output, walk_messages};

fn convert_input_messages(
    model: &Model,
    system_prompt: &Option<String>,
    messages: &[Message],
    trimming: ToolResultTrimming,
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

    let mut visitor = OpenaiResponsesVisitor {
        model,
        trimming,
        out: &mut converted,
    };
    walk_messages(messages, &mut visitor);

    converted
}

struct OpenaiResponsesVisitor<'a> {
    model: &'a Model,
    trimming: ToolResultTrimming,
    out: &'a mut Vec<serde_json::Value>,
}

impl MessageVisitor for OpenaiResponsesVisitor<'_> {
    fn on_user(&mut self, user: &UserMessage) {
        match &user.content {
            UserContent::Text(text) => {
                self.out.push(serde_json::json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                }));
            }
            UserContent::Parts(parts) => {
                let mut content = Vec::new();
                for part in parts {
                    match part {
                        UserContentPart::Text(text) if text.text.is_empty() => {}
                        UserContentPart::Text(text) => {
                            content.push(serde_json::json!({
                                "type": "input_text",
                                "text": text.text,
                            }));
                        }
                        UserContentPart::Image(img)
                            if self.model.input.contains(&InputModality::Image) =>
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
                    self.out.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
        }
    }

    fn on_assistant(&mut self, asst: &AssistantMessage) {
        for part in &asst.content {
            match part {
                AssistantContentPart::Text(text) if !text.text.is_empty() => {
                    self.out.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text.text }],
                    }));
                }
                AssistantContentPart::ToolCall(tc) => {
                    self.out.push(serde_json::json!({
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

        let text = maybe_trim_tool_output(&raw_text, is_old_turn, self.trimming);

        let has_images = tr
            .content
            .iter()
            .any(|p| matches!(p, ToolResultContentPart::Image(_)));

        self.out.push(serde_json::json!({
            "type": "function_call_output",
            "call_id": normalized_call_id(tr.tool_call_id.as_str()),
            "output": if text.is_empty() && has_images {
                "(see attached image)".to_string()
            } else {
                text
            },
        }));

        if has_images && self.model.input.contains(&InputModality::Image) {
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
            self.out.push(serde_json::json!({
                "role": "user",
                "content": content,
            }));
        }
    }
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
    let key = api_key.expose();
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let mut auth = HeaderValue::from_str(&format!("Bearer {key}"))?;
    auth.set_sensitive(true);
    headers.insert(AUTHORIZATION, auth);

    if is_codex {
        let account_id = options
            .account_id
            .clone()
            .or_else(|| extract_account_id(api_key.expose()));

        if let Some(account_id) = account_id {
            headers.insert(
                HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_str(&account_id)?,
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
                HeaderValue::from_str(session_id.as_ref())?,
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

// typed event structs for hot-path deserialization (avoids serde_json::Value lookups)

#[derive(serde::Deserialize)]
struct DeltaEvent {
    #[serde(default)]
    output_index: u64,
    #[serde(default)]
    delta: String,
}

#[derive(serde::Deserialize)]
struct OutputIndexEvent {
    #[serde(default)]
    output_index: u64,
    #[serde(default)]
    item: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct OutputItemAddedEvent {
    #[serde(default)]
    output_index: u64,
    #[serde(default)]
    item: Option<AddedItem>,
}

#[derive(serde::Deserialize)]
struct AddedItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(serde::Deserialize)]
struct CompletedEvent {
    #[serde(default)]
    response: Option<CompletedResponse>,
}

#[derive(serde::Deserialize)]
struct CompletedResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<CompletedUsage>,
}

#[derive(serde::Deserialize)]
struct CompletedUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
}

#[derive(serde::Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(serde::Deserialize)]
struct ErrorEvent {
    #[serde(default)]
    error: Option<ErrorDetail>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(serde::Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    message: Option<String>,
}

use super::StreamBlock;

#[derive(Debug)]
struct ActiveBlock {
    content_index: usize,
    block: StreamBlock,
}

#[derive(Debug, Default)]
struct ActiveBlocks {
    entries: Vec<(u64, ActiveBlock)>,
}

impl ActiveBlocks {
    fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, output_index: u64, block: ActiveBlock) {
        if let Some((_, active_block)) = self
            .entries
            .iter_mut()
            .find(|(index, _)| *index == output_index)
        {
            *active_block = block;
            return;
        }

        self.entries.push((output_index, block));
    }

    fn get_mut(&mut self, output_index: u64) -> Option<&mut ActiveBlock> {
        self.entries
            .iter_mut()
            .find(|(index, _)| *index == output_index)
            .map(|(_, block)| block)
    }

    fn remove(&mut self, output_index: u64) -> Option<ActiveBlock> {
        let entry_index = self
            .entries
            .iter()
            .position(|(index, _)| *index == output_index)?;
        Some(self.entries.swap_remove(entry_index).1)
    }

    fn take_sorted(&mut self) -> Vec<ActiveBlock> {
        let mut remaining = std::mem::take(&mut self.entries);
        remaining.sort_by_key(|(_, block)| block.content_index);
        remaining.into_iter().map(|(_, block)| block).collect()
    }
}

struct OpenAiResponsesProcessor {
    active: ActiveBlocks,
}

impl super::sse::SseProcessor for OpenAiResponsesProcessor {
    fn process(
        &mut self,
        raw: &super::sse::SseRawEvent,
        output: &mut AssistantMessage,
    ) -> super::sse::ProcessResult {
        let data = raw.data.trim();
        if data == "[DONE]" {
            return super::sse::ProcessResult::Skip;
        }
        if !data.starts_with('{') {
            return super::sse::ProcessResult::Skip;
        }
        let events = process_sse_event(raw.event.as_deref(), data, output, &mut self.active);
        // if any event is a fatal error, split it out
        for event in &events {
            if matches!(event, StreamEvent::Error { .. }) {
                return super::sse::ProcessResult::Fatal(
                    events.into_iter().next().unwrap_or_else(|| unreachable!()),
                );
            }
        }
        super::sse::ProcessResult::Events(events)
    }

    fn finish(&mut self, output: &mut AssistantMessage) {
        finish_all_blocks(&mut self.active, output);
        if output.stop_reason == StopReason::Stop
            && output
                .content
                .iter()
                .any(|p| matches!(p, AssistantContentPart::ToolCall(_)))
        {
            output.stop_reason = StopReason::ToolUse;
        }
    }

    fn label(&self) -> &'static str {
        "openai responses"
    }
}

fn parse_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
) -> EventStream {
    let processor = OpenAiResponsesProcessor {
        active: ActiveBlocks::new(),
    };
    super::sse::run_sse_stream(response, model_id, provider_name, api, processor)
}

fn process_sse_event(
    event_name: Option<&str>,
    data: &str,
    output: &mut AssistantMessage,
    active: &mut ActiveBlocks,
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    // for events without an explicit SSE event name, fall back to json "type" field
    let event_type = match event_name {
        Some(name) => name,
        None => {
            // only parse enough to get the type field
            if let Some(start) = data.find("\"type\"") {
                let rest = &data[start + 6..];
                if let Some(colon) = rest.find(':') {
                    let after = rest[colon + 1..].trim_start();
                    if let Some(s) = after.strip_prefix('"') {
                        if let Some(end) = s.find('"') {
                            &s[..end]
                        } else {
                            return events;
                        }
                    } else {
                        return events;
                    }
                } else {
                    return events;
                }
            } else {
                return events;
            }
        }
    };

    match event_type {
        // delta events are the hot path, use typed deserialization
        "response.output_text.delta" | "response.refusal.delta" => {
            let Ok(ev) = serde_json::from_str::<DeltaEvent>(data) else {
                return events;
            };
            if let Some(active_block) = active.get_mut(ev.output_index)
                && let StreamBlock::Text { text } = &mut active_block.block
            {
                text.push_str(&ev.delta);
                if let Some(AssistantContentPart::Text(content)) =
                    output.content.get_mut(active_block.content_index)
                {
                    content.text.push_str(&ev.delta);
                }
                events.push(StreamEvent::TextDelta {
                    content_index: active_block.content_index,
                    delta: ev.delta,
                });
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let Ok(ev) = serde_json::from_str::<DeltaEvent>(data) else {
                return events;
            };
            if let Some(active_block) = active.get_mut(ev.output_index)
                && let StreamBlock::Thinking { text, .. } = &mut active_block.block
            {
                text.push_str(&ev.delta);
                if let Some(AssistantContentPart::Thinking(content)) =
                    output.content.get_mut(active_block.content_index)
                    && let Some(buf) = content.text_mut()
                {
                    buf.push_str(&ev.delta);
                }
                events.push(StreamEvent::ThinkingDelta {
                    content_index: active_block.content_index,
                    delta: ev.delta,
                });
            }
        }
        "response.function_call_arguments.delta" => {
            let Ok(ev) = serde_json::from_str::<DeltaEvent>(data) else {
                return events;
            };
            if let Some(active_block) = active.get_mut(ev.output_index)
                && let StreamBlock::ToolCall { args_buf, .. } = &mut active_block.block
            {
                args_buf.push_str(&ev.delta);
                events.push(StreamEvent::ToolCallDelta {
                    content_index: active_block.content_index,
                    delta: ev.delta,
                });
            }
        }
        // less frequent events
        "response.output_item.added" => {
            let Ok(ev) = serde_json::from_str::<OutputItemAddedEvent>(data) else {
                return events;
            };
            let Some(item) = ev.item else {
                return events;
            };
            match item.item_type.as_deref() {
                Some("message") => {
                    let content_index = output.content.len();
                    output.content.push(AssistantContentPart::Text(TextContent {
                        text: String::new(),
                    }));
                    active.insert(
                        ev.output_index,
                        ActiveBlock {
                            content_index,
                            block: StreamBlock::Text {
                                text: String::new(),
                            },
                        },
                    );
                    events.push(StreamEvent::TextStart { content_index });
                }
                Some("reasoning") => {
                    let content_index = output.content.len();
                    output.content.push(AssistantContentPart::Thinking(
                        ThinkingContent::Thinking {
                            thinking: String::new(),
                            signature: None,
                        },
                    ));
                    active.insert(
                        ev.output_index,
                        ActiveBlock {
                            content_index,
                            block: StreamBlock::Thinking {
                                text: String::new(),
                                signature: None,
                            },
                        },
                    );
                    events.push(StreamEvent::ThinkingStart { content_index });
                }
                Some("function_call") => {
                    let call_id = item.call_id.as_deref().unwrap_or_default();
                    let item_id = item.id.as_deref().unwrap_or_default();
                    let id = if item_id.is_empty() {
                        call_id.to_string()
                    } else {
                        format!("{call_id}|{item_id}")
                    };
                    let name = item.name.unwrap_or_default();
                    let args_buf = item.arguments.unwrap_or_default();

                    let content_index = output.content.len();
                    output
                        .content
                        .push(AssistantContentPart::ToolCall(ToolCall {
                            id: ToolCallId::from(id.clone()),
                            name: ToolName::from(name.clone()),
                            arguments: serde_json::json!({}),
                        }));

                    active.insert(
                        ev.output_index,
                        ActiveBlock {
                            content_index,
                            block: StreamBlock::ToolCall { id, name, args_buf },
                        },
                    );
                    events.push(StreamEvent::ToolCallStart { content_index });
                }
                _ => {}
            }
        }
        "response.output_item.done" => {
            let Ok(ev) = serde_json::from_str::<OutputIndexEvent>(data) else {
                return events;
            };
            if let Some(active_block) = active.remove(ev.output_index) {
                finish_block(ev.item.as_ref(), active_block, output, &mut events);
            }
        }
        "response.completed" => {
            let Ok(ev) = serde_json::from_str::<CompletedEvent>(data) else {
                return events;
            };
            if let Some(response) = ev.response {
                if let Some(usage) = response.usage {
                    let cache_read = usage.input_tokens_details.map_or(0, |d| d.cached_tokens);
                    output.usage = Usage {
                        input_tokens: TokenCount::new(
                            usage.input_tokens.saturating_sub(cache_read),
                        ),
                        output_tokens: TokenCount::new(usage.output_tokens),
                        cache_read_tokens: TokenCount::new(cache_read),
                        cache_write_tokens: TokenCount::ZERO,
                    };
                }
                output.stop_reason =
                    map_response_status(response.status.as_deref().unwrap_or("completed"));
            }
        }
        "response.incomplete" => {
            output.stop_reason = StopReason::Length;
        }
        "response.failed" | "error" => {
            finish_all_blocks(active, output);
            output.stop_reason = StopReason::Error;
            if let Ok(ev) = serde_json::from_str::<ErrorEvent>(data) {
                output.error_message = ev
                    .error
                    .and_then(|e| e.message)
                    .or(ev.message)
                    .or_else(|| Some("openai responses stream failed".into()));
            } else {
                output.error_message = Some("openai responses stream failed".into());
            }
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
        StreamBlock::Text { text } => {
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
        StreamBlock::Thinking { text, .. } => {
            if let Some(AssistantContentPart::Thinking(content)) =
                output.content.get_mut(active_block.content_index)
                && let Some(buf) = content.text_mut()
            {
                *buf = text.clone();
            }

            events.push(StreamEvent::ThinkingEnd {
                content_index: active_block.content_index,
                thinking: text,
            });
        }
        StreamBlock::ToolCall {
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

fn finish_all_blocks(active: &mut ActiveBlocks, output: &mut AssistantMessage) {
    for block in active.take_sorted() {
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

#[doc(hidden)]
pub fn benchmark_tool_call_deltas(chunk_count: usize, arg_bytes: usize) -> usize {
    let (full_json, fragments) =
        super::bench_support::tool_call_json_fragments(chunk_count, arg_bytes);
    let mut output = AssistantMessage {
        content: vec![],
        model: "bench".into(),
        provider: Provider::Custom("bench".into()),
        api: Api::OpenaiResponses,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp_ms: Timestamp::zero(),
    };
    let mut active = ActiveBlocks::new();

    let first = fragments
        .first()
        .cloned()
        .unwrap_or_else(|| full_json.clone());

    // pre-build JSON strings (simulates SSE data arriving as &str)
    let added = format!(
        r#"{{"output_index":0,"item":{{"type":"function_call","call_id":"call_1","id":"item_1","name":"read","arguments":{}}}}}"#,
        serde_json::to_string(&first).unwrap_or_default()
    );
    let delta_strings: Vec<String> = fragments
        .iter()
        .skip(1)
        .map(|f| {
            format!(
                r#"{{"output_index":0,"delta":{}}}"#,
                serde_json::to_string(f).unwrap_or_default()
            )
        })
        .collect();
    let done = format!(
        r#"{{"output_index":0,"item":{{"type":"function_call","call_id":"call_1","id":"item_1","name":"read","arguments":{}}}}}"#,
        serde_json::to_string(&full_json).unwrap_or_default()
    );

    let _ = process_sse_event(
        Some("response.output_item.added"),
        &added,
        &mut output,
        &mut active,
    );

    for delta_json in &delta_strings {
        let _ = process_sse_event(
            Some("response.function_call_arguments.delta"),
            delta_json,
            &mut output,
            &mut active,
        );
    }

    let _ = process_sse_event(
        Some("response.output_item.done"),
        &done,
        &mut output,
        &mut active,
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
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(32768),
            supports_adaptive_thinking: false,
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
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(32768),
            supports_adaptive_thinking: false,
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
    fn auth_header_marked_sensitive_so_debug_redacts_it() {
        let options = StreamOptions::default();
        let key = ApiKey::new("sk-test-secret-key-123").unwrap();
        let headers = build_headers(&key, &options, false).expect("headers should build");
        let hv = headers.get("authorization").unwrap();
        assert!(
            hv.is_sensitive(),
            "authorization header must be sensitive so HeaderMap Debug doesn't leak the bearer"
        );
        let dbg = format!("{headers:?}");
        assert!(
            !dbg.contains("sk-test-secret-key-123"),
            "bearer leaked in HeaderMap Debug: {dbg}"
        );
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
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(32768),
            supports_adaptive_thinking: false,
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

    #[test]
    fn tool_call_args_materialise_at_block_end() {
        let mut output = AssistantMessage {
            content: vec![],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::OpenaiResponses,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        };
        let mut active = ActiveBlocks::new();

        let added = serde_json::json!({
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "id": "item_1",
                "name": "read",
                "arguments": r#"{"path":""#,
            }
        })
        .to_string();
        let start = process_sse_event(
            Some("response.output_item.added"),
            &added,
            &mut output,
            &mut active,
        );
        assert!(
            start
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
        );

        let delta_json = serde_json::json!({
            "output_index": 0,
            "delta": r#"foo.rs"}"#,
        })
        .to_string();
        let delta = process_sse_event(
            Some("response.function_call_arguments.delta"),
            &delta_json,
            &mut output,
            &mut active,
        );
        assert!(
            delta
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolCallDelta { .. }))
        );

        match &output.content[0] {
            AssistantContentPart::ToolCall(tc) => {
                assert_eq!(tc.arguments, serde_json::json!({}));
            }
            other => panic!("expected tool call, got {other:?}"),
        }

        let done_json = serde_json::json!({
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "id": "item_1",
                "name": "read",
                "arguments": r#"{"path":"foo.rs"}"#,
            }
        })
        .to_string();
        let done = process_sse_event(
            Some("response.output_item.done"),
            &done_json,
            &mut output,
            &mut active,
        );

        match &done[0] {
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
    fn benchmark_tool_call_deltas_returns_payload_size() {
        assert_eq!(benchmark_tool_call_deltas(8, 1024), 1024);
    }

    #[test]
    fn active_blocks_replace_existing_output_index() {
        let mut active = ActiveBlocks::new();
        active.insert(
            7,
            ActiveBlock {
                content_index: 0,
                block: StreamBlock::Text {
                    text: "first".into(),
                },
            },
        );
        active.insert(
            7,
            ActiveBlock {
                content_index: 1,
                block: StreamBlock::Text {
                    text: "second".into(),
                },
            },
        );

        let Some(block) = active.remove(7) else {
            panic!("expected active block")
        };
        assert_eq!(block.content_index, 1);
    }

    #[test]
    fn finish_all_blocks_keeps_content_order_with_sparse_indexes() {
        let mut output = AssistantMessage {
            content: vec![
                AssistantContentPart::Thinking(ThinkingContent::Thinking {
                    thinking: String::new(),
                    signature: None,
                }),
                AssistantContentPart::Text(TextContent {
                    text: String::new(),
                }),
            ],
            model: "test".into(),
            provider: Provider::Custom("test".into()),
            api: Api::OpenaiResponses,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp_ms: Timestamp::zero(),
        };
        let mut active = ActiveBlocks::new();
        active.insert(
            42,
            ActiveBlock {
                content_index: 1,
                block: StreamBlock::Text {
                    text: "hello".into(),
                },
            },
        );
        active.insert(
            3,
            ActiveBlock {
                content_index: 0,
                block: StreamBlock::Thinking {
                    text: "thinking".into(),
                    signature: None,
                },
            },
        );

        finish_all_blocks(&mut active, &mut output);

        match &output.content[0] {
            AssistantContentPart::Thinking(ThinkingContent::Thinking { thinking, .. }) => {
                assert_eq!(thinking, "thinking");
            }
            other => panic!("expected thinking, got {other:?}"),
        }
        match &output.content[1] {
            AssistantContentPart::Text(TextContent { text }) => {
                assert_eq!(text, "hello");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn codex_model() -> Model {
        Model {
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
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(32768),
            supports_adaptive_thinking: false,
        }
    }

    #[test]
    fn codex_uses_instructions_field() {
        let model = codex_model();
        let prompt = Some("you are a coding assistant".into());
        let options = StreamOptions::default();

        let body = build_request_body(&model, &prompt, &[], &[], &options, true);
        assert_eq!(
            body.instructions.as_deref(),
            Some("you are a coding assistant")
        );
        // system prompt should not appear in input messages
        assert!(body.input.is_empty());
    }

    #[test]
    fn codex_sends_empty_instructions_when_no_system_prompt() {
        let model = codex_model();
        let options = StreamOptions::default();

        let body = build_request_body(&model, &None, &[], &[], &options, true);
        assert_eq!(body.instructions.as_deref(), Some(""));
    }

    #[test]
    fn non_codex_omits_instructions_field() {
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
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(32768),
            supports_adaptive_thinking: false,
        };
        let prompt = Some("you are a coding assistant".into());
        let options = StreamOptions::default();

        let body = build_request_body(&model, &prompt, &[], &[], &options, false);
        assert!(body.instructions.is_none());
        // system prompt should be in input as a developer message
        assert_eq!(body.input.len(), 1);
        assert_eq!(body.input[0]["role"], "developer");
    }

    #[test]
    fn gpt_5_4_preserves_xhigh_reasoning_effort() {
        let model = Model {
            id: "gpt-5.4".into(),
            name: "GPT-5.4".into(),
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
            context_window: TokenCount::new(1_050_000),
            max_output_tokens: TokenCount::new(128_000),
            supports_adaptive_thinking: false,
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, true);
        assert_eq!(
            body.reasoning.as_ref().map(|r| r.effort.as_str()),
            Some("xhigh")
        );
    }
}
