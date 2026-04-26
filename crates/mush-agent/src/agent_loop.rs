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
    /// steering messages injected mid-run.
    /// the actual messages are carried so consumers can persist them
    /// to a conversation tree (else the cache prefix drifts on the
    /// next stream when the tree-derived context omits them)
    SteeringInjected { messages: Vec<Message> },
    /// follow-up messages continuing the loop.
    /// carries the messages for the same persistence reason as
    /// [`AgentEvent::SteeringInjected`]
    FollowUpInjected { messages: Vec<Message> },
    /// lifecycle hook ran
    HookRan {
        point: crate::hooks::HookPoint,
        result: crate::hooks::HookResult,
    },
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

/// boxed future for async callback return types
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum ContextTransformResult {
    Unchanged,
    Updated(Vec<Message>),
    Silent(Vec<Message>),
}

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

/// extension point for agent loop lifecycle callbacks.
/// all methods have default no-op implementations so callers only
/// override what they need. use `NoopHooks` for the default
pub trait AgentHooks: Send + Sync {
    /// check for steering messages between tool calls.
    /// if messages are returned, remaining tool calls are skipped and
    /// these messages are added before the next LLM call
    fn get_steering(&self) -> BoxFuture<'static, Vec<Message>> {
        Box::pin(async { vec![] })
    }

    /// check for follow-up messages when agent would otherwise stop.
    /// if messages are returned, the agent continues with another turn
    fn get_follow_up(&self) -> BoxFuture<'static, Vec<Message>> {
        Box::pin(async { vec![] })
    }

    /// transform the message context before each LLM call.
    /// use for compaction, filtering, or other context management
    fn transform_context<'a>(
        &'a self,
        _messages: &'a [Message],
    ) -> BoxFuture<'a, ContextTransformResult> {
        Box::pin(async { ContextTransformResult::Unchanged })
    }

    /// confirm before executing a tool. return Allow to proceed,
    /// Deny or DenyWithReason to skip
    fn confirm_tool(
        &self,
        _id: &ToolCallId,
        _name: &str,
        _args: &serde_json::Value,
    ) -> BoxFuture<'static, ConfirmAction> {
        Box::pin(async { ConfirmAction::Allow })
    }
}

/// default no-op hooks implementation
pub struct NoopHooks;
impl AgentHooks for NoopHooks {}

/// closure type aliases for `ClosureHooks` fields
pub type SteeringFn = Box<dyn Fn() -> BoxFuture<'static, Vec<Message>> + Send + Sync>;
pub type TransformFn =
    Box<dyn Fn(&[Message]) -> BoxFuture<'_, ContextTransformResult> + Send + Sync>;
pub type ConfirmFn = Box<
    dyn Fn(&ToolCallId, &str, &serde_json::Value) -> BoxFuture<'static, ConfirmAction>
        + Send
        + Sync,
>;

/// callback-based hooks adapter. wraps optional closures into the
/// `AgentHooks` trait. callers set only the fields they need
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

/// callback type for dynamic system prompt additions, called each turn
pub type DynamicContext = std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// callback type for file-triggered rule injection.
/// given a file path from a tool call, returns any matching rule content
/// that should be appended to the tool result.
pub type FileRuleCallback =
    std::sync::Arc<dyn Fn(&std::path::Path) -> Option<String> + Send + Sync>;

/// async callback for getting diagnostics after a file-modifying tool runs.
/// receives the file path from tool arguments, returns formatted diagnostics
/// to append to the tool result.
pub type DiagnosticCallback =
    std::sync::Arc<dyn Fn(&std::path::Path) -> BoxFuture<'_, Option<String>> + Send + Sync>;

/// injection callbacks for file rules, diagnostics, and dynamic context.
/// grouped separately from core config since these are all optional
/// extension points wired up by the host application.
#[derive(Default, Clone)]
pub struct AgentInjections {
    /// user-configured lifecycle hooks (shell commands)
    pub lifecycle_hooks: crate::hooks::LifecycleHooks,
    /// working directory for lifecycle hook commands
    pub cwd: Option<std::path::PathBuf>,
    /// dynamic addition to system prompt, called before each LLM call.
    /// used for repo map updates and other live context.
    pub dynamic_system_context: Option<DynamicContext>,
    /// file-triggered rule injection. when a tool touches a file,
    /// this callback is checked for matching rules to append.
    pub file_rules: Option<FileRuleCallback>,
    /// LSP diagnostic injection. after file-modifying tools (write, edit,
    /// apply_patch), queries the LSP server for diagnostics and appends them.
    pub lsp_diagnostics: Option<DiagnosticCallback>,
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
    pub hooks: Box<dyn AgentHooks>,
    /// injection callbacks and context
    pub injections: AgentInjections,
    /// cooperative cancellation token. when cancelled, the agent loop
    /// stops before the next turn or tool execution.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
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

        // run pre-session hooks (once, before the first LLM call)
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

            // if any pre-session hook produced output, inject it as context
            let all_output: String = results
                .iter()
                .filter(|r| !r.output.is_empty())
                .map(|r| r.output.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !all_output.is_empty() {
                messages.push(Message::User(UserMessage {
                    content: UserContent::Text(format!(
                        "[pre-session hook output]\n{all_output}"
                    )),
                    timestamp_ms: Timestamp::now(),
                }));
            }
        }

        // check for steering at start (user may have typed while waiting)
        let mut pending: Vec<Message> = config.hooks.get_steering().await;

        // outer loop: continues when follow-up messages arrive
        'outer: loop {
            // inner loop: process tool calls and steering
            loop {
                // check cooperative cancellation before each turn
                if config.cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                    tracing::info!("agent_loop cancelled before turn {turn_index}");
                    break 'outer;
                }

                // inject pending messages
                if !pending.is_empty() {
                    let injected = pending.clone();
                    messages.append(&mut pending);
                    yield AgentEvent::SteeringInjected { messages: injected };
                }

                yield AgentEvent::TurnStart { turn_index };

                // apply context transform before LLM call
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
                    }
                };

                // build LLM context
                let tool_defs: Vec<ToolDefinition> = config.tools.iter()
                    .map(|t| ToolDefinition {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters: t.parameters_schema(),
                    }).collect();

                // compose system prompt from static base + dynamic suffix
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

                    match config.registry.stream(
                        &config.model, &context, &config.options,
                    ).await {
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
                    // run stop hooks before declaring done
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

                        if crate::hooks::has_blocking_failure(&results) {
                            if let Some(feedback) = crate::hooks::format_hook_results(
                                &results,
                                crate::hooks::HookPoint::Stop,
                            ) {
                                messages.push(Message::User(UserMessage {
                                    content: UserContent::Text(feedback),
                                    timestamp_ms: Timestamp::now(),
                                }));
                            }
                            yield AgentEvent::TurnEnd { turn_index, message: assistant_msg };
                            turn_index += 1;
                            if turn_index >= config.max_turns {
                                yield AgentEvent::MaxTurnsReached { max_turns: config.max_turns };
                                break 'outer;
                            }
                            continue; // re-enter inner loop
                        }
                    }

                    yield AgentEvent::TurnEnd { turn_index, message: assistant_msg };
                    break; // no tools, exit inner loop
                }

                // execute tool calls in parallel
                // confirmations are checked sequentially (needs user interaction)
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
                    // run pre-tool hooks and filter out blocked tools
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
                                // pre-hook blocked this tool, return feedback as tool result
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

                    // emit start events for all allowed tools
                    for tc in &allowed {
                        yield AgentEvent::ToolExecStart {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            args: tc.arguments.clone(),
                        };
                    }

                    // check cancellation before executing tools
                    if config.cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                        tracing::info!("agent_loop cancelled before tool execution");
                        break 'outer;
                    }

                    // execute all allowed tools concurrently. calls that
                    // mutate the same file are serialised within the same
                    // group to avoid read-modify-write races (the bug
                    // where the last writer wins and earlier edits are
                    // silently lost). independent groups still run in
                    // parallel. racing the batch against the cancel token
                    // lets the user abort a long-running tool: tool
                    // futures get dropped on cancel, which triggers
                    // kill_on_drop on the bash child processes (see
                    // mush-tools::bash)
                    let allowed_calls: Vec<ToolCall> =
                        allowed.iter().map(|&tc| tc.clone()).collect();
                    let tools_ref = &config.tools;
                    let futs = crate::tool_grouping::execute_grouped(
                        allowed_calls,
                        |tc: &ToolCall| {
                            crate::tool_grouping::file_path_key(tc.name.as_str(), &tc.arguments)
                        },
                        move |tc: ToolCall| async move { execute_tool(tools_ref, &tc).await },
                    );
                    let results = match config.cancel.as_ref() {
                        Some(token) => tokio::select! {
                            r = futs => r,
                            _ = token.cancelled() => {
                                tracing::info!("agent_loop cancelled during tool execution");
                                break 'outer;
                            }
                        },
                        None => futs.await,
                    };

                    // emit results, run post-tool hooks, push to messages
                    for (tc, mut result) in allowed.iter().zip(results) {
                        // run post-tool hooks
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

                            // append hook output to the tool result
                            if let Some(feedback) = crate::hooks::format_hook_results(
                                &hook_results,
                                crate::hooks::HookPoint::PostToolUse,
                            ) {
                                result.content.push(ToolResultContentPart::Text(TextContent {
                                    text: feedback,
                                }));
                            }
                        }

                        // check file rules for path-based tool calls
                        if let Some(ref file_rules) = config.injections.file_rules
                            && let Some(path) = extract_file_path(&tc.arguments)
                            && let Some(rule_text) = file_rules(&path)
                        {
                            result.content.push(ToolResultContentPart::Text(TextContent {
                                text: format!("[auto-attached rules]\n{rule_text}"),
                            }));
                        }

                        // inject LSP diagnostics for file-modifying tools
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

                    // check steering after all tools complete
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

                // if no pending steering, check for more
                if pending.is_empty() {
                    pending = config.hooks.get_steering().await;
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
            {
                let follow_up = config.hooks.get_follow_up().await;
                if !follow_up.is_empty() {
                    let injected = follow_up.clone();
                    pending = follow_up;
                    yield AgentEvent::FollowUpInjected { messages: injected };
                    continue 'outer;
                }
            }

            break; // nothing more to do
        }

        yield AgentEvent::AgentEnd;
    };

    Box::pin(stream)
}

/// extract a file path from tool call arguments
///
/// tools like read, write, edit, and apply_patch use a `path` field
fn extract_file_path(args: &serde_json::Value) -> Option<std::path::PathBuf> {
    args.get("path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
}

/// tools that modify files on disk (diagnostics are relevant after these)
fn is_file_modifying_tool(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "write" | "edit" | "apply_patch"
    )
}

#[tracing::instrument(name = "tool_exec", skip_all, fields(tool = %tool_call.name))]
async fn execute_tool(tools: &ToolRegistry, tool_call: &ToolCall) -> ToolResult {
    match tools.get(tool_call.name.as_str()) {
        Some(tool) => {
            let limit = tool.output_limit();
            tracing::debug!(tool = %tool_call.name, resolved = tool.name(), ?limit, args = %tool_call.arguments, "executing tool");
            let result = tool.execute(tool_call.arguments.clone()).await;
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
            crate::truncation::apply(result, limit)
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
        async fn execute(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text("42")
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
            arguments: serde_json::json!({"path": "/etc/passwd", "count": 3}),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(execute_tool(&tools, &tc));
        assert!(result.outcome.is_error());
        // the tool-not-found error should include the attempted args so a
        // model (or human reading the log) can see which call was
        // malformed. particularly useful when a misspelled tool name is
        // masking a real operation the model was trying to perform.
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
        async fn execute(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text("results")
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
            hooks: Box::new(NoopHooks),
            injections: AgentInjections::default(),
            cancel: None,
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
            hooks: Box::new(ClosureHooks {
                follow_up: Some(get_follow_up),
                ..ClosureHooks::default()
            }),
            injections: AgentInjections::default(),
            cancel: None,
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

        let messages = vec![Message::User(UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp_ms: Timestamp::zero(),
        })];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<AgentEvent> =
            rt.block_on(async { agent_loop(config, messages).collect().await });

        // should get AgentStart then AgentEnd (no TurnStart, no Error)
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentStart)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AgentEnd)));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnStart { .. })),
            "cancelled agent should not start any turns"
        );
    }
}
