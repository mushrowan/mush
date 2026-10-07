use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::env::env_api_key;
use crate::registry::{ApiProvider, EventStream, LlmContext, ProviderError, ToolDefinition};
use crate::stream::StreamEvent;
use crate::types::*;

pub struct OpenaiCompletionsProvider {
    pub client: crate::http::HttpClient,
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
            .ok_or_else(|| {
                crate::auth::missing_key_error(&model.provider, options.oauth_account.as_ref())
            })?;
        let body = build_request_body(
            model,
            &context.system_prompt,
            &context.messages,
            &context.tools,
            options,
        );

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(AUTHORIZATION, super::bearer_header(api_key.expose())?);

        let base_url = &model.base_url;
        let url = format!("{base_url}/chat/completions");
        let call_id = crate::wire_log::next_call_id();

        tracing::debug!(model = %model.id, %url, "sending openai completions request");

        let response = super::post_json(&self.client, &url, headers, &body, call_id).await?;

        let response =
            super::check_response(response, "openai completions", model.id.as_str(), &url).await?;

        Ok(super::sse::run_sse_stream(
            response,
            model,
            call_id,
            OpenAiCompletionsProcessor::default(),
        ))
    }
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<RequestTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<serde_json::Value>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<serde_json::Value>,
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

    if let Some(prompt) = system_prompt {
        all_messages.push(serde_json::json!({
            "role": "system",
            "content": prompt,
        }));
    }

    let trimming =
        if options.cache_retention.unwrap_or(CacheRetention::Short) == CacheRetention::None {
            ToolResultTrimming::SlidingWindow
        } else {
            ToolResultTrimming::Preserve
        };
    let mut visitor = OpenaiVisitor {
        model,
        trimming,
        out: &mut all_messages,
        pending_images: Vec::new(),
    };
    super::walk_messages(&super::replayable_history(messages, model), &mut visitor);
    visitor.flush_images();

    let mut converted_tools = if tools.is_empty() {
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
                    cache_control: None,
                })
                .collect::<Vec<_>>(),
        )
    };
    if let Some(cache_control) = anthropic_cache_control(model, options) {
        apply_anthropic_cache_control(
            &mut all_messages,
            converted_tools.as_deref_mut(),
            &cache_control,
        );
    }

    if is_zai(model) {
        return zai_request_body(model, all_messages, converted_tools, options);
    }

    let reasoning_effort = super::requested_effort(options)
        .as_ref()
        .and_then(|effort| super::openai_reasoning_effort(model, effort));
    let off_effort = reasoning_effort
        .is_none()
        .then(|| super::openai_off_effort(model))
        .flatten()
        .filter(|_| accepts_reasoning_effort(model));
    let (off_effort, reasoning) = if is_openrouter(model) {
        (
            None,
            off_effort.map(|effort| serde_json::json!({ "effort": effort })),
        )
    } else {
        (off_effort, None)
    };

    RequestBody {
        model: model.id.to_string(),
        messages: all_messages,
        stream: true,
        max_completion_tokens: Some(super::effective_max_output_tokens(model, options)),
        max_tokens: None,
        temperature: if reasoning_effort.is_none() {
            options.temperature.map(|t| t.value())
        } else {
            None
        },
        tools: converted_tools,
        tool_stream: None,
        reasoning_effort: reasoning_effort.or(off_effort),
        reasoning,
        thinking: None,
        stream_options: StreamOpts {
            include_usage: true,
        },
    }
}

fn is_zai(model: &Model) -> bool {
    matches!(&model.provider, Provider::Custom(name) if name == "zai")
        || ["api.z.ai", "open.bigmodel.cn"]
            .iter()
            .any(|host| model.base_url.as_str().contains(host))
}

fn zai_request_body(
    model: &Model,
    messages: Vec<serde_json::Value>,
    tools: Option<Vec<RequestTool>>,
    options: &StreamOptions,
) -> RequestBody {
    let level = model.clamp_thinking(
        &super::requested_effort(options).map_or(ThinkingLevel::Off, |effort| {
            ThinkingLevel::from_effort_str(effort.name())
        }),
    );
    let thinking_on = model.reasoning && level != ThinkingLevel::Off;
    let thinking = model.reasoning.then(|| {
        if thinking_on {
            serde_json::json!({ "type": "enabled", "clear_thinking": false })
        } else {
            serde_json::json!({ "type": "disabled" })
        }
    });
    let effort_levels = model
        .supported_thinking_levels
        .iter()
        .filter(|level| **level != ThinkingLevel::Off)
        .count();
    let reasoning_effort = (thinking_on && effort_levels > 1)
        .then(|| match level {
            ThinkingLevel::Xhigh => Some("max".to_string()),
            other => other.effort_name().map(str::to_string),
        })
        .flatten();
    RequestBody {
        model: model.id.to_string(),
        messages,
        stream: true,
        max_completion_tokens: None,
        max_tokens: Some(super::effective_max_output_tokens(model, options)),
        temperature: if thinking_on {
            None
        } else {
            options.temperature.map(|t| t.value())
        },
        tool_stream: tools.is_some().then_some(true),
        tools,
        reasoning_effort,
        reasoning: None,
        thinking,
        stream_options: StreamOpts {
            include_usage: true,
        },
    }
}

fn is_openrouter(model: &Model) -> bool {
    model.provider == Provider::OpenRouter || model.base_url.contains("openrouter.ai")
}

fn requires_reasoning_content(model: &Model) -> bool {
    matches!(&model.provider, Provider::Custom(name) if name == "deepseek")
        || model.base_url.contains("deepseek.com")
}

fn accepts_reasoning_effort(model: &Model) -> bool {
    let base_url = model.base_url.as_str();
    ![
        "api.x.ai",
        "api.z.ai",
        "moonshot.",
        "cloudflare",
        "deepseek.com",
    ]
    .iter()
    .any(|host| base_url.contains(host))
}

fn anthropic_cache_control(model: &Model, options: &StreamOptions) -> Option<serde_json::Value> {
    if model.provider != Provider::OpenRouter || !model.id.as_str().starts_with("anthropic/") {
        return None;
    }
    match options.cache_retention.unwrap_or(CacheRetention::Short) {
        CacheRetention::None => None,
        CacheRetention::Short => Some(serde_json::json!({ "type": "ephemeral" })),
        CacheRetention::Long => Some(serde_json::json!({ "type": "ephemeral", "ttl": "1h" })),
    }
}

fn apply_anthropic_cache_control(
    messages: &mut [serde_json::Value],
    tools: Option<&mut [RequestTool]>,
    cache_control: &serde_json::Value,
) {
    if let Some(system) = messages
        .iter_mut()
        .find(|message| message["role"] == "system")
    {
        mark_last_text(system, cache_control);
    }
    if let Some(last) = tools.and_then(|tools| tools.last_mut()) {
        last.cache_control = Some(cache_control.clone());
    }
    for message in messages.iter_mut().rev() {
        let conversational = matches!(
            message["role"].as_str(),
            Some("user" | "assistant" | "tool")
        );
        if conversational && mark_last_text(message, cache_control) {
            break;
        }
    }
}

fn mark_last_text(message: &mut serde_json::Value, cache_control: &serde_json::Value) -> bool {
    match &mut message["content"] {
        serde_json::Value::String(text) if !text.is_empty() => {
            let text = std::mem::take(text);
            message["content"] = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": cache_control,
            }]);
            true
        }
        serde_json::Value::Array(parts) => {
            let Some(part) = parts.iter_mut().rev().find(|part| part["type"] == "text") else {
                return false;
            };
            part["cache_control"] = cache_control.clone();
            true
        }
        _ => false,
    }
}

struct OpenaiVisitor<'a> {
    model: &'a Model,
    trimming: ToolResultTrimming,
    out: &'a mut Vec<serde_json::Value>,
    pending_images: Vec<serde_json::Value>,
}

impl OpenaiVisitor<'_> {
    fn flush_images(&mut self) {
        if self.pending_images.is_empty() {
            return;
        }
        let mut content = vec![serde_json::json!({
            "type": "text",
            "text": "Attached image(s) from tool result:",
        })];
        content.append(&mut self.pending_images);
        self.out.push(serde_json::json!({
            "role": "user",
            "content": content,
        }));
    }

    fn tool_call_id(&self, id: &str) -> String {
        if let Some((call_id, _)) = id.split_once('|') {
            return call_id
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(MAX_TOOL_CALL_ID_CHARS)
                .collect();
        }
        if matches!(&self.model.provider, Provider::Custom(name) if name == "openai") {
            return id.chars().take(MAX_TOOL_CALL_ID_CHARS).collect();
        }
        id.to_string()
    }
}

const MAX_TOOL_CALL_ID_CHARS: usize = 40;

fn image_url_part(img: &ImageContent) -> serde_json::Value {
    serde_json::json!({
        "type": "image_url",
        "image_url": { "url": super::image_data_url(img) },
    })
}

impl super::MessageVisitor for OpenaiVisitor<'_> {
    fn on_user(&mut self, user: &UserMessage) {
        self.flush_images();
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
                        UserContentPart::Image(img) => Some(image_url_part(img)),
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
        self.flush_images();
        let mut msg_obj = serde_json::json!({ "role": "assistant" });

        let text = asst
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantContentPart::Text(t) if !t.text.is_empty() => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<String>();

        let mut replay_details = Vec::new();
        let mut traces = Vec::new();
        let mut trace_field = None;
        for part in &asst.content {
            let AssistantContentPart::Thinking(ThinkingContent::Thinking {
                thinking,
                signature,
            }) = part
            else {
                continue;
            };
            match signature.as_deref().and_then(decode_reasoning_details) {
                Some(details) => replay_details.extend(details),
                None if thinking.trim().is_empty() => {}
                None => {
                    if traces.is_empty() {
                        trace_field = signature.as_deref().and_then(ReasoningField::parse);
                    }
                    traces.push(thinking.as_str());
                }
            }
        }

        let requires_reasoning_content = requires_reasoning_content(self.model);
        if requires_reasoning_content {
            trace_field = Some(ReasoningField::ReasoningContent);
        }
        let reasoning = (!traces.is_empty()).then(|| traces.join("\n"));
        let (content, replayed) = match (reasoning, trace_field) {
            (Some(reasoning), Some(field)) => (text, Some((field, reasoning))),
            (Some(reasoning), None) if text.is_empty() => (reasoning, None),
            (Some(reasoning), None) => (format!("{reasoning}\n\n{text}"), None),
            (None, _) => (text, None),
        };
        msg_obj["content"] = if content.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(content)
        };
        if let Some((field, reasoning)) = replayed {
            msg_obj[field.as_str()] = serde_json::json!(reasoning);
        }

        let tool_calls: Vec<serde_json::Value> = asst
            .content
            .iter()
            .filter_map(|p| match p {
                AssistantContentPart::ToolCall(tc) => Some(serde_json::json!({
                    "id": self.tool_call_id(tc.id.as_str()),
                    "type": "function",
                    "function": {
                        "name": tc.name.as_str(),
                        "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default(),
                    }
                })),
                _ => None,
            })
            .collect();

        if requires_reasoning_content
            && self.model.reasoning
            && msg_obj.get("reasoning_content").is_none()
        {
            msg_obj["reasoning_content"] = serde_json::json!("");
        }
        if !replay_details.is_empty() {
            if msg_obj["content"].is_null() && tool_calls.is_empty() {
                msg_obj["content"] = serde_json::json!("");
            }
            msg_obj["reasoning_details"] = serde_json::Value::Array(replay_details);
        }
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

        let has_images = tr
            .content
            .iter()
            .any(|part| matches!(part, ToolResultContentPart::Image(_)));
        let text = if raw_text.is_empty() && has_images {
            std::borrow::Cow::Borrowed("(see attached image)")
        } else {
            super::maybe_trim_tool_output(&raw_text, is_old_turn, self.trimming)
        };

        self.out.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": self.tool_call_id(tr.tool_call_id.as_str()),
            "content": text,
        }));

        if has_images && self.model.input.contains(&InputModality::Image) {
            self.pending_images
                .extend(tr.content.iter().filter_map(|part| match part {
                    ToolResultContentPart::Image(img) => Some(image_url_part(img)),
                    ToolResultContentPart::Text(_) => None,
                }));
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChunkResponse {
    #[serde(default)]
    id: Option<String>,
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
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_text: Option<String>,
    #[serde(default)]
    reasoning_details: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningField {
    ReasoningContent,
    Reasoning,
    ReasoningText,
}

impl ReasoningField {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
            Self::ReasoningText => "reasoning_text",
        }
    }

    fn parse(field: &str) -> Option<Self> {
        match field {
            "reasoning_content" => Some(Self::ReasoningContent),
            "reasoning" => Some(Self::Reasoning),
            "reasoning_text" => Some(Self::ReasoningText),
            _ => None,
        }
    }
}

impl ChunkDelta {
    fn reasoning_field(&self) -> Option<(ReasoningField, &str)> {
        [
            (ReasoningField::ReasoningContent, &self.reasoning_content),
            (ReasoningField::Reasoning, &self.reasoning),
            (ReasoningField::ReasoningText, &self.reasoning_text),
        ]
        .into_iter()
        .find_map(|(field, text)| {
            text.as_deref()
                .filter(|text| !text.is_empty())
                .map(|text| (field, text))
        })
    }

    fn details_text(&self) -> Option<String> {
        let text = self
            .reasoning_details
            .iter()
            .flatten()
            .filter(|detail| {
                detail["type"]
                    .as_str()
                    .is_none_or(|ty| ty == "reasoning.text")
            })
            .filter_map(|detail| detail["text"].as_str())
            .collect::<String>();
        (!text.is_empty()).then_some(text)
    }
}

const REASONING_DETAILS_SIGNATURE_TYPE: &str = "openai-completions.reasoning_details.v1";

#[derive(Debug, Default)]
struct ReasoningDetails {
    by_index: std::collections::BTreeMap<u64, serde_json::Map<String, serde_json::Value>>,
    next_index: u64,
    content_index: Option<usize>,
}

impl ReasoningDetails {
    fn merge(&mut self, detail: serde_json::Map<String, serde_json::Value>) {
        let index = detail
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(self.next_index);
        self.next_index = self.next_index.max(index + 1);
        let merged = self.by_index.entry(index).or_default();
        for (key, value) in detail {
            let fragment = matches!(key.as_str(), "text" | "summary");
            match (merged.get_mut(&key), value) {
                (Some(serde_json::Value::String(prev)), serde_json::Value::String(next))
                    if fragment =>
                {
                    prev.push_str(&next);
                }
                (_, value) => {
                    merged.insert(key, value);
                }
            }
        }
    }

    fn encode(&self) -> Option<String> {
        if self.by_index.is_empty() {
            return None;
        }
        let details: Vec<_> = self.by_index.values().collect();
        Some(
            serde_json::json!({
                "type": REASONING_DETAILS_SIGNATURE_TYPE,
                "details": details,
            })
            .to_string(),
        )
    }
}

fn decode_reasoning_details(signature: &str) -> Option<Vec<serde_json::Value>> {
    if !signature.starts_with('{') {
        return None;
    }
    let mut parsed: serde_json::Value = serde_json::from_str(signature).ok()?;
    if parsed["type"] != REASONING_DETAILS_SIGNATURE_TYPE {
        return None;
    }
    let serde_json::Value::Array(details) = parsed["details"].take() else {
        return None;
    };
    details
        .iter()
        .all(serde_json::Value::is_object)
        .then_some(details)
}

#[derive(Debug, Deserialize)]
struct ChunkToolCall {
    #[serde(default)]
    index: Option<u64>,
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

#[derive(Debug)]
enum OpenBlock {
    Text(String),
    Thinking(String),
}

#[derive(Debug)]
struct ToolBlock {
    openai_index: u64,
    content_index: usize,
    args_buf: String,
}

#[derive(Default)]
struct OpenAiCompletionsProcessor {
    current: Option<OpenBlock>,
    tool_blocks: Vec<ToolBlock>,
    reasoning_details: ReasoningDetails,
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
                let events = self.process_chunk(chunk, output);
                super::sse::ProcessResult::Events(events)
            }
            Err(_) => super::sse::ProcessResult::Skip,
        }
    }

    fn finish(&mut self, output: &mut AssistantMessage) {
        self.current = None;
        finish_tool_blocks_silent(&mut self.tool_blocks, output);
    }

    fn label(&self) -> &'static str {
        "openai completions"
    }
}

impl OpenAiCompletionsProcessor {
    fn process_chunk(
        &mut self,
        chunk: ChunkResponse,
        output: &mut AssistantMessage,
    ) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        if output.generation_id.is_none()
            && let Some(id) = chunk.id
        {
            output.generation_id = Some(id);
        }

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
            let (stop_reason, message) = map_stop_reason(&reason);
            output.stop_reason = stop_reason;
            if let Some(message) = message {
                tracing::warn!(finish_reason = %reason, %message, "openai ended the turn");
                output.error_message = Some(message);
            }
        }

        let field = choice.delta.reasoning_field();
        if let Some(reasoning) = field
            .map(|(_, text)| text.to_owned())
            .or_else(|| choice.delta.details_text())
        {
            let signature = field.map(|(field, _)| field.as_str().to_owned());
            self.push_reasoning(reasoning, signature, output, &mut events);
        }

        if let Some(details) = choice.delta.reasoning_details {
            self.record_reasoning_details(details, output, &mut events);
        }

        if let Some(text) = choice.delta.content
            && !text.is_empty()
        {
            self.push_text(text, output, &mut events);
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            finish_block_events(&mut self.current, output, &mut events);
            for tc in tool_calls {
                self.push_tool_call(tc, output, &mut events);
            }
        }

        events
    }

    fn push_text(
        &mut self,
        text: String,
        output: &mut AssistantMessage,
        events: &mut Vec<StreamEvent>,
    ) {
        if let Some(OpenBlock::Text(buf)) = &mut self.current {
            buf.push_str(&text);
            let idx = output.content.len().saturating_sub(1);
            if let Some(AssistantContentPart::Text(tc)) = output.content.get_mut(idx) {
                tc.text.push_str(&text);
            }
            events.push(StreamEvent::TextDelta {
                content_index: idx,
                delta: text,
            });
            return;
        }
        finish_block_events(&mut self.current, output, events);
        let content_index = output.content.len();
        self.current = Some(OpenBlock::Text(text.clone()));
        output.content.push(AssistantContentPart::Text(TextContent {
            text: text.clone(),
        }));
        events.push(StreamEvent::TextStart { content_index });
        events.push(StreamEvent::TextDelta {
            content_index,
            delta: text,
        });
    }

    fn push_tool_call(
        &mut self,
        tc: ChunkToolCall,
        output: &mut AssistantMessage,
        events: &mut Vec<StreamEvent>,
    ) {
        let openai_idx = tc.index.unwrap_or(0);
        let block_pos = match self
            .tool_blocks
            .iter()
            .position(|b| b.openai_index == openai_idx)
        {
            Some(pos) => pos,
            None => {
                let id = tc.id.clone().unwrap_or_default();
                let name = tc
                    .function
                    .as_ref()
                    .and_then(|f| f.name.clone())
                    .unwrap_or_default();
                let content_index = output.content.len();
                output
                    .content
                    .push(AssistantContentPart::ToolCall(ToolCall {
                        id: ToolCallId::from(id),
                        name: ToolName::from(name),
                        arguments: serde_json::Value::Object(Default::default()),
                    }));
                events.push(StreamEvent::ToolCallStart { content_index });
                self.tool_blocks.push(ToolBlock {
                    openai_index: openai_idx,
                    content_index,
                    args_buf: String::new(),
                });
                self.tool_blocks.len() - 1
            }
        };

        if let Some(func) = tc.function
            && let Some(args) = func.arguments
        {
            let block = &mut self.tool_blocks[block_pos];
            block.args_buf.push_str(&args);
            events.push(StreamEvent::ToolCallDelta {
                content_index: block.content_index,
                delta: args,
            });
        }
    }

    fn push_reasoning(
        &mut self,
        reasoning: String,
        signature: Option<String>,
        output: &mut AssistantMessage,
        events: &mut Vec<StreamEvent>,
    ) {
        if let Some(OpenBlock::Thinking(text)) = &mut self.current {
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
            return;
        }
        let content_index = self.start_thinking(reasoning.clone(), signature, output, events);
        events.push(StreamEvent::ThinkingDelta {
            content_index,
            delta: reasoning,
        });
    }

    fn start_thinking(
        &mut self,
        thinking: String,
        signature: Option<String>,
        output: &mut AssistantMessage,
        events: &mut Vec<StreamEvent>,
    ) -> usize {
        finish_block_events(&mut self.current, output, events);
        let content_index = output.content.len();
        self.current = Some(OpenBlock::Thinking(thinking.clone()));
        output
            .content
            .push(AssistantContentPart::Thinking(ThinkingContent::Thinking {
                thinking,
                signature,
            }));
        events.push(StreamEvent::ThinkingStart { content_index });
        content_index
    }

    fn record_reasoning_details(
        &mut self,
        details: Vec<serde_json::Value>,
        output: &mut AssistantMessage,
        events: &mut Vec<StreamEvent>,
    ) {
        for detail in details {
            if let serde_json::Value::Object(detail) = detail {
                self.reasoning_details.merge(detail);
            }
        }
        let Some(signature) = self.reasoning_details.encode() else {
            return;
        };
        let content_index = match (self.reasoning_details.content_index, &self.current) {
            (Some(idx), _) => idx,
            (None, Some(OpenBlock::Thinking(_))) => output.content.len().saturating_sub(1),
            (None, _) => self.start_thinking(String::new(), None, output, events),
        };
        self.reasoning_details.content_index = Some(content_index);
        if let Some(AssistantContentPart::Thinking(tc)) = output.content.get_mut(content_index)
            && let Some(slot) = tc.signature_mut()
        {
            *slot = Some(signature);
        }
    }
}

fn finish_block_events(
    current: &mut Option<OpenBlock>,
    output: &AssistantMessage,
    events: &mut Vec<StreamEvent>,
) {
    let Some(block) = current.take() else {
        return;
    };
    let content_index = output.content.len().saturating_sub(1);
    events.push(match block {
        OpenBlock::Text(text) => StreamEvent::TextEnd {
            content_index,
            text,
        },
        OpenBlock::Thinking(thinking) => StreamEvent::ThinkingEnd {
            content_index,
            thinking,
        },
    });
}

fn finish_tool_blocks_silent(tool_blocks: &mut Vec<ToolBlock>, output: &mut AssistantMessage) {
    for block in tool_blocks.drain(..) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&block.args_buf)
            && let Some(AssistantContentPart::ToolCall(tc)) =
                output.content.get_mut(block.content_index)
        {
            tc.arguments = parsed;
        }
    }
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "tool_calls" | "function_call" => (StopReason::ToolUse, None),
        "content_filter" | "sensitive" => (
            StopReason::Error,
            Some("the provider's content filter stopped the response".into()),
        ),
        "network_error" => (
            StopReason::Error,
            Some("the provider hit a network error mid-response, retry".into()),
        ),
        "model_context_window_exceeded" => (
            StopReason::Error,
            Some("the response filled the model's context window. compact and retry".into()),
        ),
        _ => (StopReason::Stop, None),
    }
}

#[doc(hidden)]
pub fn benchmark_tool_call_deltas(chunk_count: usize, arg_bytes: usize) -> usize {
    let (_, fragments) = super::bench_support::tool_call_json_fragments(chunk_count, arg_bytes);
    let mut output = AssistantMessage::builder()
        .model("bench".into())
        .provider(Provider::Custom("bench".into()))
        .api(Api::OpenaiCompletions)
        .timestamp_ms(Timestamp::zero())
        .build();
    let mut state = OpenAiCompletionsProcessor::default();

    for (index, fragment) in fragments.into_iter().enumerate() {
        let chunk = ChunkResponse {
            id: None,
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    tool_calls: Some(vec![ChunkToolCall {
                        index: Some(0),
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
        let _ = state.process_chunk(chunk, &mut output);
    }

    finish_tool_blocks_silent(&mut state.tool_blocks, &mut output);

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
        assert_eq!(map_stop_reason("stop"), (StopReason::Stop, None));
        assert_eq!(map_stop_reason("length"), (StopReason::Length, None));
        assert_eq!(map_stop_reason("tool_calls"), (StopReason::ToolUse, None));
        assert_eq!(map_stop_reason("unknown"), (StopReason::Stop, None));
    }

    #[test]
    fn a_content_filter_stop_says_so() {
        let (reason, message) = map_stop_reason("content_filter");
        assert_eq!(reason, StopReason::Error);
        assert!(message.unwrap().contains("content filter"));
    }

    #[test]
    fn build_body_basic() {
        let model = test_model();
        let options = StreamOptions::default();

        let body = build_request_body(
            &model,
            &Some("you are helpful".into()),
            &[Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("hi".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            )],
            &[],
            &options,
        );

        assert_eq!(body.model, model.id.as_str());
        assert!(body.stream);
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
    fn a_reasoning_model_with_thinking_off_asks_for_no_reasoning() {
        let openrouter = Model {
            reasoning: true,
            ..test_model()
        };
        let body = build_request_body(&openrouter, &None, &[], &[], &StreamOptions::default());
        assert_eq!(body.reasoning, Some(serde_json::json!({"effort": "none"})));
        assert!(body.reasoning_effort.is_none());

        let direct = Model {
            provider: Provider::Custom("openai".into()),
            base_url: "https://api.openai.com/v1".into(),
            ..openrouter.clone()
        };
        let off = StreamOptions {
            thinking: Some(ThinkingLevel::Off),
            ..Default::default()
        };
        let body = build_request_body(&direct, &None, &[], &[], &off);
        assert_eq!(body.reasoning_effort.as_deref(), Some("none"));
        assert!(body.reasoning.is_none());

        let grok = Model {
            base_url: "https://api.x.ai/v1".into(),
            ..direct
        };
        let body = build_request_body(&grok, &None, &[], &[], &off);
        assert!(body.reasoning_effort.is_none() && body.reasoning.is_none());
    }

    fn zai_model(id: &str) -> Model {
        crate::models::zai_models()
            .into_iter()
            .find(|m| m.id.as_str() == id)
            .unwrap()
    }

    fn zai_body(id: &str, thinking: ThinkingLevel, tools: &[ToolDefinition]) -> serde_json::Value {
        let options = StreamOptions {
            thinking: Some(thinking),
            temperature: Some(Temperature::new(0.5)),
            ..Default::default()
        };
        serde_json::to_value(build_request_body(
            &zai_model(id),
            &None,
            &[],
            tools,
            &options,
        ))
        .unwrap()
    }

    #[test]
    fn zai_sends_its_own_thinking_switch_and_output_limit() {
        let body = zai_body("glm-5.2", ThinkingLevel::High, &[]);
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "enabled", "clear_thinking": false})
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body["max_tokens"].is_u64());
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("tool_stream").is_none());

        let off = zai_body("glm-4.7", ThinkingLevel::Off, &[]);
        assert_eq!(off["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(off.get("reasoning_effort").is_none());
        assert!(off["temperature"].is_number());
    }

    #[test]
    fn zai_effort_maps_xhigh_to_max_and_toggle_models_get_no_effort() {
        assert_eq!(
            zai_body("glm-5.3", ThinkingLevel::Xhigh, &[])["reasoning_effort"],
            "max"
        );
        assert_eq!(
            zai_body("glm-5.3", ThinkingLevel::Medium, &[])["reasoning_effort"],
            "high"
        );
        let always_on = zai_body("glm-5.3", ThinkingLevel::Off, &[]);
        assert_eq!(always_on["thinking"]["type"], "enabled");
        assert_eq!(always_on["reasoning_effort"], "low");

        let toggle = zai_body("glm-4.7", ThinkingLevel::High, &[]);
        assert_eq!(toggle["thinking"]["type"], "enabled");
        assert!(toggle.get("reasoning_effort").is_none());
    }

    #[test]
    fn zai_streams_tool_call_arguments_when_tools_are_offered() {
        let tool = ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let body = zai_body("glm-5.3", ThinkingLevel::High, &[tool]);
        assert_eq!(body["tool_stream"], true);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn zai_finish_reasons_surface_as_errors() {
        for reason in [
            "network_error",
            "sensitive",
            "model_context_window_exceeded",
        ] {
            let (stop, message) = map_stop_reason(reason);
            assert_eq!(stop, StopReason::Error, "{reason}");
            assert!(message.is_some(), "{reason}");
        }
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
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::OpenaiCompletions)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_1".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "file contents here".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];

        let body = build_request_body(&model, &None, &messages, &[], &StreamOptions::default());
        assert_eq!(body.messages.len(), 2);
        assert_eq!(body.messages[0]["role"], "assistant");
        assert!(body.messages[0]["tool_calls"].is_array());
        assert_eq!(body.messages[1]["role"], "tool");
        assert_eq!(body.messages[1]["tool_call_id"], "tc_1");
    }

    fn user_turn(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn old_tool_turn(output: &str) -> Vec<Message> {
        let mut messages = vec![
            user_turn("run it"),
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::OpenaiCompletions)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: output.into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];
        messages.extend((0..4).map(|i| user_turn(&format!("turn {i}"))));
        messages
    }

    #[test]
    fn openrouter_claude_gets_cache_breakpoints_on_system_last_tool_and_last_message() {
        let tools: Vec<ToolDefinition> = ["read", "edit"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: name.into(),
                parameters: serde_json::json!({"type": "object"}),
            })
            .collect();
        let messages = [user_turn("first"), user_turn("second")];
        let body = build_request_body(
            &test_model(),
            &Some("be helpful".into()),
            &messages,
            &tools,
            &StreamOptions::default(),
        );
        let ephemeral = serde_json::json!({"type": "ephemeral"});
        assert_eq!(body.messages[0]["content"][0]["cache_control"], ephemeral);
        assert_eq!(body.messages[1]["content"], "first");
        assert_eq!(body.messages[2]["content"][0]["text"], "second");
        assert_eq!(body.messages[2]["content"][0]["cache_control"], ephemeral);
        let tools = body.tools.unwrap();
        assert!(tools[0].cache_control.is_none());
        assert_eq!(tools[1].cache_control.as_ref(), Some(&ephemeral));

        let long = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..StreamOptions::default()
        };
        let body = build_request_body(&test_model(), &None, &messages, &[], &long);
        assert_eq!(body.messages[1]["content"][0]["cache_control"]["ttl"], "1h");

        let other = Model {
            id: "openai/gpt-5".into(),
            ..test_model()
        };
        let body = build_request_body(&other, &None, &messages, &[], &StreamOptions::default());
        assert_eq!(body.messages[1]["content"], "second");
    }

    #[test]
    fn tool_result_images_follow_as_a_user_message_and_ids_are_normalised() {
        let long_id = format!("call_abc|{}", "x+/=".repeat(100));
        let mut messages = old_tool_turn("");
        messages.truncate(3);
        let Message::Assistant(assistant) = &mut messages[1] else {
            panic!()
        };
        let AssistantContentPart::ToolCall(call) = &mut assistant.content[0] else {
            panic!()
        };
        call.id = long_id.as_str().into();
        let Message::ToolResult(result) = &mut messages[2] else {
            panic!()
        };
        result.tool_call_id = long_id.as_str().into();
        result.content = vec![ToolResultContentPart::Image(ImageContent {
            mime_type: ImageMimeType::Png,
            data: "aW1n".into(),
        })];
        messages.push(user_turn("what is it"));

        let options = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..StreamOptions::default()
        };
        let body = build_request_body(&test_model(), &None, &messages, &[], &options);
        assert_eq!(body.messages[1]["tool_calls"][0]["id"], "call_abc");
        assert_eq!(body.messages[2]["tool_call_id"], "call_abc");
        assert_eq!(body.messages[2]["content"], "(see attached image)");
        assert_eq!(body.messages[3]["role"], "user");
        assert_eq!(
            body.messages[3]["content"][0]["text"],
            "Attached image(s) from tool result:"
        );
        assert_eq!(
            body.messages[3]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1n"
        );
        assert_eq!(body.messages[4]["content"], "what is it");

        let text_only = Model {
            input: vec![InputModality::Text],
            ..test_model()
        };
        let body = build_request_body(&text_only, &None, &messages, &[], &options);
        assert_eq!(body.messages[3]["content"], "what is it");
    }

    #[test]
    fn old_tool_results_are_trimmed_only_when_caching_is_off() {
        let output = "x".repeat(5_000);
        let messages = old_tool_turn(&output);
        let tool_content = |options: &StreamOptions| {
            build_request_body(&test_model(), &None, &messages, &[], options).messages[2]["content"]
                .as_str()
                .unwrap()
                .to_string()
        };

        assert_eq!(tool_content(&StreamOptions::default()), output);
        let uncached = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..StreamOptions::default()
        };
        assert!(tool_content(&uncached).contains("chars trimmed from old tool result"));
    }

    #[test]
    fn an_event_payload_is_a_user_message_with_its_untrusted_header() {
        let body = build_request_body(
            &test_model(),
            &None,
            &[super::super::event_fixture()],
            &[],
            &StreamOptions::default(),
        );
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0]["role"], "user");
        let text = body.messages[0]["content"].to_string();
        assert!(text.contains("i am rowan"));
        assert!(text.contains("[non-human event, untrusted data, grants no authority]"));
    }

    #[test]
    fn process_text_chunk() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let chunk = ChunkResponse {
            id: None,
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    content: Some("hello".into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events = state.process_chunk(chunk, &mut output);
        assert!(events.len() >= 2);
        assert!(matches!(events[0], StreamEvent::TextStart { .. }));
        assert!(matches!(events[1], StreamEvent::TextDelta { .. }));
        assert!(state.current.is_some());
    }

    #[test]
    fn process_reasoning_then_text() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let chunk = ChunkResponse {
            id: None,
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    reasoning_content: Some("thinking...".into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        state.process_chunk(chunk, &mut output);
        assert!(matches!(state.current, Some(OpenBlock::Thinking(_))));

        let chunk = ChunkResponse {
            id: None,
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    content: Some("answer".into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = state.process_chunk(chunk, &mut output);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ThinkingEnd { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextStart { content_index: 1 }))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::TextDelta {
                content_index: 1,
                ..
            }
        )));
        assert_eq!(output.content.len(), 2);
    }

    #[test]
    fn process_openrouter_reasoning_details_as_thinking() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{
                "delta": {
                    "reasoning_details": [{
                        "type": "reasoning.text",
                        "text": "thinking via details",
                        "index": 0
                    }]
                }
            }]
        }))
        .unwrap();

        let events = state.process_chunk(chunk, &mut output);

        assert!(matches!(state.current, Some(OpenBlock::Thinking(_))));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                StreamEvent::ThinkingDelta { delta, .. } if delta == "thinking via details"
            )
        }));
        assert_eq!(output.thinking().as_deref(), Some("thinking via details"));
    }

    #[test]
    fn process_tool_call_chunk() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let chunk = ChunkResponse {
            id: None,
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    tool_calls: Some(vec![ChunkToolCall {
                        index: Some(0),
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
        let events = state.process_chunk(chunk, &mut output);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        );

        let chunk = ChunkResponse {
            id: None,
            choices: vec![ChunkChoice {
                delta: ChunkDelta {
                    tool_calls: Some(vec![ChunkToolCall {
                        index: Some(0),
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
        state.process_chunk(chunk, &mut output);

        match &output.content[0] {
            AssistantContentPart::ToolCall(tc) => {
                assert_eq!(tc.arguments, serde_json::json!({}));
            }
            other => panic!("expected tool call, got {other:?}"),
        }

        finish_tool_blocks_silent(&mut state.tool_blocks, &mut output);

        match &output.content[0] {
            AssistantContentPart::ToolCall(tc) => {
                assert_eq!(tc.name.as_str(), "read");
                assert_eq!(tc.arguments["path"], "foo.rs");
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn parallel_tool_calls_with_interleaved_deltas_keep_args_separate() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_a", "function": {"name": "WebSearch", "arguments": ""}}
            ]}}]
        }))
        .unwrap();
        state.process_chunk(chunk, &mut output);

        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 1, "id": "call_b", "function": {"name": "WebSearch", "arguments": ""}}
            ]}}]
        }))
        .unwrap();
        state.process_chunk(chunk, &mut output);

        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"q\":\"foo\"}"}}
            ]}}]
        }))
        .unwrap();
        state.process_chunk(chunk, &mut output);

        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "{\"q\":\"bar\"}"}}
            ]}}]
        }))
        .unwrap();
        state.process_chunk(chunk, &mut output);

        finish_tool_blocks_silent(&mut state.tool_blocks, &mut output);

        assert_eq!(
            output.content.len(),
            2,
            "both parallel tool calls should land as separate ToolCall parts"
        );

        let tool_a = match &output.content[0] {
            AssistantContentPart::ToolCall(tc) => tc,
            other => panic!("expected ToolCall at index 0, got {other:?}"),
        };
        assert_eq!(tool_a.id.as_str(), "call_a");
        assert_eq!(
            tool_a.arguments["q"], "foo",
            "tool 0 args should reflect its own delta, got {:?}",
            tool_a.arguments
        );

        let tool_b = match &output.content[1] {
            AssistantContentPart::ToolCall(tc) => tc,
            other => panic!("expected ToolCall at index 1, got {other:?}"),
        };
        assert_eq!(tool_b.id.as_str(), "call_b");
        assert_eq!(
            tool_b.arguments["q"], "bar",
            "tool 1 args should reflect its own delta, got {:?}",
            tool_b.arguments
        );
    }

    #[test]
    fn process_chunk_captures_top_level_id_for_generation_lookup() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "id": "gen-abc123",
            "choices": [{"delta": {"content": "hi"}}]
        }))
        .expect("chunk parses");
        state.process_chunk(chunk, &mut output);
        assert_eq!(output.generation_id.as_deref(), Some("gen-abc123"));
    }

    #[test]
    fn process_chunk_id_is_sticky_across_chunks() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let with_id: ChunkResponse = serde_json::from_value(serde_json::json!({
            "id": "gen-keep-me",
            "choices": [{"delta": {"content": "a"}}]
        }))
        .unwrap();
        state.process_chunk(with_id, &mut output);
        let without_id: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"content": "b"}}]
        }))
        .unwrap();
        state.process_chunk(without_id, &mut output);
        assert_eq!(output.generation_id.as_deref(), Some("gen-keep-me"));
    }

    #[test]
    fn process_usage_chunk() {
        let mut output = test_output();
        let mut state = OpenAiCompletionsProcessor::default();

        let chunk = ChunkResponse {
            id: None,
            choices: vec![],
            usage: Some(ChunkUsage {
                prompt_tokens: 150,
                completion_tokens: 50,
                prompt_tokens_details: Some(PromptTokenDetails { cached_tokens: 100 }),
            }),
        };

        state.process_chunk(chunk, &mut output);
        assert_eq!(output.usage.input_tokens, TokenCount::new(50));
        assert_eq!(output.usage.output_tokens, TokenCount::new(50));
        assert_eq!(output.usage.cache_read_tokens, TokenCount::new(100));
    }

    #[test]
    fn benchmark_tool_call_deltas_returns_payload_size() {
        assert_eq!(benchmark_tool_call_deltas(8, 1024), 1024);
    }

    fn reasoning_model(provider: &str, base_url: &str) -> Model {
        Model {
            id: "reasoner".into(),
            provider: Provider::Custom(provider.into()),
            base_url: base_url.into(),
            reasoning: true,
            ..test_model()
        }
    }

    fn streamed(model: &Model, chunks: &[serde_json::Value]) -> AssistantMessage {
        let mut output = AssistantMessage::builder()
            .model(model.id.clone())
            .provider(model.provider.clone())
            .api(model.api)
            .timestamp_ms(Timestamp::zero())
            .build();
        let mut state = OpenAiCompletionsProcessor::default();
        for chunk in chunks {
            let chunk = serde_json::from_value(serde_json::json!({
                "choices": [{ "delta": chunk }]
            }))
            .unwrap();
            state.process_chunk(chunk, &mut output);
        }
        super::super::sse::SseProcessor::finish(&mut state, &mut output);
        output
    }

    fn replayed(model: &Model, history: Vec<Message>) -> serde_json::Value {
        let mut messages = vec![user_turn("go")];
        messages.extend(history);
        let options = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..StreamOptions::default()
        };
        build_request_body(model, &None, &messages, &[], &options).messages[1].clone()
    }

    const REASONING_KEYS: [&str; 4] = [
        "reasoning_content",
        "reasoning",
        "reasoning_text",
        "reasoning_details",
    ];

    #[test]
    fn reasoning_replays_in_the_field_it_streamed_in() {
        let model = reasoning_model("local", "http://localhost:8080/v1");
        for field in ["reasoning_content", "reasoning", "reasoning_text"] {
            let output = streamed(
                &model,
                &[
                    serde_json::json!({ field: "let me " }),
                    serde_json::json!({ field: "think" }),
                    serde_json::json!({ "content": "answer" }),
                ],
            );
            assert_eq!(
                output.content[0],
                AssistantContentPart::Thinking(ThinkingContent::Thinking {
                    thinking: "let me think".into(),
                    signature: Some(field.into()),
                })
            );
            let message = replayed(&model, vec![Message::Assistant(output)]);
            assert_eq!(message["content"], "answer");
            assert_eq!(message[field], "let me think");
            for other in REASONING_KEYS.iter().filter(|key| **key != field) {
                assert!(message.get(other).is_none(), "{other} in {message}");
            }
        }
    }

    #[test]
    fn the_first_non_empty_reasoning_field_wins() {
        let model = reasoning_model("chutes", "https://llm.chutes.ai/v1");
        let output = streamed(
            &model,
            &[serde_json::json!({
                "reasoning_content": "",
                "reasoning": "once",
                "reasoning_text": "once",
            })],
        );
        assert_eq!(output.thinking().as_deref(), Some("once"));
        assert_eq!(
            replayed(&model, vec![Message::Assistant(output)])["reasoning"],
            "once"
        );
    }

    #[test]
    fn openrouter_reasoning_details_are_merged_and_replayed_whole() {
        let model = test_model();
        let output = streamed(
            &model,
            &[
                serde_json::json!({
                    "reasoning": "think ",
                    "reasoning_details": [{
                        "type": "reasoning.text", "text": "think ", "format": "anthropic", "index": 0
                    }],
                }),
                serde_json::json!({
                    "reasoning": "more",
                    "reasoning_details": [{ "type": "reasoning.text", "text": "more", "index": 0 }],
                }),
                serde_json::json!({ "content": "hi" }),
                serde_json::json!({
                    "reasoning_details": [{ "type": "reasoning.encrypted", "data": "enc", "index": 1 }],
                }),
            ],
        );
        assert_eq!(output.content.len(), 2);
        assert_eq!(output.thinking().as_deref(), Some("think more"));
        let message = replayed(&model, vec![Message::Assistant(output)]);
        assert_eq!(message["content"], "hi");
        assert_eq!(
            message["reasoning_details"],
            serde_json::json!([
                { "type": "reasoning.text", "text": "think more", "format": "anthropic", "index": 0 },
                { "type": "reasoning.encrypted", "data": "enc", "index": 1 },
            ])
        );
        assert!(message.get("reasoning").is_none());
    }

    #[test]
    fn details_arriving_after_text_get_their_own_thinking_block() {
        let model = test_model();
        let output = streamed(
            &model,
            &[
                serde_json::json!({ "content": "hi" }),
                serde_json::json!({
                    "reasoning_details": [{ "type": "reasoning.encrypted", "data": "enc" }],
                }),
            ],
        );
        assert!(matches!(
            &output.content[1],
            AssistantContentPart::Thinking(ThinkingContent::Thinking { thinking, signature: Some(_) })
                if thinking.is_empty()
        ));
        let message = replayed(&model, vec![Message::Assistant(output)]);
        assert_eq!(message["content"], "hi");
        assert_eq!(message["reasoning_details"][0]["data"], "enc");
    }

    #[test]
    fn deepseek_always_gets_reasoning_content() {
        let model = reasoning_model("deepseek", "https://api.deepseek.com");
        let output = streamed(
            &model,
            &[
                serde_json::json!({ "reasoning": "hmm" }),
                serde_json::json!({ "content": "ok" }),
            ],
        );
        let message = replayed(&model, vec![Message::Assistant(output)]);
        assert_eq!(message["reasoning_content"], "hmm");
        assert!(message.get("reasoning").is_none());

        let plain = streamed(&model, &[serde_json::json!({ "content": "ok" })]);
        assert_eq!(
            replayed(&model, vec![Message::Assistant(plain)])["reasoning_content"],
            ""
        );
    }

    #[test]
    fn unsigned_reasoning_stays_visible_as_text() {
        let model = reasoning_model("local", "http://localhost:8080/v1");
        let mut output = streamed(
            &model,
            &[
                serde_json::json!({ "reasoning": "trace" }),
                serde_json::json!({ "content": "answer" }),
            ],
        );
        if let AssistantContentPart::Thinking(thinking) = &mut output.content[0] {
            *thinking.signature_mut().unwrap() = None;
        }
        let message = replayed(&model, vec![Message::Assistant(output)]);
        assert_eq!(message["content"], "trace\n\nanswer");
        for key in REASONING_KEYS {
            assert!(message.get(key).is_none(), "{key} in {message}");
        }
    }

    #[test]
    fn foreign_thinking_never_sends_its_signature() {
        let model = reasoning_model("local", "http://localhost:8080/v1");
        let anthropic = AssistantMessage::builder()
            .content(vec![
                AssistantContentPart::Thinking(ThinkingContent::Thinking {
                    thinking: "private chain".into(),
                    signature: Some("sig_anthropic".into()),
                }),
                AssistantContentPart::Thinking(ThinkingContent::Redacted {
                    data: "opaque".into(),
                }),
                AssistantContentPart::Text(TextContent {
                    text: "answer".into(),
                }),
            ])
            .model("claude-sonnet-4-20250514".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();
        let other_completions = streamed(
            &Model {
                id: "other".into(),
                ..model.clone()
            },
            &[
                serde_json::json!({
                    "reasoning_content": "private chain",
                    "reasoning_details": [{ "type": "reasoning.encrypted", "data": "opaque" }],
                }),
                serde_json::json!({ "content": "answer" }),
            ],
        );
        for foreign in [anthropic, other_completions] {
            let message = replayed(&model, vec![Message::Assistant(foreign)]);
            let wire = message.to_string();
            assert!(!wire.contains("sig_anthropic"), "{wire}");
            assert!(!wire.contains("opaque"), "{wire}");
            for key in REASONING_KEYS {
                assert!(message.get(key).is_none(), "{key} in {wire}");
            }
            assert_eq!(message["content"], "private chainanswer");
        }
    }

    fn test_model() -> Model {
        Model::builder()
            .id("anthropic/claude-sonnet-4".into())
            .name("Claude Sonnet 4 (OpenRouter)".into())
            .api(Api::OpenaiCompletions)
            .provider(Provider::OpenRouter)
            .base_url("https://openrouter.ai/api/v1".into())
            .context_window(TokenCount::new(200_000))
            .max_output_tokens(TokenCount::new(16384))
            .input(vec![InputModality::Text, InputModality::Image])
            .cost(ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            })
            .build()
    }

    fn test_output() -> AssistantMessage {
        AssistantMessage::builder()
            .model("test".into())
            .provider(Provider::Custom("test".into()))
            .api(Api::OpenaiCompletions)
            .timestamp_ms(Timestamp::zero())
            .build()
    }
}
