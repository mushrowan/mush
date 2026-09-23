use std::collections::HashMap;

use mush_agent::AgentEvent;
use mush_agent::tool::ToolResult;
use mush_ai::models;
use mush_ai::registry::{MAX_RETRIES, retry_backoff};
use mush_ai::stream::{PartialMessage, StreamEvent};
use mush_ai::types::*;
use mush_session::Session;
use serde_json::{Value, json};

pub(crate) fn header(session: &Session) -> Value {
    let timestamp = chrono::DateTime::from_timestamp_millis(
        i64::try_from(session.meta.created_at.as_ms()).unwrap_or(i64::MAX),
    )
    .unwrap_or_default()
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut header = json!({
        "type": "session",
        "id": session.meta.id,
        "timestamp": timestamp,
        "cwd": session.meta.cwd,
    });
    if let Some(parent) = &session.meta.parent {
        header["parentSession"] = json!(parent);
    }
    header
}

pub(crate) struct JsonEvents {
    model: Model,
    prompt: Option<Message>,
    partial: Option<PartialMessage>,
    tool_args: HashMap<ToolCallId, Value>,
    tool_results: Vec<Value>,
    messages: Vec<Value>,
    turn_open: bool,
    retries: u32,
    context_tokens: TokenCount,
}

impl JsonEvents {
    pub(crate) fn new(model: Model, prompt: Option<Message>) -> Self {
        Self {
            model,
            prompt,
            partial: None,
            tool_args: HashMap::new(),
            tool_results: Vec::new(),
            messages: Vec::new(),
            turn_open: false,
            retries: 0,
            context_tokens: TokenCount::ZERO,
        }
    }

    pub(crate) fn apply(&mut self, event: &AgentEvent) -> Vec<Value> {
        let mut out = Vec::new();
        match event {
            AgentEvent::AgentStart => out.push(json!({"type": "agent_start"})),
            AgentEvent::TurnStart { .. } => {
                self.turn_open = true;
                out.push(json!({"type": "turn_start"}));
                if let Some(prompt) = self.prompt.take() {
                    self.push_message(&mut out, &prompt);
                }
            }
            AgentEvent::MessageStart { message } => {
                self.partial = Some(PartialMessage::new(message.clone()));
                out.push(json!({
                    "type": "message_start",
                    "message": self.assistant(message),
                }));
            }
            AgentEvent::StreamEvent { event } => {
                if let Some(partial) = &mut self.partial {
                    partial.apply(event);
                    let snapshot = partial.snapshot();
                    let snapshot = self.assistant(&snapshot);
                    if let Some(update) = assistant_event(event, &snapshot) {
                        out.push(json!({
                            "type": "message_update",
                            "assistantMessageEvent": update,
                            "message": snapshot,
                        }));
                    }
                }
            }
            AgentEvent::PartialDiscarded { error } => {
                self.partial = None;
                self.retries += 1;
                out.push(json!({
                    "type": "auto_retry_start",
                    "attempt": self.retries,
                    "maxAttempts": MAX_RETRIES,
                    "delayMs": retry_backoff(self.retries).as_millis(),
                    "errorMessage": error,
                }));
            }
            AgentEvent::MessageEnd { message } => {
                self.partial = None;
                self.context_tokens = message.usage.total_input_tokens();
                self.finish_retry(
                    &mut out,
                    message.is_replayable(),
                    message.error_message.as_deref(),
                );
                let value = self.assistant(message);
                out.push(json!({"type": "message_end", "message": value}));
                self.messages.push(value.clone());
                if !message.is_replayable() {
                    self.end_turn(&mut out, value);
                }
            }
            AgentEvent::ToolExecStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.tool_args.insert(tool_call_id.clone(), args.clone());
                out.push(json!({
                    "type": "tool_execution_start",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "args": args,
                }));
            }
            AgentEvent::ToolOutput {
                tool_call_id,
                tool_name,
                output,
            } => out.push(json!({
                "type": "tool_execution_update",
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "args": self.tool_args.get(tool_call_id).cloned().unwrap_or(Value::Null),
                "partialResult": {"content": [{"type": "text", "text": output}], "details": {}},
            })),
            AgentEvent::ToolExecEnd {
                tool_call_id,
                tool_name,
                result,
            } => {
                self.tool_args.remove(tool_call_id);
                out.push(json!({
                    "type": "tool_execution_end",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "result": tool_result(result),
                    "isError": result.outcome.is_error(),
                }));
                let message = Message::ToolResult(ToolResultMessage {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    content: result.content.clone(),
                    outcome: result.outcome,
                    timestamp_ms: Timestamp::now(),
                    details: result.details.clone(),
                });
                let value = self.push_message(&mut out, &message);
                self.tool_results.push(value);
            }
            AgentEvent::SteeringInjected { messages } => {
                for message in messages {
                    self.push_message(&mut out, message);
                }
            }
            AgentEvent::ContextCompacted { compaction, .. } => {
                out.push(json!({"type": "compaction_start", "reason": "threshold"}));
                out.push(json!({
                    "type": "compaction_end",
                    "reason": "threshold",
                    "result": {
                        "summary": compaction.summary,
                        "tokensBefore": self.context_tokens,
                    },
                    "aborted": false,
                    "willRetry": false,
                }));
            }
            AgentEvent::TurnEnd { message, .. } => {
                let value = self.assistant(message);
                self.end_turn(&mut out, value);
            }
            AgentEvent::Error { error } => {
                self.finish_retry(&mut out, false, Some(error));
                if self.turn_open {
                    let message = AssistantMessage::builder()
                        .model(self.model.id.clone())
                        .provider(self.model.provider.clone())
                        .api(self.model.api)
                        .stop_reason(StopReason::Error)
                        .error_message(error.clone())
                        .timestamp_ms(Timestamp::now())
                        .build();
                    let value = self.assistant(&message);
                    out.push(json!({"type": "message_start", "message": value}));
                    out.push(json!({"type": "message_end", "message": value}));
                    self.messages.push(value.clone());
                    self.end_turn(&mut out, value);
                }
            }
            AgentEvent::AgentEnd => out.push(json!({
                "type": "agent_end",
                "messages": std::mem::take(&mut self.messages),
            })),
            _ => {}
        }
        out
    }

    fn push_message(&mut self, out: &mut Vec<Value>, message: &Message) -> Value {
        let value = self.message(message);
        out.push(json!({"type": "message_start", "message": value}));
        out.push(json!({"type": "message_end", "message": value}));
        self.messages.push(value.clone());
        value
    }

    fn end_turn(&mut self, out: &mut Vec<Value>, message: Value) {
        if !std::mem::take(&mut self.turn_open) {
            return;
        }
        out.push(json!({
            "type": "turn_end",
            "message": message,
            "toolResults": std::mem::take(&mut self.tool_results),
        }));
    }

    fn finish_retry(&mut self, out: &mut Vec<Value>, success: bool, error: Option<&str>) {
        if self.retries == 0 {
            return;
        }
        let mut end = json!({
            "type": "auto_retry_end",
            "success": success,
            "attempt": std::mem::take(&mut self.retries),
        });
        if !success && let Some(error) = error {
            end["finalError"] = json!(error);
        }
        out.push(end);
    }

    fn message(&self, message: &Message) -> Value {
        match message {
            Message::User(user) => json!({
                "role": "user",
                "content": match &user.content {
                    UserContent::Text(text) => json!(text),
                    UserContent::Parts(parts) => parts.iter().map(user_part).collect(),
                },
                "timestamp": user.timestamp_ms.as_ms(),
            }),
            Message::Event(event) => json!({
                "role": "user",
                "content": event.text(),
                "timestamp": event.timestamp_ms.as_ms(),
            }),
            Message::Assistant(assistant) => self.assistant(assistant),
            Message::ToolResult(result) => json!({
                "role": "toolResult",
                "toolCallId": result.tool_call_id,
                "toolName": result.tool_name,
                "content": result.content.iter().map(result_part).collect::<Vec<_>>(),
                "details": result.details.clone().unwrap_or_else(|| json!({})),
                "isError": result.outcome.is_error(),
                "timestamp": result.timestamp_ms.as_ms(),
            }),
        }
    }

    fn assistant(&self, message: &AssistantMessage) -> Value {
        let cost = models::calculate_cost(&self.model, &message.usage);
        let usage = &message.usage;
        let mut value = json!({
            "role": "assistant",
            "content": message.content.iter().filter_map(assistant_part).collect::<Vec<_>>(),
            "api": message.api,
            "provider": message.provider.to_string(),
            "model": message.model,
            "usage": {
                "input": usage.input_tokens,
                "output": usage.output_tokens,
                "cacheRead": usage.cache_read_tokens,
                "cacheWrite": usage.cache_write_tokens,
                "totalTokens": usage.total_tokens(),
                "cost": {
                    "input": cost.input.get(),
                    "output": cost.output.get(),
                    "cacheRead": cost.cache_read.get(),
                    "cacheWrite": cost.cache_write.get(),
                    "total": cost.total().get(),
                },
            },
            "stopReason": stop_reason(message.stop_reason),
            "timestamp": message.timestamp_ms.as_ms(),
        });
        if let Some(error) = &message.error_message {
            value["errorMessage"] = json!(error);
        }
        if let Some(id) = &message.request_id {
            value["responseId"] = json!(id);
        }
        value
    }
}

fn stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        _ => "stop",
    }
}

fn assistant_event(event: &StreamEvent, partial: &Value) -> Option<Value> {
    let (kind, index, extra) = match event {
        StreamEvent::TextStart { content_index } => ("text_start", content_index, None),
        StreamEvent::TextDelta {
            content_index,
            delta,
        } => ("text_delta", content_index, Some(("delta", json!(delta)))),
        StreamEvent::TextEnd {
            content_index,
            text,
        } => ("text_end", content_index, Some(("content", json!(text)))),
        StreamEvent::ThinkingStart { content_index } => ("thinking_start", content_index, None),
        StreamEvent::ThinkingDelta {
            content_index,
            delta,
        } => (
            "thinking_delta",
            content_index,
            Some(("delta", json!(delta))),
        ),
        StreamEvent::ThinkingEnd {
            content_index,
            thinking,
        } => (
            "thinking_end",
            content_index,
            Some(("content", json!(thinking))),
        ),
        StreamEvent::ToolCallStart { content_index } => ("toolcall_start", content_index, None),
        StreamEvent::ToolCallDelta {
            content_index,
            delta,
        } => (
            "toolcall_delta",
            content_index,
            Some(("delta", json!(delta))),
        ),
        StreamEvent::ToolCallEnd {
            content_index,
            id,
            name,
            arguments,
        } => (
            "toolcall_end",
            content_index,
            Some((
                "toolCall",
                json!({"type": "toolCall", "id": id, "name": name, "arguments": arguments}),
            )),
        ),
        _ => return None,
    };
    let mut value = json!({"type": kind, "contentIndex": index, "partial": partial});
    if let Some((key, extra)) = extra {
        value[key] = extra;
    }
    Some(value)
}

fn assistant_part(part: &AssistantContentPart) -> Option<Value> {
    Some(match part {
        AssistantContentPart::Text(text) => json!({"type": "text", "text": text.text}),
        AssistantContentPart::Thinking(ThinkingContent::Thinking {
            thinking,
            signature,
        }) => {
            let mut value = json!({"type": "thinking", "thinking": thinking});
            if let Some(signature) = signature {
                value["thinkingSignature"] = json!(signature);
            }
            value
        }
        AssistantContentPart::Thinking(ThinkingContent::Redacted { data }) => json!({
            "type": "thinking",
            "thinking": "",
            "thinkingSignature": data,
            "redacted": true,
        }),
        AssistantContentPart::ToolCall(call) => json!({
            "type": "toolCall",
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        }),
        AssistantContentPart::ServerToolUse(_) | AssistantContentPart::AdvisorToolResult(_) => {
            return None;
        }
    })
}

fn image(image: &ImageContent) -> Value {
    json!({"type": "image", "data": image.data, "mimeType": image.mime_type.as_str()})
}

fn user_part(part: &UserContentPart) -> Value {
    match part {
        UserContentPart::Text(text) => json!({"type": "text", "text": text.text}),
        UserContentPart::Image(content) => image(content),
    }
}

fn result_part(part: &ToolResultContentPart) -> Value {
    match part {
        ToolResultContentPart::Text(text) => json!({"type": "text", "text": text.text}),
        ToolResultContentPart::Image(content) => image(content),
    }
}

fn tool_result(result: &ToolResult) -> Value {
    json!({
        "content": result.content.iter().map(result_part).collect::<Vec<_>>(),
        "details": result.details.clone().unwrap_or_else(|| json!({})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model::builder()
            .id("fixture".into())
            .name("fixture".into())
            .api(Api::AnthropicMessages)
            .provider(Provider::Anthropic)
            .base_url("http://unused.invalid".into())
            .context_window(TokenCount::new(8192))
            .max_output_tokens(TokenCount::new(1024))
            .build()
    }

    fn assistant(content: Vec<AssistantContentPart>, stop_reason: StopReason) -> AssistantMessage {
        AssistantMessage::builder()
            .content(content)
            .model("fixture".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .stop_reason(stop_reason)
            .timestamp_ms(Timestamp::zero())
            .build()
    }

    fn run(events: &mut JsonEvents, agent_events: Vec<AgentEvent>) -> Vec<Value> {
        agent_events
            .iter()
            .flat_map(|event| events.apply(event))
            .collect()
    }

    fn types(lines: &[Value]) -> Vec<&str> {
        lines
            .iter()
            .filter_map(|line| line["type"].as_str())
            .collect()
    }

    #[test]
    fn a_tool_turn_uses_prime_event_names_and_shapes() {
        let call = ToolCall {
            id: "call".into(),
            name: "read".into(),
            arguments: json!({"path": "a"}),
        };
        let message = assistant(
            vec![AssistantContentPart::ToolCall(call.clone())],
            StopReason::ToolUse,
        );
        let prompt = Message::User(
            UserMessage::builder()
                .content(UserContent::Text("go".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        );
        let mut events = JsonEvents::new(model(), Some(prompt));
        let lines = run(
            &mut events,
            vec![
                AgentEvent::AgentStart,
                AgentEvent::TurnStart { turn_index: 0 },
                AgentEvent::MessageStart {
                    message: assistant(Vec::new(), StopReason::Stop),
                },
                AgentEvent::StreamEvent {
                    event: StreamEvent::ToolCallEnd {
                        content_index: 0,
                        id: "call".into(),
                        name: "read".into(),
                        arguments: call.arguments.clone(),
                    },
                },
                AgentEvent::StreamEvent {
                    event: StreamEvent::Done {
                        reason: StopReason::ToolUse,
                        message: message.clone(),
                    },
                },
                AgentEvent::MessageEnd {
                    message: message.clone(),
                },
                AgentEvent::ToolExecStart {
                    tool_call_id: "call".into(),
                    tool_name: "read".into(),
                    args: call.arguments.clone(),
                },
                AgentEvent::ToolOutput {
                    tool_call_id: "call".into(),
                    tool_name: "read".into(),
                    output: "line".into(),
                },
                AgentEvent::ToolExecEnd {
                    tool_call_id: "call".into(),
                    tool_name: "read".into(),
                    result: ToolResult::text("contents"),
                },
                AgentEvent::TurnEnd {
                    turn_index: 0,
                    message,
                },
                AgentEvent::AgentEnd,
            ],
        );

        assert_eq!(
            types(&lines),
            [
                "agent_start",
                "turn_start",
                "message_start",
                "message_end",
                "message_start",
                "message_update",
                "message_end",
                "tool_execution_start",
                "tool_execution_update",
                "tool_execution_end",
                "message_start",
                "message_end",
                "turn_end",
                "agent_end",
            ]
        );
        let update = &lines[5];
        assert_eq!(update["assistantMessageEvent"]["type"], "toolcall_end");
        assert_eq!(update["assistantMessageEvent"]["contentIndex"], 0);
        assert_eq!(
            update["assistantMessageEvent"]["toolCall"],
            json!({"type": "toolCall", "id": "call", "name": "read", "arguments": {"path": "a"}})
        );
        assert_eq!(update["message"]["content"][0]["type"], "toolCall");
        assert_eq!(lines[6]["message"]["stopReason"], "toolUse");
        assert_eq!(lines[8]["args"], json!({"path": "a"}));
        assert_eq!(lines[8]["partialResult"]["content"][0]["text"], "line");
        assert_eq!(lines[9]["isError"], false);
        assert_eq!(lines[9]["result"]["content"][0]["text"], "contents");
        assert_eq!(lines[11]["message"]["role"], "toolResult");
        assert_eq!(lines[11]["message"]["toolCallId"], "call");
        let turn_end = &lines[12];
        assert_eq!(turn_end["toolResults"].as_array().map(Vec::len), Some(1));
        assert_eq!(turn_end["message"]["role"], "assistant");
        assert_eq!(lines[13]["messages"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn retries_and_failures_close_the_turn_like_prime() {
        let mut events = JsonEvents::new(model(), None);
        let done = assistant(
            vec![AssistantContentPart::Text(TextContent {
                text: "ok".into(),
            })],
            StopReason::Stop,
        );
        let lines = run(
            &mut events,
            vec![
                AgentEvent::TurnStart { turn_index: 0 },
                AgentEvent::MessageStart {
                    message: assistant(Vec::new(), StopReason::Stop),
                },
                AgentEvent::PartialDiscarded {
                    error: "connection reset".into(),
                },
                AgentEvent::MessageStart {
                    message: assistant(Vec::new(), StopReason::Stop),
                },
                AgentEvent::MessageEnd { message: done },
            ],
        );
        assert_eq!(
            types(&lines),
            [
                "turn_start",
                "message_start",
                "auto_retry_start",
                "message_start",
                "auto_retry_end",
                "message_end",
            ]
        );
        assert_eq!(lines[2]["attempt"], 1);
        assert_eq!(lines[2]["errorMessage"], "connection reset");
        assert_eq!(lines[4]["success"], true);

        let mut events = JsonEvents::new(model(), None);
        let lines = run(
            &mut events,
            vec![
                AgentEvent::TurnStart { turn_index: 0 },
                AgentEvent::Error {
                    error: "request failed: 401".into(),
                },
                AgentEvent::AgentEnd,
            ],
        );
        assert_eq!(
            types(&lines),
            [
                "turn_start",
                "message_start",
                "message_end",
                "turn_end",
                "agent_end",
            ]
        );
        assert_eq!(lines[2]["message"]["stopReason"], "error");
        assert_eq!(lines[2]["message"]["errorMessage"], "request failed: 401");
        assert_eq!(lines[3]["toolResults"], json!([]));

        let mut events = JsonEvents::new(model(), None);
        let mut aborted = assistant(Vec::new(), StopReason::Aborted);
        aborted.error_message = Some("request was aborted".into());
        let lines = run(
            &mut events,
            vec![
                AgentEvent::TurnStart { turn_index: 0 },
                AgentEvent::MessageEnd { message: aborted },
                AgentEvent::AgentEnd,
            ],
        );
        assert_eq!(
            types(&lines),
            ["turn_start", "message_end", "turn_end", "agent_end"]
        );
        assert_eq!(lines[1]["message"]["stopReason"], "aborted");
    }
}
