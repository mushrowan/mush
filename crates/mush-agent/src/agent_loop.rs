//! the core agent loop
//!
//! streams an assistant response, extracts tool calls, executes tools,
//! feeds results back, and repeats until the model stops or is aborted.

use futures::StreamExt;
use mush_ai::registry::{ApiRegistry, LlmContext, ToolDefinition};
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

use crate::tool::{AgentTool, ToolResult};

/// events emitted by the agent loop for UI consumption
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// agent loop started
    AgentStart,
    /// new turn starting (one LLM call + tool executions)
    TurnStart { turn_index: usize },
    /// assistant message streaming started
    MessageStart { message: AssistantMessage },
    /// streaming update (wraps the underlying stream event)
    StreamEvent { event: StreamEvent },
    /// assistant message streaming finished
    MessageEnd { message: AssistantMessage },
    /// tool execution starting
    ToolExecStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// tool execution finished
    ToolExecEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolResult,
    },
    /// turn finished
    TurnEnd {
        turn_index: usize,
        message: AssistantMessage,
    },
    /// agent loop finished
    AgentEnd,
    /// error occurred
    Error { error: String },
}

/// configuration for running the agent loop
pub struct AgentConfig<'a> {
    pub model: &'a Model,
    pub system_prompt: Option<String>,
    pub tools: &'a [Box<dyn AgentTool>],
    pub registry: &'a ApiRegistry,
    pub options: StreamOptions,
}

/// run the agent loop, yielding events as they happen
///
/// takes an initial set of messages and runs until the model stops
/// producing tool calls (or hits an error/abort).
pub fn agent_loop(
    config: AgentConfig<'_>,
    initial_messages: Vec<Message>,
) -> std::pin::Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send + '_>> {
    let stream = async_stream::stream! {
        yield AgentEvent::AgentStart;

        let mut messages = initial_messages;
        let mut turn_index = 0;

        loop {
            yield AgentEvent::TurnStart { turn_index };

            // build LLM context
            let tool_defs: Vec<ToolDefinition> = config.tools.iter().map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
            }).collect();

            let context = LlmContext {
                system_prompt: config.system_prompt.clone(),
                messages: messages.clone(),
                tools: tool_defs,
            };

            // stream the assistant response
            let stream_result = config.registry.stream(config.model, &context, &config.options);
            let mut event_stream = match stream_result {
                Ok(fut) => match fut.await {
                    Ok(s) => s,
                    Err(e) => {
                        yield AgentEvent::Error { error: e.to_string() };
                        yield AgentEvent::AgentEnd;
                        return;
                    }
                },
                Err(e) => {
                    yield AgentEvent::Error { error: e.to_string() };
                    yield AgentEvent::AgentEnd;
                    return;
                }
            };

            let mut final_message: Option<AssistantMessage> = None;
            let mut started = false;

            while let Some(event) = event_stream.next().await {
                match &event {
                    StreamEvent::Start { partial } if !started => {
                        yield AgentEvent::MessageStart { message: partial.clone() };
                        started = true;
                    }
                    StreamEvent::Done { message, .. } => {
                        final_message = Some(message.clone());
                    }
                    StreamEvent::Error { message, .. } => {
                        let error_msg = message.error_message.clone().unwrap_or_else(|| "unknown error".into());
                        yield AgentEvent::Error { error: error_msg };
                        yield AgentEvent::AgentEnd;
                        return;
                    }
                    _ => {}
                }
                yield AgentEvent::StreamEvent { event };
            }

            let Some(assistant_msg) = final_message else {
                yield AgentEvent::Error { error: "stream ended without a final message".into() };
                yield AgentEvent::AgentEnd;
                return;
            };

            yield AgentEvent::MessageEnd { message: assistant_msg.clone() };
            messages.push(Message::Assistant(assistant_msg.clone()));

            // extract tool calls
            let tool_calls: Vec<&ToolCall> = assistant_msg
                .content
                .iter()
                .filter_map(|p| match p {
                    AssistantContentPart::ToolCall(tc) => Some(tc),
                    _ => None,
                })
                .collect();

            if tool_calls.is_empty() {
                yield AgentEvent::TurnEnd { turn_index, message: assistant_msg };
                break;
            }

            // execute tool calls
            for tc in &tool_calls {
                yield AgentEvent::ToolExecStart {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    args: tc.arguments.clone(),
                };

                let result = execute_tool(config.tools, tc).await;

                yield AgentEvent::ToolExecEnd {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    result: result.clone(),
                };

                messages.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    content: result.content,
                    is_error: result.is_error,
                    timestamp_ms: timestamp_ms(),
                }));
            }

            yield AgentEvent::TurnEnd { turn_index, message: assistant_msg };
            turn_index += 1;
        }

        yield AgentEvent::AgentEnd;
    };

    Box::pin(stream)
}

async fn execute_tool(tools: &[Box<dyn AgentTool>], tool_call: &ToolCall) -> ToolResult {
    let tool = tools.iter().find(|t| t.name() == tool_call.name);
    match tool {
        Some(t) => t.execute(tool_call.arguments.clone()).await,
        None => ToolResult::error(format!("tool not found: {}", tool_call.name)),
    }
}

fn timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::AgentTool;
    use std::pin::Pin;

    struct CounterTool;

    impl AgentTool for CounterTool {
        fn name(&self) -> &str {
            "counter"
        }
        fn label(&self) -> &str {
            "Counter"
        }
        fn description(&self) -> &str {
            "returns a count"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
            Box::pin(async { ToolResult::text("42") })
        }
    }

    #[test]
    fn tool_not_found_returns_error() {
        let tools: Vec<Box<dyn AgentTool>> = vec![];
        let tc = ToolCall {
            id: "tc_1".into(),
            name: "nonexistent".into(),
            arguments: serde_json::json!({}),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute_tool(&tools, &tc));
        assert!(result.is_error);
    }

    #[test]
    fn tool_found_and_executed() {
        let tools: Vec<Box<dyn AgentTool>> = vec![Box::new(CounterTool)];
        let tc = ToolCall {
            id: "tc_1".into(),
            name: "counter".into(),
            arguments: serde_json::json!({}),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute_tool(&tools, &tc));
        assert!(!result.is_error);
    }

    #[test]
    fn agent_loop_errors_without_provider() {
        let registry = ApiRegistry::new();
        let model = Model {
            id: "test".into(),
            name: "test".into(),
            api: Api::AnthropicMessages,
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_output_tokens: 8192,
        };
        let tools: Vec<Box<dyn AgentTool>> = vec![];

        let config = AgentConfig {
            model: &model,
            system_prompt: None,
            tools: &tools,
            registry: &registry,
            options: StreamOptions::default(),
        };

        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp_ms: 0,
        })];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> = rt.block_on(async {
            let stream = agent_loop(config, messages);
            stream.collect().await
        });

        // should get AgentStart, TurnStart, Error, AgentEnd
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentEnd)));
    }
}
