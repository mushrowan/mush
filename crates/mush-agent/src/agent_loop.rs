use futures::StreamExt;
use mush_ai::registry::{ApiRegistry, LlmContext, ToolDefinition};
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

use crate::tool::{ToolRegistry, ToolResult};

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AgentEvent {
    AgentStart,
    TurnStart {
        turn_index: usize,
    },
    MessageStart {
        message: AssistantMessage,
    },
    StreamEvent {
        event: StreamEvent,
    },
    MessageEnd {
        message: AssistantMessage,
    },
    ToolExecStart {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        args: serde_json::Value,
    },
    ToolOutput {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        output: String,
    },
    ToolExecEnd {
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        result: ToolResult,
    },
    SteeringInjected {
        messages: Vec<Message>,
    },
    FollowUpInjected {
        messages: Vec<Message>,
    },
    HookRan {
        point: crate::hooks::HookPoint,
        result: crate::hooks::HookResult,
    },
    ContextTransformed {
        before_count: usize,
        after_count: usize,
    },
    ContextCompacted {
        before_count: usize,
        compaction: ContextCompaction,
    },
    TurnEnd {
        turn_index: usize,
        message: AssistantMessage,
    },
    MaxTurnsReached {
        max_turns: usize,
    },
    AgentEnd,
    Error {
        error: String,
    },
}

pub const DEFAULT_MAX_TURNS: usize = usize::MAX;

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum ContextTransformResult {
    Unchanged,
    Updated(Vec<Message>),
    Silent(Vec<Message>),
    Compacted(ContextCompaction),
}

#[derive(Debug, Clone)]
pub struct ContextCompaction {
    pub messages: Vec<Message>,
    pub summary: String,
    pub summarised_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    Allow,
    Deny,
    DenyWithReason(String),
}

pub trait AgentHooks: Send + Sync {
    fn get_steering(&self) -> BoxFuture<'static, Vec<Message>> {
        Box::pin(async { vec![] })
    }

    fn get_follow_up(&self) -> BoxFuture<'static, Vec<Message>> {
        Box::pin(async { vec![] })
    }

    fn transform_context<'a>(
        &'a self,
        _messages: &'a [Message],
    ) -> BoxFuture<'a, ContextTransformResult> {
        Box::pin(async { ContextTransformResult::Unchanged })
    }

    fn confirm_tool(
        &self,
        _id: &ToolCallId,
        _name: &str,
        _args: &serde_json::Value,
    ) -> BoxFuture<'static, ConfirmAction> {
        Box::pin(async { ConfirmAction::Allow })
    }
}

pub struct NoopHooks;
impl AgentHooks for NoopHooks {}

pub type SteeringFn = Box<dyn Fn() -> BoxFuture<'static, Vec<Message>> + Send + Sync>;
pub type TransformFn =
    Box<dyn Fn(&[Message]) -> BoxFuture<'_, ContextTransformResult> + Send + Sync>;
pub type ConfirmFn = Box<
    dyn Fn(&ToolCallId, &str, &serde_json::Value) -> BoxFuture<'static, ConfirmAction>
        + Send
        + Sync,
>;

#[derive(Default)]
pub struct ClosureHooks {
    pub steering: Option<SteeringFn>,
    pub follow_up: Option<SteeringFn>,
    pub transform: Option<TransformFn>,
    pub confirm: Option<ConfirmFn>,
}

impl AgentHooks for ClosureHooks {
    fn get_steering(&self) -> BoxFuture<'static, Vec<Message>> {
        match &self.steering {
            Some(f) => f(),
            None => Box::pin(async { vec![] }),
        }
    }

    fn get_follow_up(&self) -> BoxFuture<'static, Vec<Message>> {
        match &self.follow_up {
            Some(f) => f(),
            None => Box::pin(async { vec![] }),
        }
    }

    fn transform_context<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, ContextTransformResult> {
        match &self.transform {
            Some(f) => f(messages),
            None => Box::pin(async { ContextTransformResult::Unchanged }),
        }
    }

    fn confirm_tool(
        &self,
        id: &ToolCallId,
        name: &str,
        args: &serde_json::Value,
    ) -> BoxFuture<'static, ConfirmAction> {
        match &self.confirm {
            Some(f) => f(id, name, args),
            None => Box::pin(async { ConfirmAction::Allow }),
        }
    }
}

pub type DynamicContext = std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>;

pub type FileRuleCallback =
    std::sync::Arc<dyn Fn(&std::path::Path) -> Option<String> + Send + Sync>;

pub type DiagnosticCallback =
    std::sync::Arc<dyn Fn(&std::path::Path) -> BoxFuture<'_, Option<String>> + Send + Sync>;

#[derive(Default, Clone)]
pub struct AgentInjections {
    pub lifecycle_hooks: crate::hooks::LifecycleHooks,
    pub cwd: Option<std::path::PathBuf>,
    pub dynamic_system_context: Option<DynamicContext>,
    pub file_rules: Option<FileRuleCallback>,
    pub lsp_diagnostics: Option<DiagnosticCallback>,
    pub control_endpoint: Option<crate::exec_context::ControlEndpoint>,
}

pub struct AgentConfig<'a> {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub tools: ToolRegistry,
    pub registry: &'a ApiRegistry,
    pub options: StreamOptions,
    pub max_turns: usize,
    pub hooks: Box<dyn AgentHooks>,
    pub injections: AgentInjections,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}

pub fn agent_loop(
    config: AgentConfig<'_>,
    initial_messages: Vec<Message>,
) -> std::pin::Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send + '_>> {
    let stream = async_stream::stream! {
        yield AgentEvent::AgentStart;

        let mut messages = initial_messages;
        let mut turn_index = 0;
        let cancel = config.cancel.clone().unwrap_or_default();

        if !config.injections.lifecycle_hooks.for_point(crate::hooks::HookPoint::PreSession).is_empty() {
            let results = config.injections.lifecycle_hooks
                .run_all(crate::hooks::HookPoint::PreSession, config.injections.cwd.as_deref())
                .await;

            for r in &results {
                yield AgentEvent::HookRan {
                    point: crate::hooks::HookPoint::PreSession,
                    result: r.clone(),
                };
            }
        }

        let mut pending: Vec<Message> = config.hooks.get_steering().await;

        'outer: loop {
            loop {
                if config.cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                    tracing::info!("agent_loop cancelled before turn {turn_index}");
                    break 'outer;
                }

                if !pending.is_empty() {
                    let injected = pending.clone();
                    messages.append(&mut pending);
                    yield AgentEvent::SteeringInjected { messages: injected };
                }

                yield AgentEvent::TurnStart { turn_index };

                tracing::info_span!("agent_turn", turn = turn_index).in_scope(|| {
                    tracing::info!("turn started");
                });
                let llm_messages = {
                    let before = messages.len();
                    match config.hooks.transform_context(&messages).await {
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
                        ContextTransformResult::Compacted(compaction) => {
                            messages = compaction.messages.clone();
                            yield AgentEvent::ContextCompacted {
                                before_count: before,
                                compaction,
                            };
                            messages.clone()
                        }
                    }
                };

                let tool_defs: Vec<ToolDefinition> = config.tools.iter()
                    .map(|t| ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description_for(config.model.api).to_string(),
                        parameters: t.parameters_schema_for(config.model.api),
                    }).collect();

                let system_prompt = match (&config.system_prompt, &config.injections.dynamic_system_context) {
                    (Some(base), Some(suffix_fn)) => {
                        if let Some(suffix) = suffix_fn() {
                            Some(format!("{base}\n\n{suffix}"))
                        } else {
                            Some(base.clone())
                        }
                    }
                    (base, _) => base.clone(),
                };

                let context = LlmContext {
                    system_prompt,
                    messages: llm_messages,
                    tools: tool_defs,
                };

                tracing::debug!(turn = turn_index, model = %config.model.id, "streaming LLM response");

                const MAX_RETRIES: u32 = 5;
                const MAX_BACKOFF_SECS: u64 = 16;
                let mut final_message: Option<AssistantMessage> = None;
                let mut last_error: Option<String> = None;

                'attempt: for attempt in 0..=MAX_RETRIES {
                    if attempt > 0 {
                        let delay = std::time::Duration::from_secs(
                            (1u64 << (attempt - 1)).min(MAX_BACKOFF_SECS),
                        );
                        tracing::warn!(attempt, ?delay, "retrying after transient error");
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {},
                            _ = cancel.cancelled() => break 'outer,
                        }
                    }

                    let response = tokio::select! {
                        response = config.registry.stream(&config.model, &context, &config.options) => response,
                        _ = cancel.cancelled() => break 'outer,
                    };
                    let mut event_stream = match response {
                        Ok(stream) => stream,
                        Err(e) if e.is_retryable() && attempt < MAX_RETRIES => {
                            tracing::warn!(error = %e, "retryable request error");
                            last_error = Some(format!("request failed: {e}"));
                            continue 'attempt;
                        }
                        Err(e) => {
                            last_error = Some(format!("request failed: {e}"));
                            break 'attempt;
                        }
                    };

                    let mut local_final: Option<AssistantMessage> = None;
                    let mut started = false;
                    let mut had_content = false;

                    loop {
                        let event = tokio::select! {
                            event = event_stream.next() => event,
                            _ = cancel.cancelled() => break 'outer,
                        };
                        let Some(event) = event else { break };
                        match &event {
                            StreamEvent::Start { partial } if !started => {
                                yield AgentEvent::MessageStart { message: partial.clone() };
                                started = true;
                            }
                            StreamEvent::Done { message, .. } => {
                                local_final = Some(message.clone());
                            }
                            StreamEvent::TextDelta { delta, .. }
                            | StreamEvent::ThinkingDelta { delta, .. }
                                if !delta.is_empty() =>
                            {
                                had_content = true;
                            }
                            StreamEvent::ToolCallStart { .. }
                            | StreamEvent::ToolCallDelta { .. } => {
                                had_content = true;
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
                                let partial = message.clone();
                                if !partial.content.is_empty() {
                                    yield AgentEvent::MessageEnd { message: partial.clone() };
                                    messages.push(Message::Assistant(partial.clone()));
                                    for tc in tool_calls_in_message(&partial) {
                                        let result = ToolResult::error(
                                            "tool call cancelled because the assistant stream errored",
                                        );
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
                                }
                                yield AgentEvent::Error { error: error_msg };
                                yield AgentEvent::AgentEnd;
                                return;
                            }
                            _ => {}
                        }
                        yield AgentEvent::StreamEvent { event };
                    }

                    if let Some(msg) = &local_final
                        && msg.stop_reason == StopReason::Error
                        && !had_content
                        && let Some(err_msg) = msg.error_message.as_deref()
                        && mush_ai::registry::is_retryable_stream_error_message(err_msg)
                        && attempt < MAX_RETRIES
                    {
                        tracing::warn!(
                            error = %err_msg,
                            "retryable stream error from provider, scheduling retry"
                        );
                        last_error = Some(format!("stream error: {err_msg}"));
                        continue 'attempt;
                    }

                    final_message = local_final;
                    break 'attempt;
                }

                let Some(assistant_msg) = final_message else {
                    let error = last_error
                        .unwrap_or_else(|| "stream ended without a final message".into());
                    yield AgentEvent::Error { error };
                    yield AgentEvent::AgentEnd;
                    return;
                };

                yield AgentEvent::MessageEnd { message: assistant_msg.clone() };
                messages.push(Message::Assistant(assistant_msg.clone()));

                let tool_calls: Vec<&ToolCall> = assistant_msg
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        AssistantContentPart::ToolCall(tc) => Some(tc),
                        _ => None,
                    })
                    .collect();

                if tool_calls.is_empty() {
                    if !config.injections.lifecycle_hooks.for_point(crate::hooks::HookPoint::Stop).is_empty() {
                        let results = config.injections.lifecycle_hooks
                            .run_all(crate::hooks::HookPoint::Stop, config.injections.cwd.as_deref())
                            .await;

                        for r in &results {
                            yield AgentEvent::HookRan {
                                point: crate::hooks::HookPoint::Stop,
                                result: r.clone(),
                            };
                        }
                    }

                    yield AgentEvent::TurnEnd { turn_index, message: assistant_msg };
                    break;
                }

                let mut confirmed: Vec<&ToolCall> = Vec::new();
                for tc in &tool_calls {
                    {
                        let action = config.hooks.confirm_tool(&tc.id, tc.name.as_str(), &tc.arguments).await;
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
                    let mut allowed: Vec<&ToolCall> = Vec::new();
                    for tc in &confirmed {
                        if !config.injections.lifecycle_hooks.for_point(crate::hooks::HookPoint::PreToolUse).is_empty() {
                            let hook_results = config.injections.lifecycle_hooks
                                .run_for_tool(
                                    crate::hooks::HookPoint::PreToolUse,
                                    tc.name.as_str(),
                                    config.injections.cwd.as_deref(),
                                )
                                .await;

                            for r in &hook_results {
                                yield AgentEvent::HookRan {
                                    point: crate::hooks::HookPoint::PreToolUse,
                                    result: r.clone(),
                                };
                            }

                            if crate::hooks::has_blocking_failure(&hook_results) {
                                let feedback = crate::hooks::format_hook_results(
                                    &hook_results,
                                    crate::hooks::HookPoint::PreToolUse,
                                ).unwrap_or_else(|| "pre-tool hook blocked execution".into());
                                let result = ToolResult::error(feedback);
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
                        allowed.push(tc);
                    }

                    for tc in &allowed {
                        yield AgentEvent::ToolExecStart {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            args: tc.arguments.clone(),
                        };
                    }

                    if config.cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                        tracing::info!("agent_loop cancelled before tool execution");
                        for tc in &allowed {
                            let result = ToolResult::error("tool call cancelled before execution");
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
                        break 'outer;
                    }

                    let allowed_calls: Vec<ToolCall> =
                        allowed.iter().map(|&tc| tc.clone()).collect();
                    let tools_ref = &config.tools;
                    let control_endpoint = config.injections.control_endpoint.clone();
                    let futs = crate::tool_grouping::execute_grouped(
                        allowed_calls,
                        |tc: &ToolCall| {
                            crate::tool_grouping::file_path_key(tc.name.as_str(), &tc.arguments)
                        },
                        move |tc: ToolCall| {
                            let exec_id = tc.id.to_string();
                            let endpoint = control_endpoint.clone();
                            crate::exec_context::TOOL_EXEC_ID
                                .scope(exec_id, crate::exec_context::CONTROL_ENDPOINT.scope(endpoint, async move { execute_tool(tools_ref, &tc).await }))
                        },
                    );
                    let results = match config.cancel.as_ref() {
                        Some(token) => tokio::select! {
                            r = futs => r,
                            _ = token.cancelled() => {
                                tracing::info!("agent_loop cancelled during tool execution");
                                for tc in &allowed {
                                    let result = ToolResult::error("tool call cancelled during execution");
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
                                break 'outer;
                            }
                        },
                        None => futs.await,
                    };

                    for (tc, mut result) in allowed.iter().zip(results) {
                        if !config.injections.lifecycle_hooks.for_point(crate::hooks::HookPoint::PostToolUse).is_empty() {
                            let hook_results = config.injections.lifecycle_hooks
                                .run_for_tool(
                                    crate::hooks::HookPoint::PostToolUse,
                                    tc.name.as_str(),
                                    config.injections.cwd.as_deref(),
                                )
                                .await;

                            for r in &hook_results {
                                yield AgentEvent::HookRan {
                                    point: crate::hooks::HookPoint::PostToolUse,
                                    result: r.clone(),
                                };
                            }

                            if let Some(feedback) = crate::hooks::format_hook_results(
                                &hook_results,
                                crate::hooks::HookPoint::PostToolUse,
                            ) {
                                result.content.push(ToolResultContentPart::Text(TextContent {
                                    text: feedback,
                                }));
                            }
                        }

                        if let Some(ref file_rules) = config.injections.file_rules
                            && let Some(path) = extract_file_path(&tc.arguments)
                            && let Some(rule_text) = file_rules(&path)
                        {
                            result.content.push(ToolResultContentPart::Text(TextContent {
                                text: format!("[auto-attached rules]\n{rule_text}"),
                            }));
                        }

                        if let Some(ref lsp_diag) = config.injections.lsp_diagnostics
                            && is_file_modifying_tool(tc.name.as_str())
                            && let Some(path) = extract_file_path(&tc.arguments)
                            && let Some(diag_text) = lsp_diag(&path).await
                        {
                            result.content.push(ToolResultContentPart::Text(TextContent {
                                text: format!("[LSP diagnostics]\n{diag_text}"),
                            }));
                        }

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

                    {
                        let steering = config.hooks.get_steering().await;
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

                if pending.is_empty() {
                    pending = config.hooks.get_steering().await;
                }

                if pending.is_empty() {
                }
            }

            {
                let follow_up = config.hooks.get_follow_up().await;
                if !follow_up.is_empty() {
                    let injected = follow_up.clone();
                    pending = follow_up;
                    yield AgentEvent::FollowUpInjected { messages: injected };
                    continue 'outer;
                }
            }

            break;
        }

        yield AgentEvent::AgentEnd;
    };

    Box::pin(stream)
}

fn extract_file_path(args: &serde_json::Value) -> Option<std::path::PathBuf> {
    args.get("path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
}

fn is_file_modifying_tool(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "write" | "edit" | "apply_patch"
    )
}

fn tool_calls_in_message(message: &AssistantMessage) -> impl Iterator<Item = &ToolCall> {
    message.content.iter().filter_map(|part| match part {
        AssistantContentPart::ToolCall(tool_call) => Some(tool_call),
        _ => None,
    })
}

#[tracing::instrument(name = "tool_exec", skip_all, fields(tool = %tool_call.name))]
async fn execute_tool(tools: &ToolRegistry, tool_call: &ToolCall) -> ToolResult {
    match tools.get(tool_call.name.as_str()) {
        Some(tool) => {
            let limit = tool.output_limit();
            tracing::debug!(tool = %tool_call.name, resolved = tool.name(), ?limit, args = %tool_call.arguments, "executing tool");
            let result = tool.call(tool_call.arguments.clone()).await;
            let output_text: String = result
                .content
                .iter()
                .filter_map(|p| match p {
                    mush_ai::types::ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            tracing::debug!(
                tool = %tool_call.name,
                output_bytes = output_text.len(),
                output_lines = output_text.lines().count(),
                outcome = ?result.outcome,
                ?limit,
                "tool finished, applying truncation"
            );
            tracing::trace!(tool = %tool_call.name, output = %output_text, "tool output content");
            if result.outcome.is_error() {
                tracing::warn!(tool = %tool_call.name, "tool returned error");
            }
            crate::truncation::apply_for_tool(result, limit, Some(tool_call.name.as_str()))
        }
        None => {
            tracing::error!(tool = %tool_call.name, "tool not found");
            let preview = crate::tool::preview_args(&tool_call.arguments);
            ToolResult::error(format!(
                "tool not found: {}\nreceived: {preview}",
                tool_call.name
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{AgentTool, ToolRegistry};

    struct CounterTool;

    #[async_trait::async_trait]
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
        fn output_limit(&self) -> crate::tool::OutputLimit {
            crate::tool::OutputLimit::Head
        }
        async fn execute(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text("42")
        }
    }

    fn test_model() -> Model {
        Model::builder()
            .id("test".into())
            .name("test".into())
            .api(Api::AnthropicMessages)
            .provider(Provider::Anthropic)
            .base_url("https://api.anthropic.com".into())
            .context_window(TokenCount::new(200_000))
            .max_output_tokens(TokenCount::new(8192))
            .build()
    }

    #[test]
    fn tool_not_found_returns_error() {
        let tools = ToolRegistry::new();
        let tc = ToolCall {
            id: "tc_1".into(),
            name: "nonexistent".into(),
            arguments: serde_json::json!({"path": "/etc/passwd", "count": 3}),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute_tool(&tools, &tc));
        assert!(result.outcome.is_error());
        let text = match &result.content[0] {
            mush_ai::types::ToolResultContentPart::Text(t) => &t.text,
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("nonexistent"),
            "error should name the missing tool, got: {text}"
        );
        assert!(
            text.contains("path") && text.contains("/etc/passwd") && text.contains("count"),
            "error should preview attempted args, got: {text}"
        );
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

        let tc = ToolCall {
            id: "tc_1".into(),
            name: "Counter".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());

        let tc = ToolCall {
            id: "tc_2".into(),
            name: "COUNTER".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());
    }

    struct WebSearchTool;

    #[async_trait::async_trait]
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
        fn output_limit(&self) -> crate::tool::OutputLimit {
            crate::tool::OutputLimit::Head
        }
        async fn execute(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text("results")
        }
    }

    #[test]
    fn tool_name_underscore_variants_match() {
        let tools = ToolRegistry::from_boxed(vec![Box::new(WebSearchTool)]);
        let rt = tokio::runtime::Runtime::new().unwrap();

        let tc = ToolCall {
            id: "tc_1".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());

        let tc = ToolCall {
            id: "tc_2".into(),
            name: "websearch".into(),
            arguments: serde_json::json!({}),
        };
        assert!(rt.block_on(execute_tool(&tools, &tc)).outcome.is_success());

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
            hooks: Box::new(NoopHooks),
            injections: AgentInjections::default(),
            cancel: None,
        };

        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> = rt.block_on(async {
            let stream = agent_loop(config, messages);
            stream.collect().await
        });

        assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentEnd)));
    }

    #[test]
    fn stream_error_with_partial_yields_message_end_before_error() {
        struct PartialErrorProvider;

        #[async_trait::async_trait]
        impl mush_ai::registry::ApiProvider for PartialErrorProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }
            async fn stream(
                &self,
                model: &Model,
                _context: &mush_ai::registry::LlmContext,
                _options: &StreamOptions,
            ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError>
            {
                let partial = AssistantMessage::builder()
                    .content(vec![AssistantContentPart::Text(TextContent {
                        text: "I'll start by checking the file".into(),
                    })])
                    .model(model.id.clone())
                    .provider(model.provider.clone())
                    .api(model.api)
                    .stop_reason(StopReason::Error)
                    .error_message("error decoding response body")
                    .timestamp_ms(Timestamp::zero())
                    .build();
                let s = async_stream::stream! {
                    yield StreamEvent::Start { partial: partial.clone() };
                    yield StreamEvent::Error {
                        reason: StopReason::Error,
                        message: partial,
                    };
                };
                Ok(Box::pin(s) as mush_ai::registry::EventStream)
            }
        }

        let mut registry = ApiRegistry::new();
        registry.register(Box::new(PartialErrorProvider));
        let model = test_model();
        let tools = ToolRegistry::new();
        let config = AgentConfig {
            model: model.clone(),
            system_prompt: None,
            tools,
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            hooks: Box::new(NoopHooks),
            injections: AgentInjections::default(),
            cancel: None,
        };
        let user_messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("do the thing".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, user_messages).collect().await });

        let msg_end = events
            .iter()
            .position(|e| matches!(e, AgentEvent::MessageEnd { .. }));
        let err = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Error { .. }));
        let msg_end = msg_end.expect("partial output should be preserved as MessageEnd");
        let err = err.expect("error should still be reported");
        assert!(
            msg_end < err,
            "MessageEnd must come before Error so the TUI/session sees the partial content first"
        );

        let AgentEvent::MessageEnd { message } = &events[msg_end] else {
            unreachable!()
        };
        let text = message
            .content
            .iter()
            .find_map(|p| match p {
                AssistantContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(text, "I'll start by checking the file");
        assert_eq!(message.stop_reason, StopReason::Error);
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

        let transform: TransformFn = Box::new(move |msgs| {
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
            hooks: Box::new(ClosureHooks {
                transform: Some(transform),
                ..ClosureHooks::default()
            }),
            injections: AgentInjections::default(),
            cancel: None,
        };

        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let _events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, messages).collect().await });

        assert!(called.load(Ordering::SeqCst));
    }

    #[test]
    fn follow_up_injection_does_not_consume_turn_budget() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ScriptedProvider {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl mush_ai::registry::ApiProvider for ScriptedProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }

            async fn stream(
                &self,
                model: &Model,
                _context: &mush_ai::registry::LlmContext,
                _options: &StreamOptions,
            ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError>
            {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let msg = AssistantMessage::builder()
                    .content(if call == 0 {
                        vec![AssistantContentPart::Text(TextContent {
                            text: "first".into(),
                        })]
                    } else {
                        vec![AssistantContentPart::ToolCall(ToolCall {
                            id: "tc_1".into(),
                            name: "counter".into(),
                            arguments: serde_json::json!({}),
                        })]
                    })
                    .model(model.id.clone())
                    .provider(model.provider.clone())
                    .api(model.api)
                    .stop_reason(if call == 0 {
                        StopReason::Stop
                    } else {
                        StopReason::ToolUse
                    })
                    .timestamp_ms(Timestamp::zero())
                    .build();

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

        let get_follow_up: SteeringFn = Box::new(move || {
            let calls = follow_up_calls_clone.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    vec![Message::User(
                        UserMessage::builder()
                            .content(UserContent::Text("queued follow-up".into()))
                            .timestamp_ms(Timestamp::zero())
                            .build(),
                    )]
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
            hooks: Box::new(ClosureHooks {
                follow_up: Some(get_follow_up),
                ..ClosureHooks::default()
            }),
            injections: AgentInjections::default(),
            cancel: None,
        };

        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, messages).collect().await });

        let follow_up_count = events
            .iter()
            .filter(|e| match e {
                AgentEvent::FollowUpInjected { messages } => messages.len() == 1,
                _ => false,
            })
            .count();
        assert_eq!(follow_up_count, 1);
        assert!(
            events.iter().any(|e| match e {
                AgentEvent::FollowUpInjected { messages } => messages.iter().any(|m| match m {
                    Message::User(u) => u.text() == "queued follow-up",
                    _ => false,
                }),
                _ => false,
            }),
            "follow-up event should carry the actual injected user message so consumers can persist it to the conversation tree"
        );
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

    #[test]
    fn is_file_modifying_tool_matches_expected() {
        assert!(is_file_modifying_tool("write"));
        assert!(is_file_modifying_tool("Write"));
        assert!(is_file_modifying_tool("edit"));
        assert!(is_file_modifying_tool("apply_patch"));
        assert!(!is_file_modifying_tool("read"));
        assert!(!is_file_modifying_tool("bash"));
        assert!(!is_file_modifying_tool("grep"));
    }

    #[test]
    fn cancelled_token_stops_before_turn() {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();

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
            hooks: Box::new(NoopHooks),
            injections: AgentInjections::default(),
            cancel: Some(token),
        };

        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, messages).collect().await });

        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentStart)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentEnd)));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnStart { .. })),
            "cancelled agent should not start any turns"
        );
    }

    #[test]
    fn cancelled_before_tool_execution_emits_stub_tool_results() {
        struct ToolCallProvider;

        #[async_trait::async_trait]
        impl mush_ai::registry::ApiProvider for ToolCallProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }

            async fn stream(
                &self,
                model: &Model,
                _context: &mush_ai::registry::LlmContext,
                _options: &StreamOptions,
            ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError>
            {
                let message = AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_cancelled".into(),
                        name: "counter".into(),
                        arguments: serde_json::json!({}),
                    })])
                    .model(model.id.clone())
                    .provider(model.provider.clone())
                    .api(model.api)
                    .timestamp_ms(Timestamp::zero())
                    .build();
                let s = async_stream::stream! {
                    yield StreamEvent::Start { partial: message.clone() };
                    yield StreamEvent::Done { reason: StopReason::ToolUse, message };
                };
                Ok(Box::pin(s) as mush_ai::registry::EventStream)
            }
        }

        let mut registry = ApiRegistry::new();
        registry.register(Box::new(ToolCallProvider));
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_from_hook = cancel.clone();
        let config = AgentConfig {
            model: test_model(),
            system_prompt: None,
            tools: ToolRegistry::from_boxed(vec![Box::new(CounterTool)]),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            hooks: Box::new(ClosureHooks {
                confirm: Some(Box::new(move |_, _, _| {
                    cancel_from_hook.cancel();
                    Box::pin(async { ConfirmAction::Allow })
                })),
                ..ClosureHooks::default()
            }),
            injections: AgentInjections::default(),
            cancel: Some(cancel),
        };
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("use a tool".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let events: Vec<AgentEvent> = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { agent_loop(config, messages).collect().await });

        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolExecEnd { tool_call_id, result, .. }
                    if tool_call_id.as_str() == "tc_cancelled" && result.outcome.is_error()
            )
        }));
    }

    #[test]
    fn stream_error_with_partial_tool_call_emits_stub_tool_result() {
        struct ErrorAfterToolCallProvider;

        #[async_trait::async_trait]
        impl mush_ai::registry::ApiProvider for ErrorAfterToolCallProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }

            async fn stream(
                &self,
                model: &Model,
                _context: &mush_ai::registry::LlmContext,
                _options: &StreamOptions,
            ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError>
            {
                let partial = AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_stream_error".into(),
                        name: "counter".into(),
                        arguments: serde_json::json!({}),
                    })])
                    .model(model.id.clone())
                    .provider(model.provider.clone())
                    .api(model.api)
                    .stop_reason(StopReason::Error)
                    .error_message("decode failed")
                    .timestamp_ms(Timestamp::zero())
                    .build();
                let s = async_stream::stream! {
                    yield StreamEvent::Start { partial: partial.clone() };
                    yield StreamEvent::ToolCallStart { content_index: 0 };
                    yield StreamEvent::Error { reason: StopReason::Error, message: partial };
                };
                Ok(Box::pin(s) as mush_ai::registry::EventStream)
            }
        }

        let mut registry = ApiRegistry::new();
        registry.register(Box::new(ErrorAfterToolCallProvider));
        let config = AgentConfig {
            model: test_model(),
            system_prompt: None,
            tools: ToolRegistry::from_boxed(vec![Box::new(CounterTool)]),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            hooks: Box::new(NoopHooks),
            injections: AgentInjections::default(),
            cancel: None,
        };
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("use a tool".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let events: Vec<AgentEvent> = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { agent_loop(config, messages).collect().await });

        assert!(events.iter().any(|event| {
            matches!(
                event,
                AgentEvent::ToolExecEnd { tool_call_id, result, .. }
                    if tool_call_id.as_str() == "tc_stream_error" && result.outcome.is_error()
            )
        }));
    }

    #[test]
    fn overloaded_stream_error_triggers_retry_with_eventual_success() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct OverloadOnceProvider {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl mush_ai::registry::ApiProvider for OverloadOnceProvider {
            fn api(&self) -> Api {
                Api::AnthropicMessages
            }

            async fn stream(
                &self,
                model: &Model,
                _context: &mush_ai::registry::LlmContext,
                _options: &StreamOptions,
            ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError>
            {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let msg = if call == 0 {
                    AssistantMessage::builder()
                        .content(Vec::new())
                        .model(model.id.clone())
                        .provider(model.provider.clone())
                        .api(model.api)
                        .stop_reason(StopReason::Error)
                        .error_message("Overloaded".to_string())
                        .timestamp_ms(Timestamp::zero())
                        .build()
                } else {
                    AssistantMessage::builder()
                        .content(vec![AssistantContentPart::Text(TextContent {
                            text: "hello after retry".into(),
                        })])
                        .model(model.id.clone())
                        .provider(model.provider.clone())
                        .api(model.api)
                        .stop_reason(StopReason::Stop)
                        .timestamp_ms(Timestamp::zero())
                        .build()
                };

                let s = async_stream::stream! {
                    yield StreamEvent::Start { partial: msg.clone() };
                    yield StreamEvent::Done { reason: msg.stop_reason, message: msg };
                };
                Ok(Box::pin(s) as mush_ai::registry::EventStream)
            }
        }

        let mut registry = ApiRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        registry.register(Box::new(OverloadOnceProvider {
            calls: calls.clone(),
        }));

        let model = test_model();
        let tools = ToolRegistry::new();
        let config = AgentConfig {
            model: model.clone(),
            system_prompt: None,
            tools,
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: 1,
            hooks: Box::new(NoopHooks),
            injections: AgentInjections::default(),
            cancel: None,
        };

        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, messages).collect().await });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "provider should be called twice: first overloaded, second succeeds"
        );

        let last_message_end = events
            .iter()
            .rev()
            .find_map(|e| match e {
                AgentEvent::MessageEnd { message } => Some(message),
                _ => None,
            })
            .expect("expected at least one MessageEnd");
        assert_eq!(last_message_end.stop_reason, StopReason::Stop);
        assert_eq!(last_message_end.text(), "hello after retry");

        let error_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Error { .. }))
            .count();
        assert_eq!(
            error_count, 0,
            "no AgentEvent::Error should be emitted when retry succeeds"
        );
    }

    struct RecordingProvider {
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl mush_ai::registry::ApiProvider for RecordingProvider {
        fn api(&self) -> Api {
            Api::AnthropicMessages
        }
        async fn stream(
            &self,
            model: &Model,
            context: &mush_ai::registry::LlmContext,
            _options: &StreamOptions,
        ) -> Result<mush_ai::registry::EventStream, mush_ai::registry::ProviderError> {
            self.seen
                .lock()
                .unwrap()
                .extend(context.messages.iter().filter_map(|message| match message {
                    Message::User(user) => Some(user.text()),
                    _ => None,
                }));
            let done = AssistantMessage::builder()
                .content(vec![AssistantContentPart::Text(TextContent {
                    text: "done".into(),
                })])
                .model(model.id.clone())
                .provider(model.provider.clone())
                .api(model.api)
                .timestamp_ms(Timestamp::zero())
                .build();
            let s = async_stream::stream! {
                yield StreamEvent::Done { message: done, reason: StopReason::Stop };
            };
            Ok(Box::pin(s) as mush_ai::registry::EventStream)
        }
    }

    fn echoing_hook(command: &str) -> crate::hooks::LifecycleHook {
        crate::hooks::LifecycleHook {
            tool_match: "*".into(),
            command: command.into(),
            timeout: std::time::Duration::from_secs(5),
            blocking: false,
        }
    }

    #[tokio::test]
    async fn compaction_replaces_history_for_follow_up_turns() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let user = |text: &str| {
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text(text.into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            )
        };
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(RecordingProvider { seen: seen.clone() }));
        let transform_called = AtomicBool::new(false);
        let follow_up_sent = AtomicBool::new(false);
        let config = AgentConfig {
            model: test_model(),
            system_prompt: None,
            tools: ToolRegistry::new(),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: 3,
            hooks: Box::new(ClosureHooks {
                transform: Some(Box::new(move |messages| {
                    let result = if transform_called.swap(true, Ordering::SeqCst) {
                        assert_eq!(messages.len(), 4);
                        ContextTransformResult::Unchanged
                    } else {
                        ContextTransformResult::Compacted(ContextCompaction {
                            messages: vec![user("summary"), messages[2].clone()],
                            summary: "summary".into(),
                            summarised_count: 2,
                        })
                    };
                    Box::pin(async move { result })
                })),
                follow_up: Some(Box::new(move || {
                    let messages = if follow_up_sent.swap(true, Ordering::SeqCst) {
                        vec![]
                    } else {
                        vec![user("follow up")]
                    };
                    Box::pin(async move { messages })
                })),
                ..Default::default()
            }),
            injections: Default::default(),
            cancel: None,
        };
        let events: Vec<_> = agent_loop(config, vec![user("old 1"), user("old 2"), user("kept")])
            .collect()
            .await;

        assert_eq!(
            *seen.lock().unwrap(),
            ["summary", "kept", "summary", "kept", "follow up"]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ContextCompacted { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_pre_session_hooks_output_stays_out_of_the_conversation() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(RecordingProvider { seen: seen.clone() }));
        let mut lifecycle_hooks = crate::hooks::LifecycleHooks::default();
        lifecycle_hooks.set(
            crate::hooks::HookPoint::PreSession,
            vec![echoing_hook("echo repo is dirty")],
        );
        let config = AgentConfig {
            model: test_model(),
            system_prompt: None,
            tools: ToolRegistry::new(),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: DEFAULT_MAX_TURNS,
            hooks: Box::new(NoopHooks),
            injections: AgentInjections {
                lifecycle_hooks,
                ..AgentInjections::default()
            },
            cancel: None,
        };
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let _: Vec<AgentEvent> = agent_loop(config, messages).collect().await;

        assert_eq!(seen.lock().unwrap().clone(), vec!["hi".to_string()]);
    }

    #[tokio::test]
    async fn a_blocking_stop_hook_ends_the_turn() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = ApiRegistry::new();
        registry.register(Box::new(RecordingProvider { seen: seen.clone() }));
        let mut lifecycle_hooks = crate::hooks::LifecycleHooks::default();
        lifecycle_hooks.set(
            crate::hooks::HookPoint::Stop,
            vec![crate::hooks::LifecycleHook {
                blocking: true,
                ..echoing_hook("echo tests are red; exit 1")
            }],
        );
        let config = AgentConfig {
            model: test_model(),
            system_prompt: None,
            tools: ToolRegistry::new(),
            registry: &registry,
            options: StreamOptions::default(),
            max_turns: 3,
            hooks: Box::new(NoopHooks),
            injections: AgentInjections {
                lifecycle_hooks,
                ..AgentInjections::default()
            },
            cancel: None,
        };
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hi".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let events: Vec<AgentEvent> = agent_loop(config, messages).collect().await;

        let turn_ends = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnd { .. }))
            .count();
        assert_eq!(turn_ends, 1, "the model should not be asked again");
        assert_eq!(seen.lock().unwrap().clone(), vec!["hi".to_string()]);
    }
}
