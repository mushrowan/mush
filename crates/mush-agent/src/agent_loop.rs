//! the core agent loop
//!
//! streams an assistant response, extracts tool calls, executes tools,
//! feeds results back, and repeats until the model stops or is aborted.
//!
//! supports steering (inject messages mid-run), follow-ups (continue after
//! agent would stop), and context transforms (compact before each LLM call).

use futures::StreamExt;
use mush_ai::registry::{ApiRegistry, LlmContext, ToolDefinition};
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

use crate::tool::{ToolRegistry, ToolResult};

/// events emitted by the agent loop for UI consumption
#[derive(Debug, Clone)]
#[non_exhaustive]
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
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        args: serde_json::Value,
    },
    /// partial output from a running tool (e.g. bash streaming stdout)
    ToolOutput {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        output: String,
    },
    /// tool execution finished
    ToolExecEnd {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        result: ToolResult,
    },
    /// steering messages injected mid-run
    SteeringInjected { count: usize },
    /// follow-up messages continuing the loop
    FollowUpInjected { count: usize },
    /// context was transformed (e.g. compacted)
    ContextTransformed {
        before_count: usize,
        after_count: usize,
    },
    /// turn finished
    TurnEnd {
        turn_index: usize,
        message: AssistantMessage,
    },
    /// agent hit max turns limit
    MaxTurnsReached { max_turns: usize },
    /// agent loop finished
    AgentEnd,
    /// error occurred
    Error { error: String },
}

/// default max turns before the agent stops (effectively unlimited)
pub const DEFAULT_MAX_TURNS: usize = usize::MAX;

/// callback type for steering and follow-up messages
pub type MessageCallback<'a> = Box<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Message>> + Send>>
        + Send
        + Sync
        + 'a,
>;

#[derive(Debug, Clone)]
pub enum ContextTransformResult {
    Unchanged,
    Updated(Vec<Message>),
    Silent(Vec<Message>),
}

/// callback type for context transforms (e.g. compaction)
pub type ContextTransform<'a> = Box<
    dyn Fn(
            &[Message],
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = ContextTransformResult> + Send + 'a>,
        > + Send
        + Sync
        + 'a,
>;

/// result of a tool confirmation check
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// execute the tool
    Allow,
    /// skip this tool call, return default error to the model
    Deny,
    /// skip this tool call with a specific reason (shown to the model)
    DenyWithReason(String),
}

/// callback for tool confirmation. receives tool call id, name, and args.
pub type ConfirmCallback<'a> = Box<
    dyn Fn(
            &ToolCallId,
            &str,
            &serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ConfirmAction> + Send>>
        + Send
        + Sync
        + 'a,
>;

#[derive(Default)]
pub struct AgentHooks<'a> {
    /// check for steering messages between tool calls.
    /// if messages are returned, remaining tool calls are skipped and
    /// these messages are added before the next LLM call.
    pub get_steering: Option<MessageCallback<'a>>,
    /// check for follow-up messages when agent would otherwise stop.
    /// if messages are returned, the agent continues with another turn.
    pub get_follow_up: Option<MessageCallback<'a>>,
    /// transform the message context before each LLM call.
    /// use for compaction, filtering, or other context management.
    pub transform_context: Option<ContextTransform<'a>>,
    /// confirm before executing a tool. if None, all tools run without confirmation.
    pub confirm_tool: Option<ConfirmCallback<'a>>,
}

/// configuration for running the agent loop
pub struct AgentConfig<'a> {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub tools: ToolRegistry,
    pub registry: &'a ApiRegistry,
    pub options: StreamOptions,
    /// max tool-calling turns before forced stop (default: unlimited)
    pub max_turns: usize,
    pub hooks: AgentHooks<'a>,
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

        // check for steering at start (user may have typed while waiting)
        let mut pending: Vec<Message> = if let Some(ref get) = config.hooks.get_steering {
            get().await
        } else {
            vec![]
        };

        // outer loop: continues when follow-up messages arrive
        'outer: loop {
            // inner loop: process tool calls and steering
            loop {
                // inject pending messages
                if !pending.is_empty() {
                    let count = pending.len();
                    messages.append(&mut pending);
                    yield AgentEvent::SteeringInjected { count };
                }

                yield AgentEvent::TurnStart { turn_index };

                // apply context transform before LLM call
                let llm_messages = if let Some(ref transform) = config.hooks.transform_context {
                    let before = messages.len();
                    match transform(&messages).await {
                        ContextTransformResult::Unchanged => messages.clone(),
                        ContextTransformResult::Updated(transformed) => {
                            let after = transformed.len();
                            if after < before {
                                yield AgentEvent::ContextTransformed {
                                    before_count: before,
                                    after_count: after,
                                };
                            }
                            transformed
                        }
                        ContextTransformResult::Silent(transformed) => transformed,
                    }
                } else {
                    messages.clone()
                };

                // build LLM context
                let tool_defs: Vec<ToolDefinition> = config.tools.iter()
                    .map(|t| ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters: t.parameters_schema(),
                    }).collect();

                let context = LlmContext {
                    system_prompt: config.system_prompt.clone(),
                    messages: llm_messages,
                    tools: tool_defs,
                };

                // stream the assistant response
                tracing::debug!(turn = turn_index, model = %config.model.id, "streaming LLM response");

                // retry transient errors with exponential backoff
                const MAX_RETRIES: u32 = 3;
                let mut event_stream = None;
                let mut last_error = None;

                for attempt in 0..=MAX_RETRIES {
                    if attempt > 0 {
                        let delay = std::time::Duration::from_secs(1 << (attempt - 1));
                        tracing::warn!(attempt, ?delay, "retrying after transient error");
                        tokio::time::sleep(delay).await;
                    }

                    let stream_result = config.registry.stream(
                        &config.model, &context, &config.options,
                    );
                    match stream_result {
                        Ok(fut) => match fut.await {
                            Ok(stream) => {
                                event_stream = Some(stream);
                                break;
                            }
                            Err(e) if e.is_retryable() && attempt < MAX_RETRIES => {
                                tracing::warn!(error = %e, "retryable request error");
                                last_error = Some(format!("request failed: {e}"));
                            }
                            Err(e) => {
                                last_error = Some(format!("request failed: {e}"));
                                break;
                            }
                        },
                        Err(e) => {
                            // setup errors (no provider, missing key) aren't retryable
                            last_error = Some(format!("stream setup failed: {e}"));
                            break;
                        }
                    }
                }

                let mut event_stream = match event_stream {
                    Some(s) => s,
                    None => {
                        yield AgentEvent::Error {
                            error: last_error.unwrap_or_else(|| "unknown stream error".into()),
                        };
                        break;
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
                            tracing::error!(
                                error = %error_msg,
                                model = %config.model.id,
                                provider = %config.model.provider,
                                api = ?config.model.api,
                                partial_content_parts = message.content.len(),
                                partial_stop_reason = ?message.stop_reason,
                                "LLM stream error"
                            );
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
                    break; // no tools, exit inner loop
                }

                // execute tool calls in parallel
                // confirmations are checked sequentially (needs user interaction)
                let mut confirmed: Vec<&ToolCall> = Vec::new();
                for tc in &tool_calls {
                    if let Some(ref confirm) = config.hooks.confirm_tool {
                        let action = confirm(&tc.id, tc.name.as_str(), &tc.arguments).await;
                        let deny_reason = match action {
                            ConfirmAction::Allow => None,
                            ConfirmAction::Deny => Some("tool call denied by user".to_string()),
                            ConfirmAction::DenyWithReason(reason) => Some(reason),
                        };
                        if let Some(reason) = deny_reason {
                            let result = ToolResult::error(reason);
                            yield AgentEvent::ToolExecEnd {
                                tool_call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                result: result.clone(),
                            };
                            messages.push(Message::ToolResult(ToolResultMessage {
                                tool_call_id: tc.id.clone(),
                                tool_name: tc.name.clone(),
                                content: result.content,
                                outcome: ToolOutcome::Error,
                                timestamp_ms: Timestamp::now(),
                            }));
                            continue;
                        }
                    }
                    confirmed.push(tc);
                }

                if !confirmed.is_empty() {
                    // emit start events for all confirmed tools
                    for tc in &confirmed {
                        yield AgentEvent::ToolExecStart {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            args: tc.arguments.clone(),
                        };
                    }

                    // execute all confirmed tools concurrently
                    let futs: Vec<_> = confirmed
                        .iter()
                        .map(|tc| execute_tool(&config.tools, tc))
                        .collect();
                    let results = futures::future::join_all(futs).await;

                    // emit results and push to messages
                    for (tc, result) in confirmed.iter().zip(results) {
                        yield AgentEvent::ToolExecEnd {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            result: result.clone(),
                        };
                        messages.push(Message::ToolResult(ToolResultMessage {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            content: result.content,
                            outcome: result.outcome,
                            timestamp_ms: Timestamp::now(),
                        }));
                    }

                    // check steering after all tools complete
                    if let Some(ref get) = config.hooks.get_steering {
                        let steering = get().await;
                        if !steering.is_empty() {
                            pending = steering;
                        }
                    }
                }

                yield AgentEvent::TurnEnd { turn_index, message: assistant_msg };
                turn_index += 1;

                if turn_index >= config.max_turns {
                    yield AgentEvent::MaxTurnsReached { max_turns: config.max_turns };
                    break 'outer;
                }

                // if no pending steering, check for more
                if pending.is_empty() && let Some(ref get) = config.hooks.get_steering {
                    pending = get().await;
                }

                // if there are pending messages, the inner loop continues
                // if not, we exit to check follow-ups
                if pending.is_empty() {
                    // no tool calls + no steering = agent wants to stop
                    // but we already broke above if tool_calls is empty,
                    // so here we always have tools and should continue
                }
            }

            // agent would stop. check for follow-ups
            if let Some(ref get) = config.hooks.get_follow_up {
                let follow_up = get().await;
                if !follow_up.is_empty() {
                    let count = follow_up.len();
                    pending = follow_up;
                    yield AgentEvent::FollowUpInjected { count };
                    continue 'outer;
                }
            }

            break; // nothing more to do
        }

        yield AgentEvent::AgentEnd;
    };

    Box::pin(stream)
}

async fn execute_tool(tools: &ToolRegistry, tool_call: &ToolCall) -> ToolResult {
    match tools.get(tool_call.name.as_str()) {
        Some(tool) => {
            tracing::debug!(tool = %tool_call.name, resolved = tool.name(), "executing tool");
            let result = tool.execute(tool_call.arguments.clone()).await;
            if result.outcome.is_error() {
                tracing::warn!(tool = %tool_call.name, "tool returned error");
            }
            // skip truncation for tools that handle their own output limits
            // (read already caps at 2000 lines / 50KB with line numbers)
            if crate::truncation::self_truncating(tool.name()) {
                result
            } else {
                crate::truncation::truncate_tool_output(result)
            }
        }
        None => {
            tracing::error!(tool = %tool_call.name, "tool not found");
            ToolResult::error(format!("tool not found: {}", tool_call.name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{AgentTool, ToolRegistry};
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

    fn test_model() -> Model {
        Model {
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
            context_window: TokenCount::new(200_000),
            max_output_tokens: TokenCount::new(8192),
        }
    }

    #[test]
    fn tool_not_found_returns_error() {
        let tools = ToolRegistry::new();
        let tc = ToolCall {
            id: "tc_1".into(),
            name: "nonexistent".into(),
            arguments: serde_json::json!({}),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute_tool(&tools, &tc));
        assert!(result.outcome.is_error());
    }

    #[test]
    fn tool_found_and_executed() {
        let tools = ToolRegistry::from_boxed(vec![Box::new(CounterTool)]);
        let tc = ToolCall {
            id: "tc_1".into(),
            name: "counter".into(),
            arguments: serde_json::json!({}),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute_tool(&tools, &tc));
        assert!(result.outcome.is_success());
    }

    #[test]
    fn tool_name_matching_is_normalised() {
        let tools = ToolRegistry::from_boxed(vec![Box::new(CounterTool)]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // PascalCase should match snake_case tool
        let tc = ToolCall {
            id: "tc_1".into(),
            name: "Counter".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());

        // UPPERCASE should match
        let tc = ToolCall {
            id: "tc_2".into(),
            name: "COUNTER".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());
    }

    struct WebSearchTool;

    impl AgentTool for WebSearchTool {
        fn name(&self) -> &str {
            "web_search"
        }
        fn label(&self) -> &str {
            "Web Search"
        }
        fn description(&self) -> &str {
            "searches the web"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + '_>> {
            Box::pin(async { ToolResult::text("results") })
        }
    }

    #[test]
    fn tool_name_underscore_variants_match() {
        let tools = ToolRegistry::from_boxed(vec![Box::new(WebSearchTool)]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // PascalCase (what claude sends) should match web_search
        let tc = ToolCall {
            id: "tc_1".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());

        // lowercase no underscore should match
        let tc = ToolCall {
            id: "tc_2".into(),
            name: "websearch".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());

        // exact match still works
        let tc = ToolCall {
            id: "tc_3".into(),
            name: "web_search".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());
    }

    #[test]
    fn agent_loop_errors_without_provider() {
        let registry = ApiRegistry::new();
        let model = test_model();
        let tools = ToolRegistry::new();

        let config = AgentConfig {
            model: model.clone(),
            system_prompt: None,
            tools,
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            hooks: AgentHooks::default(),
        };

        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp_ms: Timestamp::zero(),
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

    #[test]
    fn context_transform_called_before_llm() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        let registry = ApiRegistry::new();
        let model = test_model();
        let tools = ToolRegistry::new();

        let transform: ContextTransform<'_> = Box::new(move |msgs| {
            called_clone.store(true, Ordering::SeqCst);
            let msgs = msgs.to_vec();
            Box::pin(async move { ContextTransformResult::Silent(msgs) })
        });

        let config = AgentConfig {
            model: model.clone(),
            system_prompt: None,
            tools,
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            hooks: AgentHooks {
                transform_context: Some(transform),
                ..AgentHooks::default()
            },
        };

        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp_ms: Timestamp::zero(),
        })];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, messages).collect().await });

        // transform was called even though LLM will error (no provider)
        assert!(called.load(Ordering::SeqCst));
    }

    #[test]
    fn follow_up_injection_does_not_consume_turn_budget() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ScriptedProvider {
            calls: Arc<AtomicUsize>,
        }

        impl mush_ai::registry::ApiProvider for ScriptedProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }

            fn stream(
                &self,
                model: &Model,
                _context: &mush_ai::registry::LlmContext,
                _options: &StreamOptions,
            ) -> mush_ai::registry::StreamResult {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let msg = AssistantMessage {
                    content: if call == 0 {
                        vec![AssistantContentPart::Text(TextContent {
                            text: "first".into(),
                        })]
                    } else {
                        vec![AssistantContentPart::ToolCall(ToolCall {
                            id: "tc_1".into(),
                            name: "counter".into(),
                            arguments: serde_json::json!({}),
                        })]
                    },
                    model: model.id.clone(),
                    provider: model.provider.clone(),
                    api: model.api,
                    usage: Usage::default(),
                    stop_reason: if call == 0 {
                        StopReason::Stop
                    } else {
                        StopReason::ToolUse
                    },
                    error_message: None,
                    timestamp_ms: Timestamp::zero(),
                };

                Box::pin(async move {
                    let s = async_stream::stream! {
                        yield StreamEvent::Start {
                            partial: msg.clone(),
                        };
                        yield StreamEvent::Done {
                            reason: msg.stop_reason,
                            message: msg,
                        };
                    };
                    Ok(Box::pin(s) as mush_ai::registry::EventStream)
                })
            }
        }

        let mut registry = ApiRegistry::new();
        registry.register(Box::new(ScriptedProvider {
            calls: Arc::new(AtomicUsize::new(0)),
        }));

        let model = test_model();
        let tools = ToolRegistry::from_boxed(vec![Box::new(CounterTool)]);

        let follow_up_calls = Arc::new(AtomicUsize::new(0));
        let follow_up_calls_clone = follow_up_calls.clone();

        let get_follow_up: MessageCallback<'_> = Box::new(move || {
            let calls = follow_up_calls_clone.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    vec![Message::User(UserMessage {
                        content: UserContent::Text("queued follow-up".into()),
                        timestamp_ms: Timestamp::zero(),
                    })]
                } else {
                    vec![]
                }
            })
        });

        let config = AgentConfig {
            model: model.clone(),
            system_prompt: None,
            tools,
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: 1,
            hooks: AgentHooks {
                get_follow_up: Some(get_follow_up),
                ..AgentHooks::default()
            },
        };

        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp_ms: Timestamp::zero(),
        })];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, messages).collect().await });

        let follow_up_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::FollowUpInjected { count: 1 }))
            .count();
        assert_eq!(follow_up_count, 1);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolExecStart { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::MaxTurnsReached { max_turns: 1 }))
        );
    }

    #[test]
    fn config_defaults_are_sensible() {
        assert_eq!(DEFAULT_MAX_TURNS, usize::MAX);
    }
}
