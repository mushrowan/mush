use std::path::PathBuf;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::StreamBlock;
use super::sse::safe_preview_text;
use super::{MessageVisitor, maybe_trim_tool_output, walk_messages};
use crate::env::anthropic_api_key;
use crate::ids::{lower_hex, random_bytes, random_uuid_v4};
use crate::registry::{
    ApiProvider, EventStream, LlmContext, ProviderError, STREAM_ENDED_EARLY, ToolDefinition,
};
use crate::stream::StreamEvent;
use crate::types::*;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const ANTHROPIC_DIRECT_API: &str = "api.anthropic.com";
const CLAUDE_CODE_VERSION: &str = "2.1.280";
const CLAUDE_CODE_VERSION_SALT: &str = "59cf53e54c78";
const CLAUDE_CODE_MAX_OUTPUT_TOKENS: u64 = 128_000;
const ADVISOR_TOOL_TYPE: &str = "advisor_20260301";
const ADVISOR_TOOL_NAME: &str = "advisor";
const DEFAULT_ADVISOR_MODEL: &str = "claude-opus-5-5";

const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

fn claude_cli_user_agent() -> String {
    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)")
}

fn claude_code_version_suffix(first_user_text: &str) -> String {
    let units: Vec<u16> = first_user_text.encode_utf16().collect();
    let picked: String = [4, 7, 20]
        .into_iter()
        .map(|i| {
            units.get(i).map_or_else(
                || "0".to_string(),
                |unit| String::from_utf16_lossy(&[*unit]),
            )
        })
        .collect();
    let digest = Sha256::digest(format!(
        "{CLAUDE_CODE_VERSION_SALT}{picked}{CLAUDE_CODE_VERSION}"
    ));
    let mut hex = lower_hex(&digest[..2]);
    hex.truncate(3);
    hex
}

fn first_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find_map(|message| match message {
            Message::User(user) => Some(match &user.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Parts(parts) => parts
                    .iter()
                    .find_map(|part| match part {
                        UserContentPart::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        })
        .unwrap_or_default()
}

fn is_request_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("req_") else {
        return false;
    };
    (1..=36).contains(&rest.len())
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, group)| is_hex_len(group, *len))
}

fn last_request_id(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(assistant) => assistant.request_id.clone(),
            _ => None,
        })
        .filter(|id| is_request_id(id))
}

fn last_prompt_id(messages: &[Message]) -> Option<PromptId> {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => user.prompt_id.clone(),
            _ => None,
        })
        .filter(|id| is_uuid(id))
}

fn claude_code_billing_header(messages: &[Message]) -> String {
    let suffix = claude_code_version_suffix(&first_user_text(messages));
    let mut header = format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION}.{suffix}; cc_entrypoint=cli; cch=00000;"
    );
    if let Some(prev_req) = last_request_id(messages) {
        header.push_str(&format!(" cc_prev_req={prev_req};"));
    }
    if let Some(prompt_id) = last_prompt_id(messages) {
        header.push_str(&format!(" cc_prompt_id={prompt_id};"));
    }
    header
}

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

const CLAUDE_CODE_CAPTURE_TOOL_ORDER: &[&str] = &[
    "Agent",
    "AskUserQuestion",
    "Bash",
    "Edit",
    "Read",
    "ScheduleWakeup",
    "Skill",
    "ToolSearch",
    "Workflow",
    "Write",
];

fn to_claude_code_name(name: &str) -> String {
    let normalised = super::normalize_tool_name(name);
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|t| t.to_lowercase() == normalised)
        .map(|t| t.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn claude_code_capture_tool_order(name: &str) -> Option<usize> {
    let normalised = super::normalize_tool_name(name);
    CLAUDE_CODE_CAPTURE_TOOL_ORDER
        .iter()
        .position(|tool| super::normalize_tool_name(tool) == normalised)
}

fn from_claude_code_name(name: &str, tools: &[ToolDefinition]) -> String {
    let normalised = super::normalize_tool_name(name);
    tools
        .iter()
        .find(|t| super::normalize_tool_name(&t.name) == normalised)
        .map(|t| t.name.clone())
        .unwrap_or_else(|| name.to_string())
}

pub struct AnthropicProvider {
    pub client: crate::http::HttpClient,
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
        let mut api_key = options
            .api_key
            .clone()
            .or_else(anthropic_api_key)
            .ok_or_else(|| {
                crate::auth::missing_key_error(&Provider::Anthropic, options.oauth_account.as_ref())
            })?;

        if api_key.is_oauth_token()
            && let Some(fresh) =
                super::stored_oauth_key("anthropic", options.oauth_account.as_ref()).await
        {
            api_key = fresh;
        }

        let is_oauth = api_key.is_oauth_token();

        let body = build_request_body(
            model,
            &context.system_prompt,
            &context.messages,
            &context.tools,
            options,
            is_oauth,
        );

        let mut headers = build_request_headers(model, options, &api_key, is_oauth)?;

        let base_url = if model.base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            &model.base_url
        };
        let url = format!("{base_url}/v1/messages");
        let url = if is_oauth {
            format!("{url}?beta=true")
        } else {
            url
        };
        let call_id = crate::wire_log::next_call_id();
        let body_json = serde_json::to_string(&body).ok();
        if let Some(body_json) = &body_json {
            record_request_snapshot(&body, body_json, &headers, model, options, is_oauth);
        }

        tracing::debug!(model = %model.id, %url, oauth = is_oauth, "sending anthropic request");

        let send = |headers| {
            send_request(
                &self.client,
                &url,
                headers,
                &body,
                body_json.as_deref(),
                call_id,
            )
        };
        let response = send(headers.clone()).await?;

        let response = if response.status().as_u16() == 401 && is_oauth {
            tracing::warn!(
                model = %model.id,
                "anthropic returned 401 with oauth token, forcing refresh + single retry"
            );
            match super::refresh_rejected_oauth_key(
                "anthropic",
                options.oauth_account.as_ref(),
                &api_key,
            )
            .await?
            {
                Some(fresh) => {
                    if let Ok(auth) = super::bearer_header(fresh.expose()) {
                        headers.insert("authorization", auth);
                    }
                    headers.insert("x-stainless-retry-count", HeaderValue::from_static("1"));
                    send(headers).await?
                }
                None => response,
            }
        } else {
            response
        };

        let response =
            super::check_response(response, "anthropic", model.id.as_str(), &url).await?;

        Ok(parse_sse_stream(
            response,
            model,
            call_id,
            is_oauth,
            context.tools.clone(),
        ))
    }
}

async fn send_request(
    client: &crate::http::HttpClient,
    url: &str,
    headers: HeaderMap,
    body: &RequestBody,
    body_json: Option<&str>,
    call_id: u64,
) -> Result<reqwest::Response, ProviderError> {
    match body_json {
        Some(body_json) => {
            super::post_json_text(client, url, headers, body_json.to_owned(), call_id).await
        }
        None => super::post_json(client, url, headers, body, call_id).await,
    }
}

fn record_request_snapshot(
    body: &RequestBody,
    body_json: &str,
    headers: &HeaderMap,
    model: &Model,
    options: &StreamOptions,
    is_oauth: bool,
) {
    let system_json = body
        .system
        .as_ref()
        .and_then(|s| serde_json::to_string(s).ok())
        .unwrap_or_default();
    let tools_json = body
        .tools
        .as_ref()
        .and_then(|t| serde_json::to_string(t).ok())
        .unwrap_or_default();
    let fp = messages_fingerprint(&system_json, &tools_json, &body.messages);
    tracing::info!(
        system = fp.system,
        tools = fp.tools,
        messages_prefix = fp.messages_prefix,
        last_user = fp.last_user,
        messages_tail = fp.messages_tail,
        "anthropic request fingerprint"
    );
    let session = options.session_id.as_ref().map(|id| id.as_str());
    let snapshot = crate::request_snapshot::dump(
        "anthropic",
        session,
        &fp,
        body_json,
        &crate::http::header_pairs(headers),
    );
    let ttl_secs =
        anthropic_cache_control(model.base_url.as_str(), options.cache_retention, is_oauth)
            .map_or(0, |cc| cc.ttl_secs());
    crate::request_snapshot::record_request(
        session.unwrap_or(""),
        fp,
        body.context_management.is_some(),
        ttl_secs,
        snapshot,
    );
}

#[derive(Serialize)]
struct RequestBody {
    model: String,
    messages: Vec<RequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<RequestTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<RequestMetadata>,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<ContextManagement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Diagnostics>,
    stream: bool,
}

#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    block_type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl SystemBlock {
    fn text(text: String, cache_control: Option<CacheControl>) -> Self {
        Self {
            block_type: "text",
            text,
            cache_control,
        }
    }
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

impl CacheControl {
    fn ttl_secs(&self) -> u16 {
        if self.ttl.as_deref() == Some("1h") {
            3600
        } else {
            300
        }
    }
}

#[derive(Debug, Serialize)]
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
        #[serde(skip_serializing_if = "String::is_empty")]
        display: String,
    },
}

fn messages_fingerprint(
    system_json: &str,
    tools_json: &str,
    messages: &[RequestMessage],
) -> crate::request_snapshot::RequestFingerprint {
    fn json(messages: &[RequestMessage]) -> String {
        let sorted: Vec<_> = messages
            .iter()
            .map(RequestMessage::with_sorted_keys)
            .collect();
        serde_json::to_string(&sorted).unwrap_or_default()
    }
    let (prefix, last_user, tail) = match messages.iter().rposition(RequestMessage::opens_user_turn)
    {
        Some(index) => (
            json(&messages[..index]),
            serde_json::to_string(&messages[index].with_sorted_keys()).unwrap_or_default(),
            json(&messages[index + 1..]),
        ),
        None => (
            serde_json::to_string(messages).unwrap_or_default(),
            String::new(),
            String::new(),
        ),
    };
    crate::request_snapshot::RequestFingerprint::from_split_sections(
        system_json,
        tools_json,
        &prefix,
        &last_user,
        &tail,
    )
}

#[derive(Serialize)]
struct SortedRequestMessage<'a> {
    content: &'a serde_json::Value,
    role: &'static str,
}

impl RequestMessage {
    fn with_sorted_keys(&self) -> SortedRequestMessage<'_> {
        SortedRequestMessage {
            content: &self.content,
            role: self.role,
        }
    }

    fn opens_user_turn(&self) -> bool {
        self.role == "user"
            && !self.content.as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
                })
            })
    }
}

#[derive(Serialize)]
struct RequestMessage {
    role: &'static str,
    content: serde_json::Value,
}

#[derive(Serialize)]
#[serde(untagged)]
enum RequestTool {
    Client {
        name: String,
        description: String,
        input_schema: serde_json::Value,
        eager_input_streaming: bool,
    },
    Advisor {
        #[serde(rename = "type")]
        tool_type: &'static str,
        name: &'static str,
        model: &'static str,
        defer_loading: bool,
    },
}

#[derive(Serialize)]
struct OutputConfig {
    effort: String,
}

#[derive(Serialize)]
struct RequestMetadata {
    user_id: String,
}

#[derive(Serialize)]
struct ContextManagement {
    edits: Vec<ContextManagementEdit>,
}

#[derive(Serialize)]
struct ContextManagementEdit {
    #[serde(rename = "type")]
    edit_type: &'static str,
    keep: &'static str,
}

#[derive(Serialize)]
struct Diagnostics {
    previous_message_id: Option<String>,
}

fn build_request_body(
    model: &Model,
    system_prompt: &Option<String>,
    messages: &[Message],
    tools: &[ToolDefinition],
    options: &StreamOptions,
    is_oauth: bool,
) -> RequestBody {
    let max_tokens = TokenCount::new(request_max_tokens(model, options, is_oauth));
    let cache_control =
        anthropic_cache_control(model.base_url.as_str(), options.cache_retention, is_oauth);

    let system_prompt_cache_control = cache_control.clone().map(|mut cc| {
        if is_oauth {
            cc.scope = Some("global".into());
        }
        cc
    });

    let mut system_blocks = Vec::new();
    if is_oauth || model.base_url.contains(ANTHROPIC_DIRECT_API) {
        system_blocks.push(SystemBlock::text(
            claude_code_billing_header(messages),
            None,
        ));
    }
    if is_oauth {
        system_blocks.push(SystemBlock::text(CLAUDE_CODE_IDENTITY.into(), None));
    }
    if let Some(prompt) = system_prompt {
        system_blocks.push(SystemBlock::text(
            prompt.clone(),
            system_prompt_cache_control,
        ));
    }
    let system = (!system_blocks.is_empty()).then_some(system_blocks);

    let trimming = if cache_control.is_some() {
        ToolResultTrimming::Preserve
    } else {
        ToolResultTrimming::SlidingWindow
    };
    let converted_messages = convert_messages(
        &super::replayable_history(messages, model),
        is_oauth,
        cache_control.clone(),
        trimming,
    );

    let converted_tools = convert_tools(
        tools,
        is_oauth,
        oauth_advisor_model(model, messages, tools, options, is_oauth),
    );

    let (thinking, effective_max_tokens) = match &options.thinking {
        Some(level) if *level != ThinkingLevel::Off && model.reasoning => {
            if model.supports_adaptive_thinking && !options.force_budget_thinking {
                let adaptive = ThinkingConfig::Adaptive {
                    config_type: "adaptive".into(),
                    display: thinking_display(options, is_oauth).into(),
                };
                (Some(adaptive), max_tokens.get())
            } else {
                let (max_tokens, budget) =
                    budget_thinking_tokens(level, max_tokens.get(), model.max_output_tokens.get());
                let enabled = ThinkingConfig::Enabled {
                    config_type: "enabled".into(),
                    budget_tokens: budget,
                    display: "summarized".into(),
                };
                (Some(enabled), max_tokens)
            }
        }
        _ => (None, max_tokens.get()),
    };
    let output_config = if accepts_effort(model) {
        super::requested_effort(options)
            .as_ref()
            .and_then(|effort| anthropic_effort(effort, model))
            .map(|effort| OutputConfig { effort })
    } else {
        None
    };

    let temperature = if thinking.is_none() {
        options.temperature.map(|t| t.value())
    } else {
        None
    };

    let metadata = is_oauth.then(|| claude_code_metadata(options));
    let context_management = oauth_context_management(options, is_oauth, thinking.is_some());
    let diagnostics = is_oauth.then(|| Diagnostics {
        previous_message_id: messages.iter().rev().find_map(|message| match message {
            Message::Assistant(assistant) => assistant.generation_id.clone(),
            _ => None,
        }),
    });

    RequestBody {
        model: model.id.to_string(),
        messages: converted_messages,
        system,
        tools: converted_tools,
        metadata,
        max_tokens: effective_max_tokens,
        temperature,
        thinking,
        context_management,
        output_config,
        diagnostics,
        stream: true,
    }
}

fn build_request_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &ApiKey,
    is_oauth: bool,
) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(if is_oauth {
            "application/json"
        } else {
            "text/event-stream"
        }),
    );
    headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));

    if is_oauth {
        let betas = options
            .anthropic_betas
            .clone()
            .unwrap_or_default()
            .gated_for_context_window(model.context_window);
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_str(&betas.to_header_value())?,
        );
        headers.insert(
            "user-agent",
            HeaderValue::from_str(&claude_cli_user_agent())?,
        );
        headers.insert("x-app", HeaderValue::from_static("cli"));
        headers.insert(
            "x-claude-code-request-class",
            HeaderValue::from_static("main"),
        );
        headers.insert(
            "anthropic-dangerous-direct-browser-access",
            HeaderValue::from_static("true"),
        );
        headers.insert(
            "x-client-request-id",
            HeaderValue::from_str(&random_uuid_v4())?,
        );
        headers.insert("x-stainless-arch", HeaderValue::from_static("x64"));
        headers.insert("x-stainless-lang", HeaderValue::from_static("js"));
        headers.insert("x-stainless-os", HeaderValue::from_static("Linux"));
        headers.insert(
            "x-stainless-package-version",
            HeaderValue::from_static("0.112.1"),
        );
        headers.insert("x-stainless-retry-count", HeaderValue::from_static("0"));
        headers.insert("x-stainless-runtime", HeaderValue::from_static("node"));
        headers.insert(
            "x-stainless-runtime-version",
            HeaderValue::from_static("v26.3.0"),
        );
        headers.insert("x-stainless-timeout", HeaderValue::from_static("600"));
        if let Some(sid) = &options.session_id
            && let Ok(hv) = HeaderValue::from_str(sid.as_ref())
        {
            headers.insert("x-claude-code-session-id", hv);
        }
        headers.insert("authorization", super::bearer_header(api_key.expose())?);
    } else {
        let key = api_key.expose();
        if !model.supports_adaptive_thinking {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_static("interleaved-thinking-2025-05-14"),
            );
        }
        let mut api_key_hv = HeaderValue::from_str(key)?;
        api_key_hv.set_sensitive(true);
        headers.insert("x-api-key", api_key_hv);
    }

    Ok(headers)
}

fn thinking_display(options: &StreamOptions, is_oauth: bool) -> &'static str {
    if !is_oauth {
        return "summarized";
    }
    let updates = options
        .anthropic_betas
        .as_ref()
        .is_none_or(|betas| betas.thinking_updates);
    if updates { "updates" } else { "" }
}

fn oauth_context_management(
    options: &StreamOptions,
    is_oauth: bool,
    has_thinking: bool,
) -> Option<ContextManagement> {
    let enabled = options
        .anthropic_betas
        .as_ref()
        .is_none_or(|betas| betas.context_management);
    if !is_oauth || !enabled || !has_thinking {
        return None;
    }
    Some(ContextManagement {
        edits: vec![ContextManagementEdit {
            edit_type: "clear_thinking_20251015",
            keep: "all",
        }],
    })
}

fn convert_tools(
    tools: &[ToolDefinition],
    is_oauth: bool,
    advisor_model: Option<&'static str>,
) -> Option<Vec<RequestTool>> {
    if tools.is_empty() && advisor_model.is_none() {
        return is_oauth.then(Vec::new);
    }

    let mut converted: Vec<(usize, Option<usize>, RequestTool)> = tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let name = if is_oauth {
                to_claude_code_name(&tool.name)
            } else {
                tool.name.clone()
            };
            (
                index,
                is_oauth
                    .then(|| claude_code_capture_tool_order(&name))
                    .flatten(),
                RequestTool::Client {
                    name,
                    description: tool.description.clone(),
                    input_schema: tool.parameters.clone(),
                    eager_input_streaming: true,
                },
            )
        })
        .collect();

    if let Some(model) = advisor_model {
        converted.push((
            usize::MAX,
            None,
            RequestTool::Advisor {
                tool_type: ADVISOR_TOOL_TYPE,
                name: ADVISOR_TOOL_NAME,
                model,
                defer_loading: true,
            },
        ));
    }

    if is_oauth {
        converted
            .sort_by_key(|(index, capture_order, _)| (capture_order.unwrap_or(usize::MAX), *index));
    }

    Some(converted.into_iter().map(|(_, _, tool)| tool).collect())
}

fn oauth_advisor_model(
    model: &Model,
    messages: &[Message],
    tools: &[ToolDefinition],
    options: &StreamOptions,
    is_oauth: bool,
) -> Option<&'static str> {
    if !is_oauth {
        return None;
    }
    if !options
        .anthropic_betas
        .as_ref()
        .is_none_or(|betas| betas.advisor)
    {
        return None;
    }
    let has_advisor_transcript = messages_contain_advisor_blocks(messages);
    if !has_advisor_transcript && !should_offer_advisor(model, tools, options) {
        return None;
    }
    advisor_model_for_executor(&model.id)
}

fn should_offer_advisor(model: &Model, tools: &[ToolDefinition], options: &StreamOptions) -> bool {
    !tools.is_empty()
        && is_lower_cost_advisor_executor(&model.id)
        && matches!(
            super::requested_effort(options),
            Some(EffortLevel::High | EffortLevel::Xhigh | EffortLevel::Other(_))
        )
}

fn is_lower_cost_advisor_executor(model_id: &str) -> bool {
    matches!(
        model_id,
        "claude-haiku-4-5" | "claude-haiku-4-5-20251001" | "claude-sonnet-4-6"
    )
}

fn messages_contain_advisor_blocks(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::Assistant(assistant) => assistant.content.iter().any(|part| {
            matches!(
                part,
                AssistantContentPart::ServerToolUse(ServerToolUse { name, .. })
                    if name.as_str() == ADVISOR_TOOL_NAME
            ) || matches!(part, AssistantContentPart::AdvisorToolResult(_))
        }),
        _ => false,
    })
}

fn advisor_model_for_executor(model_id: &str) -> Option<&'static str> {
    match model_id {
        "claude-haiku-4-5"
        | "claude-haiku-4-5-20251001"
        | "claude-sonnet-4-6"
        | "claude-opus-4-6"
        | "claude-opus-4-7"
        | "claude-opus-4-8" => Some(DEFAULT_ADVISOR_MODEL),
        "claude-fable-5" => Some("claude-fable-5"),
        "claude-mythos-5" => Some("claude-mythos-5"),
        _ => None,
    }
}

fn claude_code_metadata(options: &StreamOptions) -> RequestMetadata {
    #[derive(Serialize)]
    struct UserId {
        device_id: String,
        account_uuid: String,
        session_id: String,
    }

    let user_id = UserId {
        device_id: claude_code_device_id(),
        account_uuid: options.account_id.clone().unwrap_or_default(),
        session_id: options
            .session_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default(),
    };

    RequestMetadata {
        user_id: serde_json::to_string(&user_id).unwrap_or_else(|_| "{}".into()),
    }
}

fn claude_code_device_id() -> String {
    let path = claude_code_device_id_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let id = existing.trim();
        if is_hex_len(id, 64) {
            return id.to_string();
        }
    }

    let id = lower_hex(&random_bytes::<32>());
    let _ = crate::private_io::write_atomic(&path, &id);
    id
}

fn claude_code_device_id_path() -> PathBuf {
    crate::dirs::data_dir().join("claude-code-device-id")
}

fn is_hex_len(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

pub(crate) fn sent_max_tokens(model: &Model, options: &StreamOptions, is_oauth: bool) -> u64 {
    let max_tokens = request_max_tokens(model, options, is_oauth);
    match &options.thinking {
        Some(level)
            if *level != ThinkingLevel::Off
                && model.reasoning
                && !(model.supports_adaptive_thinking && !options.force_budget_thinking) =>
        {
            budget_thinking_tokens(level, max_tokens, model.max_output_tokens.get()).0
        }
        _ => max_tokens,
    }
}

fn request_max_tokens(model: &Model, options: &StreamOptions, is_oauth: bool) -> u64 {
    let requested = if is_oauth {
        options
            .max_tokens
            .map_or(CLAUDE_CODE_MAX_OUTPUT_TOKENS, TokenCount::get)
    } else {
        super::effective_max_output_tokens(model, options)
    };
    match model.max_output_tokens.get() {
        0 => requested,
        limit => requested.min(limit),
    }
}

fn effort_ladder(model: &Model) -> impl Iterator<Item = &ThinkingLevel> {
    model
        .supported_thinking_levels
        .iter()
        .filter(|level| level.effort_name().is_some())
}

fn accepts_effort(model: &Model) -> bool {
    model.reasoning && (model.supports_adaptive_thinking || effort_ladder(model).next().is_some())
}

fn anthropic_effort(effort: &EffortLevel, model: &Model) -> Option<String> {
    if effort_ladder(model).next().is_none() {
        let clamped = match effort {
            EffortLevel::Minimal | EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            _ => "high",
        };
        return Some(clamped.to_string());
    }
    let requested = ThinkingLevel::from_effort_str(effort.name());
    let rank = |level: &ThinkingLevel| {
        ThinkingLevel::LADDER
            .iter()
            .position(|rung| rung == level)
            .unwrap_or(ThinkingLevel::LADDER.len())
    };
    effort_ladder(model)
        .find(|level| **level == requested)
        .or_else(|| {
            effort_ladder(model)
                .filter(|level| rank(level) <= rank(&requested))
                .max_by_key(|level| rank(level))
        })
        .or_else(|| effort_ladder(model).min_by_key(|level| rank(level)))
        .and_then(ThinkingLevel::effort_name)
        .map(str::to_string)
}

const MIN_THINKING_TOKENS: u64 = 1024;

fn thinking_budget(level: &ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Other(_) => 16_384,
    }
}

fn budget_thinking_tokens(level: &ThinkingLevel, base: u64, model_max: u64) -> (u64, u64) {
    let budget = thinking_budget(level).max(MIN_THINKING_TOKENS);
    let wanted = base + budget;
    let max_tokens = if model_max == 0 {
        wanted
    } else {
        wanted.min(model_max)
    };
    let budget = if max_tokens <= budget {
        max_tokens
            .saturating_sub(MIN_THINKING_TOKENS)
            .max(MIN_THINKING_TOKENS)
    } else {
        budget
    };
    (max_tokens, budget)
}

fn convert_messages(
    messages: &[Message],
    is_oauth: bool,
    cache_control: Option<CacheControl>,
    trimming: ToolResultTrimming,
) -> Vec<RequestMessage> {
    let converted = convert_messages_raw(messages, is_oauth, trimming);
    let mut fixed = fix_orphaned_tool_calls(converted);
    apply_cache_control_to_last_cacheable_message(&mut fixed, cache_control);
    fixed
}

fn convert_messages_raw(
    messages: &[Message],
    is_oauth: bool,
    trimming: ToolResultTrimming,
) -> Vec<RequestMessage> {
    let mut visitor = AnthropicVisitor {
        is_oauth,
        trimming,
        result: Vec::new(),
    };
    walk_messages(messages, &mut visitor);
    visitor.result
}

struct AnthropicVisitor {
    is_oauth: bool,
    trimming: ToolResultTrimming,
    result: Vec<RequestMessage>,
}

impl MessageVisitor for AnthropicVisitor {
    fn on_user(&mut self, user: &UserMessage) {
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
                        UserContentPart::Image(img) => Some(image_block(img)),
                    })
                    .collect();
                serde_json::Value::Array(blocks)
            }
        };
        self.result.push(RequestMessage {
            role: "user",
            content,
        });
    }

    fn on_assistant(&mut self, asst: &AssistantMessage) {
        let is_oauth = self.is_oauth;
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
                    } => match (thinking.is_empty(), signature) {
                        (true, None) => None,
                        (_, Some(sig)) => Some(serde_json::json!({
                            "type": "thinking",
                            "thinking": thinking,
                            "signature": sig,
                        })),
                        (false, None) => Some(serde_json::json!({
                            "type": "text",
                            "text": thinking,
                        })),
                    },
                },
                AssistantContentPart::ToolCall(tc) => {
                    let name = if is_oauth {
                        to_claude_code_name(tc.name.as_str())
                    } else {
                        tc.name.as_str().to_string()
                    };
                    let id = anthropic_tool_id(tc.id.as_str());
                    let mut block = serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": tc.arguments,
                    });
                    if is_oauth && let Some(obj) = block.as_object_mut() {
                        obj.insert("caller".into(), serde_json::json!({"type": "direct"}));
                    }
                    Some(block)
                }
                AssistantContentPart::ServerToolUse(server_tool) => Some(serde_json::json!({
                    "type": "server_tool_use",
                    "id": server_tool.id.as_str(),
                    "name": server_tool.name.as_str(),
                    "input": server_tool.input,
                })),
                AssistantContentPart::AdvisorToolResult(result) => Some(serde_json::json!({
                    "type": "advisor_tool_result",
                    "tool_use_id": result.tool_use_id.as_str(),
                    "content": result.content,
                })),
            })
            .collect();

        if !blocks.is_empty() {
            self.result.push(RequestMessage {
                role: "assistant",
                content: serde_json::Value::Array(blocks),
            });
        }
    }

    fn on_tool_result(&mut self, tr: &ToolResultMessage, is_old_turn: bool) {
        let content_blocks = if self.is_oauth {
            claude_code_tool_result_content(tr, is_old_turn, self.trimming)
        } else {
            tool_result_blocks(&tr.content, is_old_turn, self.trimming)
        };

        let content = serde_json::json!([{
            "type": "tool_result",
            "tool_use_id": anthropic_tool_id(tr.tool_call_id.as_str()),
            "content": content_blocks,
            "is_error": tr.outcome.is_error(),
        }]);
        self.result.push(RequestMessage {
            role: "user",
            content,
        });
    }
}

fn claude_code_tool_result_content(
    tr: &ToolResultMessage,
    is_old_turn: bool,
    trimming: ToolResultTrimming,
) -> serde_json::Value {
    match tr.content.as_slice() {
        [] => serde_json::Value::String(String::new()),
        [ToolResultContentPart::Text(text)] => serde_json::Value::String(
            maybe_trim_tool_output(&text.text, is_old_turn, trimming).into_owned(),
        ),
        parts => tool_result_blocks(parts, is_old_turn, trimming),
    }
}

fn tool_result_blocks(
    parts: &[ToolResultContentPart],
    is_old_turn: bool,
    trimming: ToolResultTrimming,
) -> serde_json::Value {
    parts
        .iter()
        .map(|part| match part {
            ToolResultContentPart::Text(t) => serde_json::json!({
                "type": "text",
                "text": maybe_trim_tool_output(&t.text, is_old_turn, trimming),
            }),
            ToolResultContentPart::Image(img) => image_block(img),
        })
        .collect()
}

fn image_block(img: &ImageContent) -> serde_json::Value {
    serde_json::json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": img.mime_type,
            "data": img.data,
        }
    })
}

fn anthropic_tool_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut changed = false;
    for byte in id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') {
            out.push(byte as char);
        } else {
            out.push('_');
            changed = true;
        }
    }

    if out.is_empty() {
        out.push_str("tool_call");
        changed = true;
    }

    if changed {
        out.push('_');
        out.push_str(&lower_hex(&Sha256::digest(id.as_bytes())[..4]));
    }

    out
}

fn fix_orphaned_tool_calls(messages: Vec<RequestMessage>) -> Vec<RequestMessage> {
    let mut result = Vec::with_capacity(messages.len());
    let mut pending_ids = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for msg in messages {
        let blocks = msg
            .content
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let is_tool_result = blocks
            .iter()
            .any(|block| block_type(block) == Some("tool_result"));
        match msg.role {
            "assistant" => {
                inject_synthetic_results(&mut result, &mut pending_ids, &mut seen_ids);
                pending_ids.extend(
                    blocks
                        .iter()
                        .filter(|block| block_type(block) == Some("tool_use"))
                        .filter_map(|block| block.get("id")?.as_str())
                        .map(str::to_string),
                );
            }
            "user" if is_tool_result => {
                let id = blocks
                    .first()
                    .and_then(|block| block.get("tool_use_id")?.as_str());
                match id {
                    Some(id) if pending_ids.iter().any(|pending| pending == id) => {
                        seen_ids.insert(id.to_string());
                    }
                    Some(id) => {
                        tracing::warn!(
                            tool_use_id = id,
                            "stripping orphaned tool_result (no matching tool_use)"
                        );
                        continue;
                    }
                    None => {}
                }
            }
            "user" => inject_synthetic_results(&mut result, &mut pending_ids, &mut seen_ids),
            _ => {}
        }
        result.push(msg);
    }

    inject_synthetic_results(&mut result, &mut pending_ids, &mut seen_ids);
    result
}

fn block_type(block: &serde_json::Value) -> Option<&str> {
    block.get("type")?.as_str()
}

fn apply_cache_control_to_last_cacheable_message(
    messages: &mut [RequestMessage],
    cache_control: Option<CacheControl>,
) {
    let Some(cache) = cache_control else {
        return;
    };

    let Ok(cache_json) = serde_json::to_value(cache) else {
        return;
    };

    for message in messages.iter_mut().rev() {
        match &mut message.content {
            serde_json::Value::Array(blocks) => {
                if let Some(obj) = blocks.iter_mut().rev().find_map(|block| {
                    let obj = block.as_object_mut()?;
                    is_cacheable_message_block(obj).then_some(obj)
                }) {
                    obj.insert("cache_control".into(), cache_json);
                    return;
                }
            }
            serde_json::Value::String(text) if !text.trim().is_empty() => {
                let text = std::mem::take(text);
                message.content = serde_json::json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": cache_json,
                }]);
                return;
            }
            _ => {}
        }
    }
}

fn is_cacheable_message_block(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    match obj.get("type").and_then(|kind| kind.as_str()) {
        Some("thinking" | "redacted_thinking") | None => false,
        Some("text") => obj
            .get("text")
            .and_then(|text| text.as_str())
            .is_some_and(|text| !text.is_empty()),
        Some(_) => true,
    }
}

fn inject_synthetic_results(
    result: &mut Vec<RequestMessage>,
    pending_ids: &mut Vec<String>,
    seen_ids: &mut std::collections::HashSet<String>,
) {
    for id in pending_ids.drain(..) {
        if !seen_ids.contains(&id) {
            result.push(RequestMessage {
                role: "user",
                content: serde_json::json!([{
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": [{"type": "text", "text": "no result provided"}],
                    "is_error": true,
                }]),
            });
        }
    }
    seen_ids.clear();
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
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

impl SseEvent {
    fn ends_message(&self) -> bool {
        match self {
            Self::MessageDelta { delta, .. } => delta.stop_reason.is_some(),
            Self::MessageStop | Self::Error { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct MessageStartData {
    #[serde(default)]
    id: Option<String>,
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
    #[serde(rename = "server_tool_use")]
    ServerToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    #[serde(rename = "advisor_tool_result")]
    AdvisorToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum DeltaData {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
    #[serde(rename = "citations_delta")]
    CitationsDelta,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaData {
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_details: Option<StopDetailsData>,
}

#[derive(Debug, Deserialize)]
struct StopDetailsData {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    explanation: Option<String>,
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
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    details: Option<ErrorDetails>,
}

#[derive(Debug, Deserialize)]
struct ErrorDetails {
    #[serde(default)]
    error_code: Option<String>,
}

impl ErrorData {
    fn description(self) -> String {
        [
            self.kind,
            self.details.and_then(|d| d.error_code),
            Some(self.message),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
    }
}

struct AnthropicBlock {
    output_index: Option<usize>,
    block: StreamBlock,
}

struct AnthropicProcessor {
    blocks: Vec<AnthropicBlock>,
    is_oauth: bool,
    tools: Vec<ToolDefinition>,
    ended: bool,
}

impl super::sse::SseProcessor for AnthropicProcessor {
    fn process(
        &mut self,
        raw: &super::sse::SseRawEvent,
        output: &mut AssistantMessage,
    ) -> super::sse::ProcessResult {
        match serde_json::from_str::<SseEvent>(&raw.data) {
            Ok(event) => {
                self.ended |= event.ends_message();
                let events =
                    process_sse_event(event, output, &mut self.blocks, self.is_oauth, &self.tools);
                super::sse::ProcessResult::Events(events)
            }
            Err(_) => super::sse::ProcessResult::Skip,
        }
    }

    fn finish(&mut self, output: &mut AssistantMessage) {
        if !self.ended {
            output.stop_reason = StopReason::Error;
            output.error_message = Some(STREAM_ENDED_EARLY.into());
        }
    }

    fn emit_start(&self) -> bool {
        false
    }

    fn label(&self) -> &'static str {
        "anthropic"
    }
}

fn parse_sse_stream(
    response: reqwest::Response,
    model: &Model,
    call_id: u64,
    is_oauth: bool,
    tools: Vec<ToolDefinition>,
) -> EventStream {
    let processor = AnthropicProcessor {
        blocks: Vec::new(),
        is_oauth,
        tools,
        ended: false,
    };
    super::sse::run_sse_stream(response, model, call_id, processor)
}

fn process_sse_event(
    event: SseEvent,
    output: &mut AssistantMessage,
    blocks: &mut Vec<AnthropicBlock>,
    is_oauth: bool,
    tools: &[ToolDefinition],
) -> Vec<StreamEvent> {
    let mut events = Vec::new();

    match event {
        SseEvent::MessageStart { message } => {
            if let Some(id) = message.id {
                output.generation_id = Some(id);
            }
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
            index,
            content_block,
        } => start_block(
            index,
            content_block,
            output,
            blocks,
            is_oauth,
            tools,
            &mut events,
        ),
        SseEvent::ContentBlockDelta { index, delta } => {
            if let Some(block) = blocks.get_mut(index) {
                apply_delta(block, delta, output, &mut events);
            }
        }
        SseEvent::ContentBlockStop { index } => {
            if let Some(block) = blocks.get(index) {
                stop_block(block, output, &mut events);
            }
        }
        SseEvent::MessageDelta { delta, usage } => {
            output.usage.output_tokens = TokenCount::new(usage.output_tokens);
            if let Some(reason) = delta.stop_reason {
                let (stop_reason, message) = map_stop_reason(&reason, delta.stop_details.as_ref());
                output.stop_reason = stop_reason;
                if let Some(message) = message {
                    tracing::warn!(stop_reason = %reason, %message, "anthropic ended the turn");
                    output.error_message = Some(message);
                }
            }
        }
        SseEvent::MessageStop => {}
        SseEvent::Ping => {}
        SseEvent::Error { error } => {
            tracing::error!(error = %error.message, "anthropic SSE error event");
            output.stop_reason = StopReason::Error;
            output.error_message = Some(error.description());
        }
    }

    events
}

fn start_block(
    index: usize,
    content_block: ContentBlockData,
    output: &mut AssistantMessage,
    blocks: &mut Vec<AnthropicBlock>,
    is_oauth: bool,
    tools: &[ToolDefinition],
    events: &mut Vec<StreamEvent>,
) {
    if blocks.len() <= index {
        blocks.resize_with(index + 1, || AnthropicBlock {
            output_index: None,
            block: StreamBlock::ServerSideTranscript,
        });
    }
    let output_index = output.content.len();
    let block = &mut blocks[index];
    block.output_index =
        (!matches!(content_block, ContentBlockData::Unknown)).then_some(output_index);
    match content_block {
        ContentBlockData::Text { text } => {
            block.block = StreamBlock::Text { text: text.clone() };
            output
                .content
                .push(AssistantContentPart::Text(TextContent { text }));
            events.push(StreamEvent::TextStart {
                content_index: output_index,
            });
        }
        ContentBlockData::Thinking { thinking } => {
            block.block = StreamBlock::Thinking {
                text: thinking.clone(),
                signature: None,
            };
            output
                .content
                .push(AssistantContentPart::Thinking(ThinkingContent::Thinking {
                    thinking,
                    signature: None,
                }));
            events.push(StreamEvent::ThinkingStart {
                content_index: output_index,
            });
        }
        ContentBlockData::RedactedThinking { data } => {
            block.block = StreamBlock::Thinking {
                text: "[reasoning redacted]".into(),
                signature: Some(data.clone()),
            };
            output
                .content
                .push(AssistantContentPart::Thinking(ThinkingContent::Redacted {
                    data,
                }));
            events.push(StreamEvent::ThinkingStart {
                content_index: output_index,
            });
        }
        ContentBlockData::ToolUse { id, name } => {
            let resolved_name = if is_oauth {
                from_claude_code_name(&name, tools)
            } else {
                name.clone()
            };
            block.block = StreamBlock::ToolCall {
                id: id.clone(),
                name: resolved_name.clone(),
                args_buf: String::new(),
            };
            output
                .content
                .push(AssistantContentPart::ToolCall(ToolCall {
                    id: ToolCallId::from(id),
                    name: ToolName::from(resolved_name),
                    arguments: serde_json::Value::Object(Default::default()),
                }));
            events.push(StreamEvent::ToolCallStart {
                content_index: output_index,
            });
        }
        ContentBlockData::ServerToolUse { id, name, input } => {
            block.block = StreamBlock::ServerSideTranscript;
            output
                .content
                .push(AssistantContentPart::ServerToolUse(ServerToolUse {
                    id: ToolCallId::from(id),
                    name: ToolName::from(name),
                    input,
                }));
        }
        ContentBlockData::AdvisorToolResult {
            tool_use_id,
            content,
        } => {
            block.block = StreamBlock::ServerSideTranscript;
            output
                .content
                .push(AssistantContentPart::AdvisorToolResult(AdvisorToolResult {
                    tool_use_id: ToolCallId::from(tool_use_id),
                    content,
                }));
        }
        ContentBlockData::Unknown => {
            block.block = StreamBlock::ServerSideTranscript;
        }
    }
}

fn apply_delta(
    block: &mut AnthropicBlock,
    delta: DeltaData,
    output: &mut AssistantMessage,
    events: &mut Vec<StreamEvent>,
) {
    let Some(content_index) = block.output_index else {
        return;
    };
    match delta {
        DeltaData::TextDelta { text } => {
            if let StreamBlock::Text { text: buf } = &mut block.block {
                buf.push_str(&text);
            }
            if let Some(AssistantContentPart::Text(tc)) = output.content.get_mut(content_index) {
                tc.text.push_str(&text);
            }
            events.push(StreamEvent::TextDelta {
                content_index,
                delta: text,
            });
        }
        DeltaData::ThinkingDelta { thinking } => {
            if let StreamBlock::Thinking { text: buf, .. } = &mut block.block {
                buf.push_str(&thinking);
            }
            if let Some(AssistantContentPart::Thinking(tc)) = output.content.get_mut(content_index)
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
            if let StreamBlock::ToolCall { args_buf, .. } = &mut block.block {
                args_buf.push_str(&partial_json);
            }
            events.push(StreamEvent::ToolCallDelta {
                content_index,
                delta: partial_json,
            });
        }
        DeltaData::SignatureDelta { signature } => {
            if let StreamBlock::Thinking { signature: sig, .. } = &mut block.block {
                sig.get_or_insert_with(String::new).push_str(&signature);
            }
            if let Some(AssistantContentPart::Thinking(tc)) = output.content.get_mut(content_index)
                && let Some(sig) = tc.signature_mut()
            {
                sig.get_or_insert_with(String::new).push_str(&signature);
            }
        }
        DeltaData::CitationsDelta | DeltaData::Unknown => {}
    }
}

fn stop_block(
    block: &AnthropicBlock,
    output: &mut AssistantMessage,
    events: &mut Vec<StreamEvent>,
) {
    let Some(content_index) = block.output_index else {
        return;
    };
    match &block.block {
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
            let arguments = serde_json::from_str(args_buf).unwrap_or_else(|err| {
                tracing::warn!(
                    tool = %name,
                    tool_id = %id,
                    args_len = args_buf.len(),
                    args_preview = %safe_preview_text(args_buf, 240),
                    error = %err,
                    "tool call arguments failed to parse, falling back to empty object"
                );
                serde_json::Value::Object(Default::default())
            });
            if let Some(AssistantContentPart::ToolCall(tc)) = output.content.get_mut(content_index)
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
        StreamBlock::ServerSideTranscript => {}
    }
}

fn map_stop_reason(
    reason: &str,
    details: Option<&StopDetailsData>,
) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => (StopReason::Error, Some(refusal_message(details))),
        "model_context_window_exceeded" => (
            StopReason::Error,
            Some("the response filled the model's context window. compact and retry".into()),
        ),
        other => (
            StopReason::Error,
            Some(format!("unknown stop reason: {other}")),
        ),
    }
}

fn refusal_message(details: Option<&StopDetailsData>) -> String {
    let detail = details.and_then(|d| {
        d.explanation
            .clone()
            .or_else(|| d.category.as_ref().map(|c| format!("category: {c}")))
    });
    match detail {
        Some(detail) => format!("claude declined to respond ({detail}). retry on another model"),
        None => "claude declined to respond. retry on another model".into(),
    }
}

#[doc(hidden)]
pub fn benchmark_tool_call_deltas(chunk_count: usize, arg_bytes: usize) -> usize {
    let (_, fragments) = super::bench_support::tool_call_json_fragments(chunk_count, arg_bytes);
    let mut output = AssistantMessage::builder()
        .model("bench".into())
        .provider(Provider::Custom("bench".into()))
        .api(Api::AnthropicMessages)
        .timestamp_ms(Timestamp::zero())
        .build();
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
        assert_eq!(map_stop_reason("end_turn", None), (StopReason::Stop, None));
        assert_eq!(
            map_stop_reason("max_tokens", None),
            (StopReason::Length, None)
        );
        assert_eq!(
            map_stop_reason("tool_use", None),
            (StopReason::ToolUse, None)
        );
    }

    #[test]
    fn a_refusal_names_the_classifier_category() {
        let details = StopDetailsData {
            category: Some("cyber".into()),
            explanation: None,
        };
        let (reason, message) = map_stop_reason("refusal", Some(&details));
        assert_eq!(reason, StopReason::Error);
        assert!(message.unwrap().contains("cyber"));
    }

    #[test]
    fn a_refusal_prefers_anthropics_own_explanation() {
        let details = StopDetailsData {
            category: Some("cyber".into()),
            explanation: Some("this could enable cyber harm".into()),
        };
        let (_, message) = map_stop_reason("refusal", Some(&details));
        assert_eq!(
            message.unwrap(),
            "claude declined to respond (this could enable cyber harm). retry on another model"
        );
    }

    #[test]
    fn a_refusal_without_a_category_still_says_it_was_a_refusal() {
        let (_, message) = map_stop_reason("refusal", None);
        assert!(message.unwrap().contains("declined"));
    }

    #[test]
    fn a_filled_context_window_says_so() {
        let (reason, message) = map_stop_reason("model_context_window_exceeded", None);
        assert_eq!(reason, StopReason::Error);
        assert!(message.unwrap().contains("context window"));
    }

    #[test]
    fn an_unknown_stop_reason_keeps_its_name() {
        let (reason, message) = map_stop_reason("brand_new_thing", None);
        assert_eq!(reason, StopReason::Error);
        assert!(message.unwrap().contains("brand_new_thing"));
    }

    #[test]
    fn a_message_delta_carries_the_refusal_onto_the_output() {
        let mut output = AssistantMessage::builder()
            .model("test".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();

        process_sse_event(
            SseEvent::MessageDelta {
                delta: MessageDeltaData {
                    stop_reason: Some("refusal".into()),
                    stop_details: Some(StopDetailsData {
                        category: Some("cyber".into()),
                        explanation: None,
                    }),
                },
                usage: UsageDeltaData { output_tokens: 7 },
            },
            &mut output,
            &mut Vec::new(),
            false,
            &[],
        );

        assert_eq!(output.stop_reason, StopReason::Error);
        assert!(
            output.error_message.as_deref().unwrap().contains("cyber"),
            "got {:?}",
            output.error_message
        );
    }

    #[test]
    fn streaming_error_preserves_spend_limit_code() {
        let event: SseEvent = serde_json::from_str(
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit reached","details":{"error_code":"enforced_spend_limit_reached"}}}"#,
        ).unwrap();
        let mut output = AssistantMessage::builder()
            .model("test".into())
            .provider(Provider::Anthropic)
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build();
        process_sse_event(event, &mut output, &mut Vec::new(), false, &[]);
        assert_eq!(
            output.error_message.as_deref(),
            Some("rate_limit_error: enforced_spend_limit_reached: limit reached")
        );
    }

    #[test]
    fn oauth_beta_default_includes_expected_flags() {
        let header = AnthropicBetas::default().to_header_value();
        assert!(header.contains("claude-code-20250219"), "header={header}");
        assert!(header.contains("oauth-2025-04-20"), "header={header}");
        assert!(header.contains("interleaved-thinking-2025-05-14"));
        assert!(header.contains("prompt-caching-scope-2026-01-05"));
        assert!(header.contains("context-1m-2025-08-07"));
        assert!(header.contains("effort-2025-11-24"));
        assert!(header.contains("context-management-2025-06-27"));
        assert!(header.contains("thinking-token-count-2026-05-13"));
        assert!(header.contains("thinking-display-updates-2026-08-18"));
        assert!(header.contains("advisor-tool-2026-03-01"));
        assert!(header.contains("advanced-tool-use-2025-11-20"));
    }

    #[test]
    fn oauth_beta_toggles_respect_config() {
        let betas = AnthropicBetas {
            context_1m: false,
            effort: false,
            context_management: false,
            ..AnthropicBetas::default()
        };
        let header = betas.to_header_value();
        assert!(!header.contains("context-1m-2025-08-07"));
        assert!(!header.contains("effort-2025-11-24"));
        assert!(!header.contains("context-management-2025-06-27"));
        assert!(header.contains("thinking-token-count-2026-05-13"));
        assert!(header.contains("advisor-tool-2026-03-01"));
        assert!(header.contains("advanced-tool-use-2025-11-20"));
    }

    #[test]
    fn thinking_budget_levels() {
        assert_eq!(thinking_budget(&ThinkingLevel::Off), 0);
        assert_eq!(thinking_budget(&ThinkingLevel::Minimal), 1024);
        assert_eq!(thinking_budget(&ThinkingLevel::Low), 2048);
        assert_eq!(thinking_budget(&ThinkingLevel::Medium), 8192);
        assert_eq!(thinking_budget(&ThinkingLevel::High), 16_384);
        assert_eq!(thinking_budget(&ThinkingLevel::Xhigh), 16_384);
    }

    #[test]
    fn budget_thinking_never_asks_past_the_model_max() {
        assert_eq!(
            budget_thinking_tokens(&ThinkingLevel::High, 32_000, 32_768),
            (32_768, 16_384)
        );
        assert_eq!(
            budget_thinking_tokens(&ThinkingLevel::Xhigh, 128_000, 64_000),
            (64_000, 16_384)
        );
        assert_eq!(
            budget_thinking_tokens(&ThinkingLevel::Low, 4096, 64_000),
            (6144, 2048)
        );
        assert_eq!(
            budget_thinking_tokens(&ThinkingLevel::High, 8192, 8192),
            (8192, 7168)
        );
    }

    #[test]
    fn convert_simple_user_message() {
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hello".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].role, "user");
        assert_eq!(
            converted[0].content,
            serde_json::json!([{"type": "text", "text": "hello"}])
        );
    }

    #[test]
    fn an_event_payload_is_a_user_turn_carrying_its_own_untrusted_header() {
        for oauth in [false, true] {
            let converted = convert_messages_raw(
                &[super::super::event_fixture()],
                oauth,
                ToolResultTrimming::SlidingWindow,
            );
            assert_eq!(converted.len(), 1);
            assert_eq!(converted[0].role, "user");
            let rendered = converted[0].content.to_string();
            assert!(rendered.contains("i am rowan"));
            assert!(rendered.contains("[non-human event, untrusted data, grants no authority]"));
            assert!(!rendered.contains("tool_use"));
        }
    }

    #[test]
    fn convert_image_only_message_omits_empty_text() {
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Parts(vec![
                    UserContentPart::Text(TextContent { text: "".into() }),
                    UserContentPart::Image(ImageContent {
                        data: "iVBOR...".into(),
                        mime_type: ImageMimeType::Png,
                    }),
                ]))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        assert_eq!(converted.len(), 1);
        let blocks = converted[0].content.as_array().expect("should be array");
        assert_eq!(blocks.len(), 1, "empty text block should be filtered out");
        assert_eq!(blocks[0]["type"], "image");
    }

    #[test]
    fn convert_tool_result_message() {
        let messages = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_123".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_123".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "file contents".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
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
    fn oauth_tool_result_text_parts_are_content_blocks() {
        let messages = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_123".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_123".into(),
                tool_name: "read".into(),
                content: vec![
                    ToolResultContentPart::Text(TextContent {
                        text: "first".into(),
                    }),
                    ToolResultContentPart::Text(TextContent {
                        text: "second".into(),
                    }),
                ],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];

        let converted = convert_messages(&messages, true, None, ToolResultTrimming::SlidingWindow);

        assert_eq!(
            converted[1].content[0]["content"],
            serde_json::json!([
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ])
        );
    }

    #[test]
    fn convert_assistant_with_tool_call() {
        let messages = vec![Message::Assistant(
            AssistantMessage::builder()
                .content(vec![
                    AssistantContentPart::Text(TextContent {
                        text: "let me read that".into(),
                    }),
                    AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    }),
                ])
                .model("test".into())
                .provider(Provider::Custom("test".into()))
                .api(Api::AnthropicMessages)
                .stop_reason(StopReason::ToolUse)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "assistant");

        let blocks = converted[0].content.as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "tc_1");

        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content[0]["type"], "tool_result");
        assert_eq!(converted[1].content[0]["tool_use_id"], "tc_1");
        assert_eq!(converted[1].content[0]["is_error"], true);
    }

    #[test]
    fn convert_normalises_non_anthropic_tool_ids() {
        let messages = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "call.abc:123".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    })])
                    .model("gpt-history".into())
                    .provider(Provider::Custom("openai".into()))
                    .api(Api::OpenaiResponses)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call.abc:123".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "contents".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);

        let id = converted[0].content[0]["id"].as_str().unwrap();
        assert!(id.starts_with("call_abc_123_"), "normalised id: {id}");
        assert_eq!(converted[1].content[0]["tool_use_id"], id);
    }

    #[test]
    fn anthropic_tool_id_avoids_normalisation_collisions() {
        let dot = anthropic_tool_id("call.a");
        let colon = anthropic_tool_id("call:a");

        assert_ne!(dot, colon);
        assert!(dot.starts_with("call_a_"));
        assert!(colon.starts_with("call_a_"));
        assert_eq!(anthropic_tool_id("call_a"), "call_a");
    }

    #[test]
    fn empty_thinking_with_signature_preserves_block() {
        let messages = vec![Message::Assistant(
            AssistantMessage::builder()
                .content(vec![
                    AssistantContentPart::Thinking(ThinkingContent::Thinking {
                        thinking: String::new(),
                        signature: Some("sig_abc123".into()),
                    }),
                    AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    }),
                ])
                .model("test".into())
                .provider(Provider::Custom("test".into()))
                .api(Api::AnthropicMessages)
                .stop_reason(StopReason::ToolUse)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        let blocks = converted[0].content.as_array().unwrap();
        assert_eq!(blocks.len(), 2, "thinking block should be preserved");
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "");
        assert_eq!(blocks[0]["signature"], "sig_abc123");
        assert_eq!(blocks[1]["type"], "tool_use");
    }

    #[test]
    fn empty_thinking_without_signature_is_dropped() {
        let messages = vec![Message::Assistant(
            AssistantMessage::builder()
                .content(vec![
                    AssistantContentPart::Thinking(ThinkingContent::Thinking {
                        thinking: String::new(),
                        signature: None,
                    }),
                    AssistantContentPart::Text(TextContent { text: "hi".into() }),
                ])
                .model("test".into())
                .provider(Provider::Custom("test".into()))
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        let blocks = converted[0].content.as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn thinking_with_text_and_signature_preserves_both() {
        let messages = vec![Message::Assistant(
            AssistantMessage::builder()
                .content(vec![AssistantContentPart::Thinking(
                    ThinkingContent::Thinking {
                        thinking: "let me think about this".into(),
                        signature: Some("sig_xyz".into()),
                    },
                )])
                .model("test".into())
                .provider(Provider::Custom("test".into()))
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        let blocks = converted[0].content.as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "let me think about this");
        assert_eq!(blocks[0]["signature"], "sig_xyz");
    }

    fn signed_history(model: &str, provider: Provider, api: Api, signature: &str) -> Vec<Message> {
        vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("hi".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![
                        AssistantContentPart::Thinking(ThinkingContent::Thinking {
                            thinking: "private chain".into(),
                            signature: Some(signature.into()),
                        }),
                        AssistantContentPart::Thinking(ThinkingContent::Redacted {
                            data: "opaque".into(),
                        }),
                        AssistantContentPart::Thinking(ThinkingContent::Thinking {
                            thinking: " ".into(),
                            signature: Some(signature.into()),
                        }),
                        AssistantContentPart::Text(TextContent {
                            text: "answer".into(),
                        }),
                    ])
                    .model(model.into())
                    .provider(provider)
                    .api(api)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
        ]
    }

    #[test]
    fn foreign_thinking_replays_as_text_without_signatures() {
        let origins = [
            (
                "claude-opus-4-1",
                Provider::Anthropic,
                Api::AnthropicMessages,
                "sig_other_model",
            ),
            (
                "claude-sonnet-4-20250514",
                Provider::OpenRouter,
                Api::OpenaiCompletions,
                "reasoning_content",
            ),
            (
                "anthropic/claude-sonnet-4",
                Provider::OpenRouter,
                Api::OpenaiCompletions,
                r#"{"type":"openai-completions.reasoning_details.v1","details":[{"type":"reasoning.encrypted","data":"enc"}]}"#,
            ),
        ];
        for (id, provider, api, signature) in origins {
            let messages = signed_history(id, provider, api, signature);
            let options = StreamOptions {
                cache_retention: Some(CacheRetention::None),
                ..Default::default()
            };
            let body = build_request_body(&test_model(), &None, &messages, &[], &options, false);
            let wire = serde_json::to_string(&body.messages).unwrap();
            assert!(!wire.contains(signature), "{wire}");
            assert!(!wire.contains("opaque"), "{wire}");
            assert!(!wire.contains("thinking"), "{wire}");
            assert_eq!(
                body.messages[1].content,
                serde_json::json!([
                    {"type": "text", "text": "private chain"},
                    {"type": "text", "text": "answer"},
                ])
            );
        }
    }

    #[test]
    fn the_messages_fingerprint_matches_hashing_the_serialised_messages() {
        let messages = [
            RequestMessage {
                role: "user",
                content: serde_json::json!("first question"),
            },
            RequestMessage {
                role: "assistant",
                content: serde_json::json!([
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls", "cwd": "/"}}
                ]),
            },
            RequestMessage {
                role: "user",
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok \u{e9} \"quoted\""}
                ]),
            },
            RequestMessage {
                role: "user",
                content: serde_json::json!([{"type": "text", "text": "second", "cache_control": {"type": "ephemeral"}}]),
            },
            RequestMessage {
                role: "assistant",
                content: serde_json::json!([{"type": "text", "text": "answer"}]),
            },
        ];
        for messages in [&messages[..], &messages[1..3], &[]] {
            let serialised = serde_json::to_string(messages).unwrap();
            assert_eq!(
                messages_fingerprint("[]", "[]", messages),
                crate::request_snapshot::RequestFingerprint::from_json_sections(
                    "[]",
                    "[]",
                    &serialised
                )
            );
        }
    }

    #[test]
    fn same_model_thinking_keeps_its_signature() {
        let model = test_model();
        let messages = signed_history(
            model.id.as_str(),
            model.provider.clone(),
            model.api,
            "sig_same_model",
        );
        let body = build_request_body(
            &model,
            &None,
            &messages,
            &[],
            &StreamOptions::default(),
            false,
        );
        let blocks = body.messages[1].content.as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["signature"], "sig_same_model");
        assert_eq!(blocks[1]["type"], "redacted_thinking");
        assert_eq!(blocks[1]["data"], "opaque");
    }

    #[test]
    fn tool_use_with_matching_result_no_synthetic() {
        let messages = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "echo hi"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "hi".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content[0]["tool_use_id"], "tc_1");
        assert_eq!(converted[1].content[0]["is_error"], false);
    }

    #[test]
    fn orphaned_tool_use_gets_synthetic_result() {
        let messages = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_orphan".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "rm -rf /"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("stop! undo that".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);
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
        let messages = vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("compacted summary".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_A".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_GONE".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "stale output".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_A".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "file contents".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);

        assert_eq!(converted.len(), 3, "orphaned tool_result should be dropped");
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
        assert_eq!(converted[2].content[0]["tool_use_id"], "tc_A");
    }

    #[test]
    fn orphaned_tool_result_all_blocks_stripped() {
        let messages = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::Text(TextContent {
                        text: "done".into(),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_stale".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "stale".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("continue".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
        ];

        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);

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
        assert!(body.temperature.is_none());
    }

    #[test]
    fn oauth_context_management_matches_claude_code_keep_all() {
        let model = Model {
            id: "claude-opus-4-8".into(),
            name: "Claude Opus 4.8".into(),
            reasoning: true,
            supports_adaptive_thinking: true,
            supported_thinking_levels: vec![ThinkingLevel::High],
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, true);
        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(
            json["context_management"],
            expected_claude_code_context_management()
        );
    }

    #[test]
    fn opus_4_6_uses_adaptive_thinking_with_effort() {
        let model = Model {
            id: "claude-opus-4-6".into(),
            name: "Claude Opus 4.6".into(),
            reasoning: true,
            supports_adaptive_thinking: true,
            supported_thinking_levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Other("max".into()),
            ],
            default_thinking_level: None,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::Other("max".into())),
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
    fn adaptive_thinking_effort_can_differ_from_thinking_level() {
        let model = Model {
            id: "claude-opus-4-8".into(),
            name: "Claude Opus 4.8".into(),
            reasoning: true,
            supports_adaptive_thinking: true,
            supported_thinking_levels: vec![ThinkingLevel::Low, ThinkingLevel::High],
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            effort: Some(EffortLevel::Low),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, true);

        assert!(matches!(
            body.thinking,
            Some(ThinkingConfig::Adaptive { .. })
        ));
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("low")
        );
    }

    #[test]
    fn effort_clamps_to_high_without_a_discovered_ladder() {
        let model = Model {
            id: "claude-opus-4-7".into(),
            name: "Claude Opus 4.7".into(),
            reasoning: true,
            supports_adaptive_thinking: true,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::Xhigh),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("high")
        );
    }

    #[test]
    fn force_budget_overrides_adaptive_capability() {
        let model = Model {
            id: "claude-opus-4-7".into(),
            name: "Claude Opus 4.7".into(),
            reasoning: true,
            supports_adaptive_thinking: true,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            force_budget_thinking: true,
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);
        assert!(
            matches!(body.thinking, Some(ThinkingConfig::Enabled { .. })),
            "force_budget should produce Enabled, got {:?}",
            body.thinking
        );
    }
    #[test]
    fn opus_4_7_uses_adaptive_thinking_with_xhigh_effort() {
        let model = Model {
            id: "claude-opus-4-7".into(),
            name: "Claude Opus 4.7".into(),
            reasoning: true,
            supports_adaptive_thinking: true,
            supported_thinking_levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Xhigh,
                ThinkingLevel::Other("max".into()),
            ],
            default_thinking_level: None,
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
    fn a_budget_only_model_keeps_budget_thinking_and_sends_no_effort() {
        let model = Model {
            reasoning: true,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            effort: Some(EffortLevel::Low),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);

        assert!(matches!(
            body.thinking,
            Some(ThinkingConfig::Enabled { .. })
        ));
        assert!(body.output_config.is_none());
    }

    #[test]
    fn opus_4_5_keeps_budget_thinking_with_effort_from_its_ladder() {
        let model = Model {
            id: "claude-opus-4-5-20251101".into(),
            name: "Claude Opus 4.5".into(),
            reasoning: true,
            supports_adaptive_thinking: false,
            supported_thinking_levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::Medium),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, false);

        assert!(matches!(
            body.thinking,
            Some(ThinkingConfig::Enabled { .. })
        ));
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("medium")
        );
    }

    fn catalogue_model(id: &str) -> Model {
        crate::models::anthropic_models()
            .into_iter()
            .find(|model| model.id.as_str() == id)
            .unwrap_or_else(|| panic!("{id} is in the anthropic catalogue"))
    }

    #[test]
    fn haiku_4_5_gets_budget_thinking_without_effort_inside_its_output_limit() {
        let model = catalogue_model("claude-haiku-4-5");
        for thinking in [ThinkingLevel::High, ThinkingLevel::Off] {
            let options = StreamOptions {
                thinking: Some(thinking.clone()),
                ..Default::default()
            };
            for is_oauth in [true, false] {
                let body = build_request_body(&model, &None, &[], &[], &options, is_oauth);
                assert!(
                    body.output_config.is_none(),
                    "{thinking:?} oauth={is_oauth}"
                );
                assert!(body.max_tokens <= 64_000, "{thinking:?} oauth={is_oauth}");
                assert_eq!(
                    matches!(body.thinking, Some(ThinkingConfig::Enabled { .. })),
                    thinking == ThinkingLevel::High
                );
            }
        }
        let oauth_off = build_request_body(
            &model,
            &None,
            &[],
            &[],
            &StreamOptions {
                thinking: Some(ThinkingLevel::Off),
                ..Default::default()
            },
            true,
        );
        assert_eq!(oauth_off.max_tokens, 64_000);
    }

    #[test]
    fn haiku_5_5_takes_adaptive_thinking_with_effort_and_its_full_output() {
        let model = catalogue_model("claude-haiku-5-5");
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::Medium),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, true);

        assert!(matches!(
            body.thinking,
            Some(ThinkingConfig::Adaptive { .. })
        ));
        assert_eq!(
            body.output_config.as_ref().map(|cfg| cfg.effort.as_str()),
            Some("medium")
        );
        assert_eq!(body.max_tokens, 128_000);
    }

    #[test]
    fn an_explicit_max_tokens_is_clamped_to_the_model_output_limit() {
        let model = Model {
            max_output_tokens: TokenCount::new(64_000),
            ..test_model()
        };
        let options = StreamOptions {
            max_tokens: Some(TokenCount::new(200_000)),
            ..Default::default()
        };
        for is_oauth in [true, false] {
            let body = build_request_body(&model, &None, &[], &[], &options, is_oauth);
            assert_eq!(body.max_tokens, 64_000, "oauth={is_oauth}");
        }
        let small = StreamOptions {
            max_tokens: Some(TokenCount::new(1_000)),
            ..Default::default()
        };
        let body = build_request_body(&model, &None, &[], &[], &small, true);
        assert_eq!(body.max_tokens, 1_000);
    }

    #[test]
    fn effort_outside_the_ladder_falls_to_the_nearest_level_the_model_takes() {
        let max = ThinkingLevel::Other("max".into());
        let model = |ladder: Vec<ThinkingLevel>| Model {
            reasoning: true,
            supports_adaptive_thinking: true,
            supported_thinking_levels: ladder,
            ..test_model()
        };
        let without_xhigh = model(vec![
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            max.clone(),
        ]);
        let without_max = model(vec![
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::Xhigh,
        ]);
        let effort = |level: &str, model: &Model| {
            anthropic_effort(&EffortLevel::from_effort_str(level).unwrap(), model)
        };

        assert_eq!(effort("xhigh", &without_xhigh).as_deref(), Some("high"));
        assert_eq!(effort("max", &without_xhigh).as_deref(), Some("max"));
        assert_eq!(effort("max", &without_max).as_deref(), Some("xhigh"));
        assert_eq!(effort("minimal", &without_max).as_deref(), Some("low"));
        assert_eq!(effort("medium", &without_max).as_deref(), Some("medium"));
    }

    #[test]
    fn the_catalogue_sends_effort_only_to_models_anthropic_lists_for_it() {
        let takes_effort = [
            "claude-opus-5-5",
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-6",
        ];
        let no_effort = [
            "claude-haiku-4-5",
            "claude-sonnet-4-20250514",
            "claude-opus-4-20250514",
        ];
        for id in takes_effort {
            assert!(accepts_effort(&catalogue_model(id)), "{id} takes effort");
        }
        for id in no_effort {
            assert!(!accepts_effort(&catalogue_model(id)), "{id} rejects effort");
        }
        let listed = takes_effort.len() + no_effort.len();
        assert_eq!(crate::models::anthropic_models().len(), listed);
    }

    #[test]
    fn process_message_start_event() {
        let event = SseEvent::MessageStart {
            message: MessageStartData {
                id: None,
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
    fn process_message_start_preserves_message_id() {
        let event = SseEvent::MessageStart {
            message: MessageStartData {
                id: Some("msg_01previous".into()),
                usage: None,
            },
        };

        let mut output = test_output();
        let mut blocks = Vec::new();
        let events = process_sse_event(event, &mut output, &mut blocks, true, &[]);

        assert_eq!(output.generation_id.as_deref(), Some("msg_01previous"));
        match &events[0] {
            StreamEvent::Start { partial } => {
                assert_eq!(partial.generation_id.as_deref(), Some("msg_01previous"));
            }
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn unknown_blocks_do_not_shift_later_indexes() {
        let mut output = test_output();
        let mut blocks = Vec::new();
        let unknown: SseEvent = serde_json::from_str(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"new_block"}}"#,
        )
        .unwrap();
        process_sse_event(unknown, &mut output, &mut blocks, false, &[]);
        process_sse_event(
            SseEvent::ContentBlockStart {
                index: 1,
                content_block: ContentBlockData::Text {
                    text: String::new(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );
        let events = process_sse_event(
            SseEvent::ContentBlockDelta {
                index: 1,
                delta: DeltaData::TextDelta {
                    text: "hello".into(),
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
        assert_eq!(output.text(), "hello");
    }

    #[test]
    fn citations_delta_is_ignored_without_losing_the_text_block() {
        let mut output = test_output();
        let mut blocks = Vec::new();
        process_sse_event(
            SseEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlockData::Text {
                    text: "answer".into(),
                },
            },
            &mut output,
            &mut blocks,
            false,
            &[],
        );
        let citation: SseEvent = serde_json::from_str(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{"type":"char_location"}}}"#,
        )
        .unwrap();
        assert!(process_sse_event(citation, &mut output, &mut blocks, false, &[]).is_empty());
        let events = process_sse_event(
            SseEvent::ContentBlockStop { index: 0 },
            &mut output,
            &mut blocks,
            false,
            &[],
        );
        assert!(matches!(
            &events[0],
            StreamEvent::TextEnd { text, .. } if text == "answer"
        ));
    }

    #[test]
    fn process_text_block_lifecycle() {
        let mut output = test_output();
        let mut blocks = Vec::new();

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
    fn process_advisor_server_tool_blocks_preserves_content() {
        let mut output = test_output();
        let mut blocks = Vec::new();

        let server_tool_start: SseEvent = serde_json::from_value(serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "server_tool_use",
                "id": "srvtoolu_01advisor",
                "name": "advisor",
                "input": {}
            }
        }))
        .expect("server tool use event parses");
        let events = process_sse_event(server_tool_start, &mut output, &mut blocks, true, &[]);
        assert!(
            events.is_empty(),
            "server-side tools are not client tool calls"
        );

        let events = process_sse_event(
            SseEvent::ContentBlockStop { index: 0 },
            &mut output,
            &mut blocks,
            true,
            &[],
        );
        assert!(events.is_empty(), "server-side tool close should be quiet");

        let advisor_result_start: SseEvent = serde_json::from_value(serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "advisor_tool_result",
                "tool_use_id": "srvtoolu_01advisor",
                "content": {
                    "type": "advisor_result",
                    "text": "prefer a bounded worker pool"
                }
            }
        }))
        .expect("advisor result event parses");
        let events = process_sse_event(advisor_result_start, &mut output, &mut blocks, true, &[]);
        assert!(events.is_empty(), "advisor results are transcript blocks");

        let events = process_sse_event(
            SseEvent::ContentBlockStop { index: 1 },
            &mut output,
            &mut blocks,
            true,
            &[],
        );
        assert!(events.is_empty(), "advisor result close should be quiet");

        assert_eq!(
            serde_json::to_value(&output.content).unwrap(),
            serde_json::json!([
                {
                    "type": "server_tool_use",
                    "id": "srvtoolu_01advisor",
                    "name": "advisor",
                    "input": {}
                },
                {
                    "type": "advisor_tool_result",
                    "tool_use_id": "srvtoolu_01advisor",
                    "content": {
                        "type": "advisor_result",
                        "text": "prefer a bounded worker pool"
                    }
                }
            ])
        );
    }

    #[test]
    fn convert_assistant_preserves_advisor_server_tool_blocks() {
        let content: Vec<AssistantContentPart> = serde_json::from_value(serde_json::json!([
            {
                "type": "server_tool_use",
                "id": "srvtoolu_01advisor",
                "name": "advisor",
                "input": {}
            },
            {
                "type": "advisor_tool_result",
                "tool_use_id": "srvtoolu_01advisor",
                "content": {
                    "type": "advisor_result",
                    "text": "prefer a bounded worker pool"
                }
            }
        ]))
        .expect("advisor blocks deserialize");
        let messages = vec![Message::Assistant(
            AssistantMessage::builder()
                .content(content)
                .model("claude-opus-4-8".into())
                .provider(Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted = convert_messages(&messages, true, None, ToolResultTrimming::SlidingWindow);

        assert_eq!(converted.len(), 1);
        assert_eq!(
            converted[0].content,
            serde_json::json!([
                {
                    "type": "server_tool_use",
                    "id": "srvtoolu_01advisor",
                    "name": "advisor",
                    "input": {}
                },
                {
                    "type": "advisor_tool_result",
                    "tool_use_id": "srvtoolu_01advisor",
                    "content": {
                        "type": "advisor_result",
                        "text": "prefer a bounded worker pool"
                    }
                }
            ])
        );
    }

    #[test]
    fn claude_code_name_mapping() {
        assert_eq!(to_claude_code_name("read"), "Read");
        assert_eq!(to_claude_code_name("Write"), "Write");
        assert_eq!(to_claude_code_name("bash"), "Bash");
        assert_eq!(to_claude_code_name("webfetch"), "WebFetch");
        assert_eq!(to_claude_code_name("websearch"), "WebSearch");
        assert_eq!(to_claude_code_name("web_fetch"), "WebFetch");
        assert_eq!(to_claude_code_name("web_search"), "WebSearch");
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
    fn version_suffix_hashes_three_characters_of_the_prompt() {
        assert_eq!(claude_code_version_suffix("test test 1212 hello"), "335");
        assert_eq!(
            claude_code_version_suffix("hello world, this is a longer prompt"),
            "2f6"
        );
        assert_eq!(claude_code_version_suffix(""), "d7b");
    }

    #[test]
    fn first_user_text_skips_assistant_turns_and_takes_the_leading_text_part() {
        let assistant_first = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .model("claude-sonnet-4-20250514".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Parts(vec![
                        UserContentPart::Text(TextContent {
                            text: "read foo.rs".into(),
                        }),
                        UserContentPart::Text(TextContent {
                            text: "and bar.rs".into(),
                        }),
                    ]))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
        ];

        assert_eq!(first_user_text(&assistant_first), "read foo.rs");
        assert_eq!(first_user_text(&[]), "");
    }

    #[test]
    fn last_request_id_takes_the_newest_assistant_turn_that_has_one() {
        let with_id = |id: Option<&str>| {
            Message::Assistant(
                AssistantMessage::builder()
                    .model("claude-sonnet-4-20250514".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .maybe_request_id(id.map(str::to_string))
                    .build(),
            )
        };
        let messages = vec![
            with_id(Some("req_011CegE4MyDYb8hhpqGcgXTQ")),
            with_id(Some("req_011CegEdLttts7Bw3ZRVuax3")),
            with_id(None),
        ];

        assert_eq!(
            last_request_id(&messages).as_deref(),
            Some("req_011CegEdLttts7Bw3ZRVuax3")
        );
        assert_eq!(last_request_id(&[]), None);
    }

    #[test]
    fn last_prompt_id_takes_the_newest_user_turn_that_has_one() {
        let with_id = |id: Option<&str>| {
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("hi".into()))
                    .timestamp_ms(Timestamp::zero())
                    .maybe_prompt_id(id.map(PromptId::from))
                    .build(),
            )
        };
        let messages = vec![
            with_id(Some("b492a0c4-b3a8-4cb6-a309-7dac9efccb68")),
            with_id(Some("af566db0-4832-4c55-bfe2-c807d443d91f")),
            with_id(None),
        ];

        assert_eq!(
            last_prompt_id(&messages).map(|id| id.to_string()),
            Some("af566db0-4832-4c55-bfe2-c807d443d91f".to_string())
        );
        assert_eq!(last_prompt_id(&[]), None);
    }

    #[test]
    fn an_api_key_to_the_anthropic_endpoint_gets_the_block_too() {
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("test test 1212 hello".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];
        let body = build_request_body(
            &test_model(),
            &Some("be helpful".into()),
            &messages,
            &[],
            &StreamOptions::default(),
            false,
        );

        assert_eq!(
            body.system.unwrap()[0].text,
            "x-anthropic-billing-header: cc_version=2.1.280.335; cc_entrypoint=cli; cch=00000;"
        );
    }

    #[tokio::test]
    async fn the_reply_records_the_request_id_the_server_gave_it() {
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_01x","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            "\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let http_resp = http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("request-id", "req_011CegE4MyDYb8hhpqGcgXTQ")
            .body(bytes::Bytes::from(sse))
            .unwrap();
        let mut stream = parse_sse_stream(
            reqwest::Response::from(http_resp),
            &test_model(),
            0,
            true,
            vec![],
        );

        let mut last = None;
        use futures::StreamExt;
        while let Some(event) = stream.next().await {
            if let StreamEvent::Done { message, .. } = event {
                last = Some(message);
            }
        }

        assert_eq!(
            last.unwrap().request_id.as_deref(),
            Some("req_011CegE4MyDYb8hhpqGcgXTQ")
        );
    }

    async fn final_event(sse: String) -> StreamEvent {
        use futures::StreamExt;
        let http_resp = http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(bytes::Bytes::from(sse))
            .unwrap();
        let stream = parse_sse_stream(
            reqwest::Response::from(http_resp),
            &test_model(),
            0,
            true,
            vec![],
        );
        stream
            .collect::<Vec<_>>()
            .await
            .pop()
            .expect("a final event")
    }

    fn sse_after_message_start(rest: &[&str]) -> String {
        let start = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_01x","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            "\n\n",
        );
        std::iter::once(start).chain(rest.iter().copied()).collect()
    }

    #[tokio::test]
    async fn a_stream_cut_off_before_the_message_ends_is_an_early_end() {
        let sse = sse_after_message_start(&["event: ping\ndata: {\"type\":\"ping\"}\n\n"]);
        match final_event(sse).await {
            StreamEvent::Done { reason, message } => {
                assert_eq!(reason, StopReason::Error);
                assert_eq!(message.error_message.as_deref(), Some(STREAM_ENDED_EARLY));
            }
            other => panic!("expected a final message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_end_turn_is_a_finished_reply() {
        let sse = sse_after_message_start(&[
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            "\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        match final_event(sse).await {
            StreamEvent::Done { reason, message } => {
                assert_eq!(reason, StopReason::Stop);
                assert!(message.content.is_empty());
                assert_eq!(message.error_message, None);
            }
            other => panic!("expected a final message, got {other:?}"),
        }
    }

    #[test]
    fn request_id_shape_matches_claude_codes_pattern() {
        assert!(is_request_id("req_011CegEdLttts7Bw3ZRVuax3"));
        assert!(is_request_id("req_a-b_C9"));
        assert!(!is_request_id("msg_011CegE4MyDYb8hhpqGcgXTQ"));
        assert!(!is_request_id("req_"));
        assert!(!is_request_id(&format!("req_{}", "a".repeat(37))));
        assert!(!is_request_id("req_has spaces"));
    }

    #[test]
    fn prompt_id_shape_matches_claude_codes_pattern() {
        assert!(is_uuid("af566db0-4832-4c55-bfe2-c807d443d91f"));
        assert!(is_uuid("AF566DB0-4832-4C55-BFE2-C807D443D91F"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("af566db048324c55bfe2c807d443d91f"));
        assert!(!is_uuid("af566db0-4832-4c55-bfe2-c807d443d91"));
    }

    #[test]
    fn an_id_of_the_wrong_shape_is_dropped() {
        let messages = vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("test test 1212 hello".into()))
                    .timestamp_ms(Timestamp::zero())
                    .prompt_id("not-a-uuid".into())
                    .build(),
            ),
            Message::Assistant(
                AssistantMessage::builder()
                    .model("claude-sonnet-4-20250514".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .request_id("msg_011CegE4MyDYb8hhpqGcgXTQ")
                    .build(),
            ),
        ];
        let body = build_request_body(
            &test_model(),
            &None,
            &messages,
            &[],
            &StreamOptions::default(),
            true,
        );

        assert_eq!(
            body.system.unwrap()[0].text,
            "x-anthropic-billing-header: cc_version=2.1.280.335; cc_entrypoint=cli; cch=00000;"
        );
    }

    #[test]
    fn both_ride_in_the_block_previous_request_first() {
        let messages = vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("test test 1212 hello".into()))
                    .timestamp_ms(Timestamp::zero())
                    .prompt_id("af566db0-4832-4c55-bfe2-c807d443d91f".into())
                    .build(),
            ),
            Message::Assistant(
                AssistantMessage::builder()
                    .model("claude-sonnet-4-20250514".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .request_id("req_011CegEdLttts7Bw3ZRVuax3")
                    .build(),
            ),
        ];
        let body = build_request_body(
            &test_model(),
            &None,
            &messages,
            &[],
            &StreamOptions::default(),
            true,
        );

        assert_eq!(
            body.system.unwrap()[0].text,
            "x-anthropic-billing-header: cc_version=2.1.280.335; cc_entrypoint=cli; cch=00000; cc_prev_req=req_011CegEdLttts7Bw3ZRVuax3; cc_prompt_id=af566db0-4832-4c55-bfe2-c807d443d91f;"
        );
    }

    #[test]
    fn the_prompt_id_rides_in_the_billing_block() {
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("test test 1212 hello".into()))
                .timestamp_ms(Timestamp::zero())
                .prompt_id("af566db0-4832-4c55-bfe2-c807d443d91f".into())
                .build(),
        )];
        let body = build_request_body(
            &test_model(),
            &None,
            &messages,
            &[],
            &StreamOptions::default(),
            true,
        );

        assert_eq!(
            body.system.unwrap()[0].text,
            "x-anthropic-billing-header: cc_version=2.1.280.335; cc_entrypoint=cli; cch=00000; cc_prompt_id=af566db0-4832-4c55-bfe2-c807d443d91f;"
        );
    }

    #[test]
    fn the_previous_request_id_rides_in_the_billing_block() {
        let messages = vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("test test 1212 hello".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::Assistant(
                AssistantMessage::builder()
                    .model("claude-sonnet-4-20250514".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .request_id("req_011CegEdLttts7Bw3ZRVuax3")
                    .build(),
            ),
        ];
        let body = build_request_body(
            &test_model(),
            &None,
            &messages,
            &[],
            &StreamOptions::default(),
            true,
        );

        assert_eq!(
            body.system.unwrap()[0].text,
            "x-anthropic-billing-header: cc_version=2.1.280.335; cc_entrypoint=cli; cch=00000; cc_prev_req=req_011CegEdLttts7Bw3ZRVuax3;"
        );
    }

    #[test]
    fn the_billing_suffix_hashes_the_first_user_message() {
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("test test 1212 hello".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];
        let body = build_request_body(
            &test_model(),
            &None,
            &messages,
            &[],
            &StreamOptions::default(),
            true,
        );

        assert_eq!(
            body.system.unwrap()[0].text,
            "x-anthropic-billing-header: cc_version=2.1.280.335; cc_entrypoint=cli; cch=00000;"
        );
    }

    #[test]
    fn api_key_requests_send_interleaved_thinking_only_for_budget_models_and_stream_tools_eagerly()
    {
        let key = ApiKey::new("sk-ant-api03-test").unwrap();
        let budget = Model {
            supports_adaptive_thinking: false,
            ..test_model()
        };
        let headers =
            build_request_headers(&budget, &StreamOptions::default(), &key, false).unwrap();
        assert_eq!(headers["anthropic-beta"], "interleaved-thinking-2025-05-14");

        let adaptive = Model {
            supports_adaptive_thinking: true,
            ..test_model()
        };
        let headers =
            build_request_headers(&adaptive, &StreamOptions::default(), &key, false).unwrap();
        assert!(headers.get("anthropic-beta").is_none());

        let tools = [ToolDefinition {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let body = build_request_body(
            &budget,
            &None,
            &[],
            &tools,
            &StreamOptions::default(),
            false,
        );
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["tools"][0]["eager_input_streaming"], true);
    }

    #[test]
    fn every_beta_toggle_on_matches_the_claude_code_capture() {
        let model = Model {
            context_window: TokenCount::new(1_000_000),
            ..test_model()
        };
        let key = ApiKey::new("sk-ant-oat01-test").unwrap();
        let headers = build_request_headers(&model, &StreamOptions::default(), &key, true).unwrap();

        assert_eq!(
            headers["anthropic-beta"],
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07"
        );
    }

    #[test]
    fn thinking_updates_off_restores_the_claude_code_thinking_shape() {
        let model = Model {
            reasoning: true,
            supports_adaptive_thinking: true,
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            anthropic_betas: Some(AnthropicBetas {
                thinking_updates: false,
                ..AnthropicBetas::default()
            }),
            ..Default::default()
        };

        let body = build_request_body(&model, &None, &[], &[], &options, true);
        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["thinking"], serde_json::json!({"type": "adaptive"}));
        assert!(
            !options
                .anthropic_betas
                .as_ref()
                .unwrap()
                .to_header_value()
                .contains("thinking-display-updates-2026-08-18")
        );
    }

    #[test]
    fn the_node_runtime_version_is_reported() {
        let headers = oauth_test_headers();
        assert_eq!(headers["x-claude-code-request-class"], "main");
        assert_eq!(headers["x-stainless-runtime-version"], "v26.3.0");
    }

    #[test]
    fn the_bundled_sdk_version_is_reported() {
        let headers = oauth_test_headers();
        assert_eq!(headers["x-stainless-package-version"], "0.112.1");
    }

    #[test]
    fn version_reaches_user_agent_and_billing_block() {
        let model = test_model();
        let options = StreamOptions::default();
        let body = build_request_body(&model, &None, &[], &[], &options, true);
        let billing = body.system.unwrap()[0].text.clone();

        assert_eq!(
            claude_cli_user_agent(),
            "claude-cli/2.1.280 (external, cli)"
        );
        assert!(
            billing.starts_with("x-anthropic-billing-header: cc_version=2.1.280."),
            "billing={billing}"
        );
    }

    #[test]
    fn oauth_system_prompt_has_identity() {
        let model = test_model();
        let options = StreamOptions::default();
        let body = build_request_body(&model, &Some("be helpful".into()), &[], &[], &options, true);
        let system = body.system.unwrap();
        assert_eq!(system.len(), 3);
        assert!(system[0].text.starts_with("x-anthropic-billing-header:"));
        assert!(system[1].text.contains("Claude Code"));
        assert_eq!(system[2].text, "be helpful");
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
        let options = StreamOptions {
            cache_retention: None,
            ..Default::default()
        };
        let system = Some("you are helpful".to_string());
        let body = build_request_body(&test_model(), &system, &[], &[], &options, true);
        let system_blocks = body.system.unwrap();
        assert_eq!(system_blocks.len(), 3);
        let cc = system_blocks[2].cache_control.as_ref().unwrap();
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
        assert_eq!(system_blocks.len(), 2);
        let cc = system_blocks[1].cache_control.as_ref().unwrap();
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
        let messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hello".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let converted =
            convert_messages(&messages, false, cache, ToolResultTrimming::SlidingWindow);
        let blocks = converted[0].content.as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "hello");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn user_text_content_format_stable_across_cache_control_moves() {
        let cache = Some(CacheControl {
            control_type: "ephemeral".into(),
            ttl: None,
            scope: None,
        });

        let messages_r1 = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("hello".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];
        let converted_r1 = convert_messages(
            &messages_r1,
            false,
            cache.clone(),
            ToolResultTrimming::Preserve,
        );

        let messages_r2 = vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("hello".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContentPart::Text(TextContent {
                    text: "hi there".into(),
                })],
                generation_id: None,
                ..test_output()
            }),
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("goodbye".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
        ];
        let converted_r2 =
            convert_messages(&messages_r2, false, cache, ToolResultTrimming::Preserve);

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
    fn cache_control_on_recent_tool_result_block() {
        let cache = Some(CacheControl {
            control_type: "ephemeral".into(),
            ttl: None,
            scope: None,
        });
        let messages = vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("do the thing".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "echo hi"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "hi".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];

        let converted = convert_messages(&messages, false, cache, ToolResultTrimming::Preserve);

        let has_cc_on_user = match &converted[0].content {
            serde_json::Value::Array(blocks) => blocks
                .first()
                .is_some_and(|b| b.get("cache_control").is_some()),
            _ => false,
        };
        assert!(
            !has_cc_on_user,
            "cache_control should move to the latest cacheable block"
        );

        let tool_result = &converted[2].content.as_array().unwrap()[0];
        assert_eq!(tool_result["type"], "tool_result");
        assert_eq!(tool_result["cache_control"]["type"], "ephemeral");

        let nested_content_has_cache_control = match &tool_result["content"] {
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .any(|block| block.get("cache_control").is_some()),
            _ => false,
        };
        assert!(
            !nested_content_has_cache_control,
            "tool_result content sub-blocks must not carry cache_control"
        );
    }

    #[test]
    fn cache_control_moves_to_recent_tool_result_in_long_tool_loop() {
        let cache = Some(CacheControl {
            control_type: "ephemeral".into(),
            ttl: None,
            scope: None,
        });
        let mut messages = vec![Message::User(
            UserMessage::builder()
                .content(UserContent::Text("do the thing".into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        for i in 0..25 {
            let id = format!("tc_{i}");
            messages.push(Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: id.clone().into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "true"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Custom("test".into()))
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ));
            messages.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "ok".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }));
        }

        let converted = convert_messages(&messages, false, cache, ToolResultTrimming::Preserve);
        let first_user_blocks = converted[0].content.as_array().unwrap();
        let latest_blocks = converted.last().unwrap().content.as_array().unwrap();

        assert!(
            first_user_blocks
                .iter()
                .all(|block| block.get("cache_control").is_none()),
            "cache_control should move forward rather than stay on the stale user text"
        );
        assert_eq!(latest_blocks[0]["type"], "tool_result");
        assert_eq!(latest_blocks[0]["cache_control"]["type"], "ephemeral");
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
        assert_eq!(system.len(), 2);
        assert!(system[0].text.starts_with("x-anthropic-billing-header:"));
        assert_eq!(system[1].text, "be helpful");
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
        assert_eq!(serde_json::to_value(&tools[0]).unwrap()["name"], "Read");
    }

    #[test]
    fn oauth_body_matches_claude_code_capture_shape() {
        let model = Model {
            id: "claude-opus-4-8".into(),
            name: "Claude Opus 4.8".into(),
            reasoning: true,
            context_window: TokenCount::new(1_000_000),
            max_output_tokens: TokenCount::new(64_000),
            supports_adaptive_thinking: true,
            supported_thinking_levels: vec![ThinkingLevel::High],
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            session_id: Some("099eaa52-339e-4f1b-adc9-38a451168a7e".into()),
            account_id: Some("0f47b1d2-dec7-41a4-bd49-4d34f0b8f360".into()),
            ..Default::default()
        };
        let messages = vec![
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "toolu_01abc".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "echo hi"}),
                    })])
                    .model("claude-opus-4-8".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .generation_id("msg_01previous")
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "toolu_01abc".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: "hi".into(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
        ];
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "run shell commands".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let body = build_request_body(
            &model,
            &Some("be helpful".into()),
            &messages,
            &tools,
            &options,
            true,
        );
        let json = serde_json::to_value(&body).unwrap();

        assert!(
            json.get("cache_control").is_none(),
            "claude-code keeps cache_control on blocks, not the request root: {json}"
        );
        assert_eq!(
            json["thinking"],
            serde_json::json!({"type": "adaptive", "display": "updates"})
        );
        assert_eq!(
            json["context_management"],
            expected_claude_code_context_management()
        );
        assert_eq!(
            json["diagnostics"],
            serde_json::json!({"previous_message_id": "msg_01previous"})
        );

        let user_id: serde_json::Value =
            serde_json::from_str(json["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            user_id["account_uuid"],
            "0f47b1d2-dec7-41a4-bd49-4d34f0b8f360"
        );
        assert_eq!(
            user_id["session_id"],
            "099eaa52-339e-4f1b-adc9-38a451168a7e"
        );
        let device_id = user_id["device_id"].as_str().unwrap();
        assert_eq!(device_id.len(), 64);
        assert!(device_id.chars().all(|c| c.is_ascii_hexdigit()));

        let system = json["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], claude_code_billing_header(&messages));
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["text"], CLAUDE_CODE_IDENTITY);
        assert!(system[1].get("cache_control").is_none());
        assert_eq!(system[2]["text"], "be helpful");
        assert_eq!(
            system[2]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h", "scope": "global"})
        );

        assert_eq!(json["tools"][0]["name"], "Bash");
        assert_eq!(json["tools"][0]["eager_input_streaming"], true);
        assert_eq!(
            json["messages"][0]["content"][0]["caller"],
            serde_json::json!({"type": "direct"})
        );
        assert_eq!(json["messages"][1]["content"][0]["content"], "hi");
    }

    #[test]
    fn oauth_tools_are_sorted_like_claude_code_capture() {
        let model = test_model();
        let options = StreamOptions::default();
        let tools = vec![
            ToolDefinition {
                name: "write".into(),
                description: "write files".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "read".into(),
                description: "read files".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "skill".into(),
                description: "load skills".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "bash".into(),
                description: "run shell commands".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "repo_map".into(),
                description: "show repo map".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolDefinition {
                name: "edit".into(),
                description: "edit files".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];

        let body = build_request_body(&model, &None, &[], &tools, &options, true);
        let names: Vec<_> = body
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| serde_json::to_value(tool).unwrap()["name"].clone())
            .collect();

        assert_eq!(
            names,
            vec![
                serde_json::json!("Bash"),
                serde_json::json!("Edit"),
                serde_json::json!("Read"),
                serde_json::json!("Skill"),
                serde_json::json!("Write"),
                serde_json::json!("repo_map")
            ]
        );
    }

    #[test]
    fn oauth_advisor_beta_adds_native_advisor_tool() {
        let model = Model {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            ..test_model()
        };
        let options = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "run shell commands".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let body = build_request_body(&model, &None, &[], &tools, &options, true);
        let tools = serde_json::to_value(body.tools.unwrap()).unwrap();

        assert_eq!(
            tools,
            serde_json::json!([
                {
                    "name": "Bash",
                    "description": "run shell commands",
                    "input_schema": {"type": "object"},
                    "eager_input_streaming": true
                },
                {
                    "type": "advisor_20260301",
                    "name": "advisor",
                    "model": "claude-opus-5-5", "defer_loading": true
                }
            ])
        );
    }

    #[test]
    fn oauth_advisor_tool_is_not_added_to_opus_capture_shape() {
        let model = Model {
            id: "claude-opus-4-8".into(),
            name: "Claude Opus 4.8".into(),
            ..test_model()
        };
        let options = StreamOptions::default();
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "run shell commands".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let body = build_request_body(&model, &None, &[], &tools, &options, true);
        let tools = serde_json::to_value(body.tools.unwrap()).unwrap();

        assert_eq!(tools.as_array().unwrap().len(), 1);
        assert_eq!(tools[0]["name"], "Bash");
    }

    #[test]
    fn oauth_advisor_transcript_keeps_native_advisor_tool() {
        let model = Model {
            id: "claude-opus-4-8".into(),
            name: "Claude Opus 4.8".into(),
            ..test_model()
        };
        let content: Vec<AssistantContentPart> = serde_json::from_value(serde_json::json!([
            {
                "type": "server_tool_use",
                "id": "srvtoolu_01advisor",
                "name": "advisor",
                "input": {}
            },
            {
                "type": "advisor_tool_result",
                "tool_use_id": "srvtoolu_01advisor",
                "content": {
                    "type": "advisor_result",
                    "text": "prefer a bounded worker pool"
                }
            }
        ]))
        .expect("advisor blocks deserialize");
        let messages = vec![Message::Assistant(
            AssistantMessage::builder()
                .content(content)
                .model("claude-opus-4-8".into())
                .provider(Provider::Anthropic)
                .api(Api::AnthropicMessages)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )];

        let body = build_request_body(
            &model,
            &None,
            &messages,
            &[],
            &StreamOptions::default(),
            true,
        );
        let tools = serde_json::to_value(body.tools.unwrap()).unwrap();

        assert_eq!(
            tools,
            serde_json::json!([
                {
                    "type": "advisor_20260301",
                    "name": "advisor",
                    "model": "claude-opus-5-5", "defer_loading": true
                }
            ])
        );
    }

    #[test]
    fn oauth_empty_tool_catalogue_stays_empty() {
        let model = Model {
            id: "claude-haiku-4-5-20251001".into(),
            name: "Claude Haiku 4.5".into(),
            ..test_model()
        };

        let body = build_request_body(&model, &None, &[], &[], &StreamOptions::default(), true);
        let tools = serde_json::to_value(body.tools.unwrap()).unwrap();

        assert_eq!(tools, serde_json::json!([]));
    }

    #[test]
    fn advisor_tool_respects_beta_toggle_and_oauth_mode() {
        let model = Model {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            ..test_model()
        };
        let options = StreamOptions {
            anthropic_betas: Some(AnthropicBetas {
                advisor: false,
                ..Default::default()
            }),
            ..Default::default()
        };
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "run shell commands".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let disabled = build_request_body(&model, &None, &[], &tools, &options, true);
        let disabled_names: Vec<_> = disabled
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| serde_json::to_value(tool).unwrap()["name"].clone())
            .collect();
        assert_eq!(disabled_names, vec![serde_json::json!("Bash")]);

        let direct =
            build_request_body(&model, &None, &[], &tools, &StreamOptions::default(), false);
        let direct_names: Vec<_> = direct
            .tools
            .unwrap()
            .into_iter()
            .map(|tool| serde_json::to_value(tool).unwrap()["name"].clone())
            .collect();
        assert_eq!(direct_names, vec![serde_json::json!("bash")]);
    }

    #[test]
    fn benchmark_tool_call_deltas_returns_payload_size() {
        assert_eq!(benchmark_tool_call_deltas(8, 1024), 1024);
    }

    fn expected_claude_code_context_management() -> serde_json::Value {
        serde_json::json!({
            "edits": [
                {
                    "type": "clear_thinking_20251015",
                    "keep": "all",
                },
            ]
        })
    }

    fn oauth_test_headers() -> HeaderMap {
        let key = ApiKey::new("sk-ant-oat01-test").unwrap();
        build_request_headers(&test_model(), &StreamOptions::default(), &key, true).unwrap()
    }

    fn test_model() -> Model {
        Model::builder()
            .id("claude-sonnet-4-20250514".into())
            .name("Claude Sonnet 4".into())
            .api(Api::AnthropicMessages)
            .provider(Provider::Anthropic)
            .base_url(DEFAULT_BASE_URL.into())
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

    fn messages_with_old_large_tool_result() -> Vec<Message> {
        let large_output = "x".repeat(3000);
        vec![
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("read foo.rs".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::Assistant(
                AssistantMessage::builder()
                    .content(vec![AssistantContentPart::ToolCall(ToolCall {
                        id: "tc_old".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "foo.rs"}),
                    })])
                    .model("test".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .stop_reason(StopReason::ToolUse)
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc_old".into(),
                tool_name: "read".into(),
                content: vec![ToolResultContentPart::Text(TextContent {
                    text: large_output.clone(),
                })],
                outcome: ToolOutcome::Success,
                timestamp_ms: Timestamp::zero(),
                details: None,
            }),
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("thanks".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("now do something else".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
            Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("one more thing".into()))
                    .timestamp_ms(Timestamp::zero())
                    .build(),
            ),
        ]
    }

    #[test]
    fn sliding_window_trims_old_tool_results() {
        let messages = messages_with_old_large_tool_result();
        let converted = convert_messages(&messages, false, None, ToolResultTrimming::SlidingWindow);

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

    #[test]
    fn cached_message_content_survives_saved_history_and_follow_up() {
        let original = messages_with_old_large_tool_result();
        for oauth in [false, true] {
            let cache = anthropic_cache_control(
                "https://api.anthropic.com",
                Some(CacheRetention::Long),
                oauth,
            );
            let before = convert_messages(
                &original,
                oauth,
                cache.clone(),
                ToolResultTrimming::Preserve,
            );
            let mut resumed: Vec<Message> =
                serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
            resumed.push(Message::Assistant(
                AssistantMessage::builder()
                    .model("test".into())
                    .provider(Provider::Anthropic)
                    .api(Api::AnthropicMessages)
                    .timestamp_ms(Timestamp::zero())
                    .content(vec![AssistantContentPart::Text(TextContent {
                        text: "done".into(),
                    })])
                    .build(),
            ));
            resumed.push(Message::User(
                UserMessage::builder()
                    .content(UserContent::Text("explicit follow-up".into()))
                    .timestamp_ms(Timestamp::now())
                    .build(),
            ));
            let after = convert_messages(&resumed, oauth, cache, ToolResultTrimming::Preserve);
            for (a, b) in before.iter().zip(&after) {
                let strip = |message: &RequestMessage| {
                    let mut content = message.content.clone();
                    for block in content.as_array_mut().unwrap() {
                        block.as_object_mut().unwrap().remove("cache_control");
                    }
                    (message.role, content)
                };
                assert_eq!(strip(a), strip(b));
            }
            assert_eq!(
                after.last().unwrap().content[0]["cache_control"]["type"],
                "ephemeral"
            );
            assert_eq!(
                after.last().unwrap().content[0]["cache_control"]["ttl"],
                "1h"
            );
        }
    }

    fn test_output() -> AssistantMessage {
        AssistantMessage::builder()
            .model("test".into())
            .provider(Provider::Custom("test".into()))
            .api(Api::AnthropicMessages)
            .timestamp_ms(Timestamp::zero())
            .build()
    }

    #[test]
    fn the_sent_max_tokens_match_the_request_body() {
        let catalogue = crate::models::anthropic_models();
        let opus = catalogue
            .iter()
            .find(|model| model.id.as_str() == "claude-opus-5-5")
            .unwrap()
            .clone();
        let budget = Model {
            reasoning: true,
            ..test_model()
        };
        let thinking = StreamOptions {
            thinking: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let capped = StreamOptions {
            max_tokens: Some(TokenCount::new(10_000)),
            ..Default::default()
        };
        for (model, options) in [
            (&opus, StreamOptions::default()),
            (&opus, thinking.clone()),
            (&budget, thinking),
            (&budget, StreamOptions::default()),
            (&opus, capped),
        ] {
            for oauth in [true, false] {
                let body = build_request_body(model, &None, &[], &[], &options, oauth);
                assert_eq!(
                    sent_max_tokens(model, &options, oauth),
                    body.max_tokens,
                    "{} oauth={oauth}",
                    model.id
                );
            }
        }
        assert_eq!(
            sent_max_tokens(&opus, &StreamOptions::default(), true),
            128_000
        );
    }
}
