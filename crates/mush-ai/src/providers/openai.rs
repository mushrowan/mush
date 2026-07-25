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
        let call_id = crate::wire_log::next_call_id();

        tracing::debug!(model = %model.id, %url, "sending openai completions request");

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .json(&body)
            .with_extension(crate::http::WireCallId(call_id))
            .send()
            .await?;

        let response =
            super::check_response(response, "openai completions", model.id.as_str(), &url).await?;

        Ok(parse_sse_stream(
            response,
            model.id.clone(),
            model.provider.clone(),
            model.api,
            call_id,
        ))
    }
}

// request body
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

    let reasoning_effort = super::requested_effort(options)
        .as_ref()
        .and_then(|effort| super::openai_reasoning_effort(model, effort));

    RequestBody {
        model: model.id.to_string(),
        messages: all_messages,
        stream: true,
        max_completion_tokens: Some(super::effective_max_output_tokens(model, options)),
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

// SSE parsing
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
    reasoning_details: Option<Vec<ReasoningDetail>>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ReasoningDetail {
    #[serde(rename = "type", default)]
    detail_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

impl ChunkDelta {
    fn reasoning_delta(&self) -> Option<String> {
        let mut delta = [
            self.reasoning_content.as_deref(),
            self.reasoning.as_deref(),
            self.reasoning_text.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<String>();

        if let Some(details) = &self.reasoning_details {
            delta.extend(
                details
                    .iter()
                    .filter(|detail| {
                        detail
                            .detail_type
                            .as_deref()
                            .is_none_or(|ty| ty == "reasoning.text")
                    })
                    .filter_map(|detail| detail.text.as_deref()),
            );
        }

        (!delta.is_empty()).then_some(delta)
    }
}

#[derive(Debug, Deserialize)]
struct ChunkToolCall {
    /// openai's per-call index. parallel tool calls share a single
    /// `tool_calls` array but each delta carries its own `index` so the
    /// receiver can route partial-args chunks to the right call. mush
    /// keys an in-flight `ToolBlock` by this value, falling back to 0
    /// for providers that omit it on single-call streams
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

use super::StreamBlock;

/// in-flight tool call from openai chat completions. parallel calls each
/// keep their own buffer so interleaved deltas (different `index`es in
/// the same chunk or split across chunks) route to the right call.
/// `id` and `name` aren't tracked here because they're already on the
/// corresponding `AssistantContentPart::ToolCall` in `output.content`
/// (recorded at start) so they don't need duplicating
#[derive(Debug)]
struct ToolBlock {
    /// openai's `tool_calls[].index` for this call. used to disambiguate
    /// parallel calls when their deltas interleave
    openai_index: u64,
    /// position of the corresponding `AssistantContentPart::ToolCall`
    /// inside `output.content`. recorded at start so deltas can address
    /// the right entry without re-scanning
    content_index: usize,
    args_buf: String,
}

struct OpenAiCompletionsProcessor {
    current: Option<StreamBlock>,
    tool_blocks: Vec<ToolBlock>,
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
                let events = process_chunk(chunk, output, &mut self.current, &mut self.tool_blocks);
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

fn parse_sse_stream(
    response: reqwest::Response,
    model_id: ModelId,
    provider_name: Provider,
    api: Api,
    call_id: u64,
) -> EventStream {
    let processor = OpenAiCompletionsProcessor {
        current: None,
        tool_blocks: Vec::new(),
    };
    super::sse::run_sse_stream(response, model_id, provider_name, api, call_id, processor)
}

fn process_chunk(
    chunk: ChunkResponse,
    output: &mut AssistantMessage,
    current: &mut Option<StreamBlock>,
    tool_blocks: &mut Vec<ToolBlock>,
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    // capture the openrouter generation id once. some chunks emit it,
    // some don't; sticky-set it the first time we see one
    if output.generation_id.is_none()
        && let Some(id) = chunk.id
    {
        output.generation_id = Some(id);
    }

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
    if let Some(reasoning) = choice.delta.reasoning_delta() {
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

    // handle tool calls. parallel calls each get their own ToolBlock
    // keyed by openai's `tool_calls[].index`. text/thinking is finished
    // before the first tool block opens because chat completions never
    // re-opens text after tool calls
    if let Some(tool_calls) = choice.delta.tool_calls {
        if matches!(
            current,
            Some(StreamBlock::Text { .. } | StreamBlock::Thinking { .. })
        ) {
            finish_block_events(current, output, &mut events);
        }
        for tc in tool_calls {
            let openai_idx = tc.index.unwrap_or(0);
            let block_pos = match tool_blocks
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
                    tool_blocks.push(ToolBlock {
                        openai_index: openai_idx,
                        content_index,
                        args_buf: String::new(),
                    });
                    tool_blocks.len() - 1
                }
            };

            if let Some(func) = tc.function
                && let Some(args) = func.arguments
            {
                tool_blocks[block_pos].args_buf.push_str(&args);
                let content_index = tool_blocks[block_pos].content_index;
                events.push(StreamEvent::ToolCallDelta {
                    content_index,
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
        StreamBlock::ServerSideTranscript => {}
    }
    Some(content_index)
}

/// flush all in-flight parallel tool calls without emitting events.
/// called from the SSE processor's `finish` hook so the final
/// `AssistantMessage` carries fully-parsed args even when the stream
/// ends mid-call (or when openai never sends a terminal marker per
/// tool, which it doesn't on chat completions)
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
    let mut output = AssistantMessage::builder()
        .model("bench".into())
        .provider(Provider::Custom("bench".into()))
        .api(Api::OpenaiCompletions)
        .timestamp_ms(Timestamp::zero())
        .build();
    let mut current = None;
    let mut tool_blocks = Vec::new();

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
        let _ = process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);
    }

    finish_tool_blocks_silent(&mut tool_blocks, &mut output);

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
        let mut tool_blocks = Vec::new();

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

        let events = process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);
        assert!(events.len() >= 2); // TextStart + TextDelta
        assert!(matches!(events[0], StreamEvent::TextStart { .. }));
        assert!(matches!(events[1], StreamEvent::TextDelta { .. }));
        assert!(current.is_some());
    }

    #[test]
    fn process_reasoning_then_text() {
        let mut output = test_output();
        let mut current = None;
        let mut tool_blocks = Vec::new();

        // reasoning chunk
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
        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);
        assert!(matches!(current, Some(StreamBlock::Thinking { .. })));

        // then text
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
        let events = process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);
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
    fn process_openrouter_reasoning_details_as_thinking() {
        let mut output = test_output();
        let mut current = None;
        let mut tool_blocks = Vec::new();

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

        let events = process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);

        assert!(matches!(current, Some(StreamBlock::Thinking { .. })));
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
        let mut current = None;
        let mut tool_blocks = Vec::new();

        // tool call start
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
        let events = process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        );

        // tool call arg continuation
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
        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);

        // mid-stream the args are still partial JSON, so the recorded
        // arguments stay at the empty-object default
        match &output.content[0] {
            AssistantContentPart::ToolCall(tc) => {
                assert_eq!(tc.arguments, serde_json::json!({}));
            }
            other => panic!("expected tool call, got {other:?}"),
        }

        // finish: drains tool_blocks and parses the buffered JSON onto
        // the existing ToolCall part
        finish_tool_blocks_silent(&mut tool_blocks, &mut output);

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
        // openai chat completions streams parallel tool calls keyed by
        // `tool_calls[].index`. for parallel calls the per-tool deltas
        // can interleave, so each delta must be routed to the tool
        // identified by its `index`, not appended to "the latest tool"
        // tracked in a single slot. without index-aware routing, args
        // for tool 0 leak into tool 1 (or vice versa) and one of the
        // tools is silently dropped/emptied
        let mut output = test_output();
        let mut current = None;
        let mut tool_blocks = Vec::new();

        // tool 0 starts (id + name, empty args)
        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_a", "function": {"name": "WebSearch", "arguments": ""}}
            ]}}]
        }))
        .unwrap();
        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);

        // tool 1 starts (id + name, empty args)
        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 1, "id": "call_b", "function": {"name": "WebSearch", "arguments": ""}}
            ]}}]
        }))
        .unwrap();
        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);

        // tool 0 receives a partial-args delta (interleaved with tool 1)
        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"q\":\"foo\"}"}}
            ]}}]
        }))
        .unwrap();
        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);

        // tool 1 receives its partial-args delta
        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 1, "function": {"arguments": "{\"q\":\"bar\"}"}}
            ]}}]
        }))
        .unwrap();
        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);

        finish_tool_blocks_silent(&mut tool_blocks, &mut output);

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
        // openrouter (and any openai-compatible streamer) emits a
        // top-level `id` on every chunk. that id is what feeds the
        // /generation cost reconciliation lookup. capture it once and
        // store it on the AssistantMessage so the post-stream handler
        // can fire the lookup after the turn lands
        let mut output = test_output();
        let mut current = None;
        let mut tool_blocks = Vec::new();

        let chunk: ChunkResponse = serde_json::from_value(serde_json::json!({
            "id": "gen-abc123",
            "choices": [{"delta": {"content": "hi"}}]
        }))
        .expect("chunk parses");
        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);
        assert_eq!(output.generation_id.as_deref(), Some("gen-abc123"));
    }

    #[test]
    fn process_chunk_id_is_sticky_across_chunks() {
        // openrouter sometimes emits a chunk without `id` mid-stream.
        // once captured the id should not be cleared by a subsequent
        // chunk that lacks one
        let mut output = test_output();
        let mut current = None;
        let mut tool_blocks = Vec::new();

        let with_id: ChunkResponse = serde_json::from_value(serde_json::json!({
            "id": "gen-keep-me",
            "choices": [{"delta": {"content": "a"}}]
        }))
        .unwrap();
        process_chunk(with_id, &mut output, &mut current, &mut tool_blocks);
        let without_id: ChunkResponse = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"content": "b"}}]
        }))
        .unwrap();
        process_chunk(without_id, &mut output, &mut current, &mut tool_blocks);
        assert_eq!(output.generation_id.as_deref(), Some("gen-keep-me"));
    }

    #[test]
    fn process_usage_chunk() {
        let mut output = test_output();
        let mut current = None;
        let mut tool_blocks = Vec::new();

        let chunk = ChunkResponse {
            id: None,
            choices: vec![],
            usage: Some(ChunkUsage {
                prompt_tokens: 150,
                completion_tokens: 50,
                prompt_tokens_details: Some(PromptTokenDetails { cached_tokens: 100 }),
            }),
        };

        process_chunk(chunk, &mut output, &mut current, &mut tool_blocks);
        assert_eq!(output.usage.input_tokens, TokenCount::new(50)); // 150 - 100 cached
        assert_eq!(output.usage.output_tokens, TokenCount::new(50));
        assert_eq!(output.usage.cache_read_tokens, TokenCount::new(100));
    }

    #[test]
    fn benchmark_tool_call_deltas_returns_payload_size() {
        assert_eq!(benchmark_tool_call_deltas(8, 1024), 1024);
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
