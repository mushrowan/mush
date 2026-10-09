use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use crate::stream::StreamEvent;
use crate::types::{Api, Model, StreamOptions};

pub type EventStream = Pin<Box<dyn futures::Stream<Item = StreamEvent> + Send>>;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum ProviderError {
    #[error("no provider registered for api: {0:?}")]
    #[diagnostic(help("register a provider for this api type before streaming"))]
    NoProvider(Api),

    #[error("missing api key for provider: {0}")]
    #[diagnostic(help("set the appropriate env var or pass an api key in options"))]
    MissingApiKey(crate::types::Provider),

    #[error(
        "the {provider} login '{account}' has expired and could not be refreshed ({reason}). run /login {provider} {account} to sign in again"
    )]
    LoginExpired {
        provider: String,
        account: crate::types::AccountLabel,
        reason: String,
    },

    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("request middleware failed: {0}")]
    Middleware(String),

    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),

    #[error("{api} returned {status} ({content_type}): {body}")]
    #[diagnostic(help("check your api key and model id"))]
    ApiError {
        api: &'static str,
        status: reqwest::StatusCode,
        content_type: String,
        body: String,
        retry_after: Option<std::time::Duration>,
    },

    #[error("{0}")]
    Other(String),
}

impl From<reqwest_middleware::Error> for ProviderError {
    fn from(err: reqwest_middleware::Error) -> Self {
        match err {
            reqwest_middleware::Error::Reqwest(e) => Self::Request(e),
            reqwest_middleware::Error::Middleware(e) => Self::Middleware(e.to_string()),
        }
    }
}

const MAX_ERROR_BODY: usize = 2000;

pub fn truncate_error_body(body: &str) -> String {
    if body.len() <= MAX_ERROR_BODY {
        body.to_string()
    } else {
        format!(
            "{}... ({} bytes total)",
            &body[..body.floor_char_boundary(MAX_ERROR_BODY)],
            body.len()
        )
    }
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Request(e) => {
                e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() || e.is_decode()
            }
            Self::ApiError { status, body, .. } => {
                status.as_u16() == 429
                    || status.as_u16() >= 500
                    || classify_stream_error_message(body) == StreamErrorKind::Refusal
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::ApiError { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

pub const RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

#[must_use]
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(ms) = header("retry-after-ms").and_then(|raw| raw.trim().parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some(seconds_or_forever(ms / 1000.0));
    }
    let raw = header("retry-after")?.trim();
    if let Ok(seconds) = raw.parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then(|| seconds_or_forever(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    Some(
        (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
            .to_std()
            .unwrap_or_default(),
    )
}

fn seconds_or_forever(seconds: f64) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(seconds).unwrap_or(std::time::Duration::MAX)
}

#[must_use]
pub fn retry_delay(
    backoff: std::time::Duration,
    retry_after: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    let unit =
        (u64::from_le_bytes(crate::ids::random_bytes::<8>()) >> 11) as f64 / (1u64 << 53) as f64;
    jittered_retry_delay(backoff, retry_after, unit)
}

fn jittered_retry_delay(
    backoff: std::time::Duration,
    retry_after: Option<std::time::Duration>,
    unit: f64,
) -> Option<std::time::Duration> {
    if retry_after.is_some_and(|wait| wait > RETRY_MAX_DELAY) {
        return None;
    }
    if let Some(wait) = retry_after
        && wait >= backoff
    {
        return Some(wait);
    }
    let jittered = backoff.mul_f64(0.75 + 0.5 * unit.clamp(0.0, 1.0));
    Some(jittered.max(retry_after.unwrap_or_default()))
}

pub const MAX_RETRIES: u32 = 5;

#[must_use]
pub fn retry_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs(1 << attempt.saturating_sub(1).min(4))
}

pub const STREAM_ENDED_EARLY: &str = "stream ended without a final message";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    Transient,
    Refusal,
    Permanent,
}

impl StreamErrorKind {
    #[must_use]
    pub fn is_retryable(self) -> bool {
        match self {
            Self::Transient | Self::Refusal => true,
            Self::Permanent => false,
        }
    }
}

#[must_use]
pub fn classify_stream_error_message(message: &str) -> StreamErrorKind {
    let lower = message.to_ascii_lowercase();
    let matches = |patterns: &[&str]| patterns.iter().any(|pat| lower.contains(pat));
    if matches(&[
        "claude declined to respond",
        "cyber_policy",
        "flagged as potentially violating",
    ]) {
        StreamErrorKind::Refusal
    } else if matches(&[
        "overloaded",
        "rate_limit",
        "rate limit",
        "internal_server_error",
        "internal server error",
        "service_unavailable",
        "service unavailable",
        "timeout",
        "temporarily unavailable",
        "error decoding response body",
        "error reading a body from connection",
        "unexpected eof during chunk size line",
        "connection reset",
        "broken pipe",
        "connection closed before message completed",
        STREAM_ENDED_EARLY,
    ]) {
        StreamErrorKind::Transient
    } else {
        StreamErrorKind::Permanent
    }
}

#[must_use]
pub fn is_context_overflow_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context length exceeded",
        "context window",
        "too many input tokens",
        "input tokens exceed",
        "maximum context",
        "prompt is too long",
    ]
    .iter()
    .any(|pat| lower.contains(pat))
}

#[must_use]
pub fn is_retryable_stream_error_message(message: &str) -> bool {
    classify_stream_error_message(message).is_retryable()
}

pub fn retry_stream_error(error: &str, attempt: u32) -> bool {
    match classify_stream_error_message(error) {
        StreamErrorKind::Refusal => {
            tracing::warn!(reason = %error, attempt, "the provider refused the request, retrying");
            true
        }
        StreamErrorKind::Transient => {
            tracing::warn!(%error, attempt, "retryable stream error, retrying");
            true
        }
        StreamErrorKind::Permanent => false,
    }
}

#[derive(Debug, Clone)]
pub struct LlmContext {
    pub system_prompt: Option<String>,
    pub messages: Vec<crate::types::Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[async_trait::async_trait]
pub trait ApiProvider: Send + Sync {
    fn api(&self) -> Api;

    async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Result<EventStream, ProviderError>;
}

#[derive(Default, Clone)]
pub struct ApiRegistry {
    providers: HashMap<Api, Arc<dyn ApiProvider>>,
}

impl ApiRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Box<dyn ApiProvider>) {
        let api = provider.api();
        self.providers.insert(api, Arc::from(provider));
    }

    pub fn get(&self, api: Api) -> Option<&dyn ApiProvider> {
        self.providers.get(&api).map(|p| p.as_ref())
    }

    #[tracing::instrument(name = "llm_stream", skip_all, fields(model = %model.id, api = ?model.api))]
    pub async fn stream(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Result<EventStream, ProviderError> {
        let provider = self
            .get(model.api)
            .ok_or(ProviderError::NoProvider(model.api))?;
        provider.stream(model, context, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenCount;

    #[test]
    fn empty_registry_returns_none() {
        let registry = ApiRegistry::new();
        assert!(registry.get(Api::AnthropicMessages).is_none());
    }

    #[tokio::test]
    async fn stream_without_provider_returns_error() {
        let registry = ApiRegistry::new();
        let model = crate::types::Model {
            id: "test".into(),
            name: "test".into(),
            api: Api::AnthropicMessages,
            provider: crate::types::Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            input: vec![crate::types::InputModality::Text],
            cost: crate::types::ModelCost::default(),
            context_window: TokenCount::new(200_000),
            effective_context_window: None,
            max_output_tokens: TokenCount::new(8192),
            supports_adaptive_thinking: false,
            supported_thinking_levels: Vec::new(),
            default_thinking_level: None,
        };
        let ctx = LlmContext {
            system_prompt: None,
            messages: vec![],
            tools: vec![],
        };
        let err = registry
            .stream(&model, &ctx, &StreamOptions::default())
            .await;
        assert!(err.is_err());
    }

    #[test]
    fn retryable_429_rate_limit() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            content_type: "application/json".into(),
            body: "rate limited".into(),
            retry_after: None,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_500_server_error() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            content_type: "application/json".into(),
            body: "internal error".into(),
            retry_after: None,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_502_bad_gateway() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::BAD_GATEWAY,
            content_type: "text/html".into(),
            body: "bad gateway".into(),
            retry_after: None,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_503_service_unavailable() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            content_type: "text/plain".into(),
            body: "unavailable".into(),
            retry_after: None,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn not_retryable_400_bad_request() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::BAD_REQUEST,
            content_type: "application/json".into(),
            body: "bad request".into(),
            retry_after: None,
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn retryable_400_refusal() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::BAD_REQUEST,
            content_type: "application/json".into(),
            body: r#"{"error":{"type":"invalid_request","code":"cyber_policy"}}"#.into(),
            retry_after: None,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn not_retryable_401_unauthorized() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::UNAUTHORIZED,
            content_type: "application/json".into(),
            body: "unauthorized".into(),
            retry_after: None,
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn not_retryable_403_forbidden() {
        let err = ProviderError::ApiError {
            api: "test",
            status: reqwest::StatusCode::FORBIDDEN,
            content_type: "application/json".into(),
            body: "forbidden".into(),
            retry_after: None,
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn retry_after_reads_milliseconds_seconds_and_dates() {
        use reqwest::header::{HeaderMap, HeaderValue};
        use std::time::Duration;
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, HeaderValue::from_str(value).unwrap());
            }
            map
        };
        assert_eq!(
            parse_retry_after(&headers(&[
                ("retry-after-ms", "1500"),
                ("retry-after", "9")
            ])),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            parse_retry_after(&headers(&[("retry-after", "7")])),
            Some(Duration::from_secs(7))
        );
        let later = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc2822();
        let from_date = parse_retry_after(&headers(&[("retry-after", &later)])).unwrap();
        assert!(from_date > Duration::from_secs(25) && from_date <= Duration::from_secs(30));
        assert_eq!(
            parse_retry_after(&headers(&[("retry-after", "soon")])),
            None
        );
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
        for (name, value) in [("retry-after", "1e300"), ("retry-after-ms", "1e300")] {
            let wait = parse_retry_after(&headers(&[(name, value)]));
            assert_eq!(wait, Some(Duration::MAX), "{name}");
            assert_eq!(retry_delay(Duration::from_secs(1), wait), None);
        }
    }

    #[test]
    fn retry_delays_jitter_the_backoff_and_respect_the_server_wait() {
        use std::time::Duration;
        let backoff = Duration::from_secs(4);
        assert_eq!(
            jittered_retry_delay(backoff, None, 0.0),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            jittered_retry_delay(backoff, None, 1.0),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            jittered_retry_delay(backoff, Some(Duration::from_secs(10)), 0.5),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            jittered_retry_delay(backoff, Some(Duration::from_millis(3500)), 0.0),
            Some(Duration::from_millis(3500))
        );
        assert_eq!(
            jittered_retry_delay(backoff, Some(Duration::from_secs(61)), 0.5),
            None
        );
        let delay = retry_delay(backoff, None).unwrap();
        assert!(delay >= Duration::from_secs(3) && delay <= Duration::from_secs(5));
    }

    #[test]
    fn truncate_error_body_short() {
        let body = "short error";
        assert_eq!(truncate_error_body(body), "short error");
    }

    #[tokio::test]
    async fn truncated_response_body_is_retryable() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            let mut socket = reader.into_inner();
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });
        let error = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap_err();
        assert!(error.is_body() || error.is_decode());
        assert!(ProviderError::Request(error).is_retryable());
        server.await.unwrap();
    }

    #[test]
    fn truncate_error_body_long() {
        let body = "x".repeat(5000);
        let truncated = truncate_error_body(&body);
        assert!(truncated.len() < body.len());
        assert!(truncated.contains("5000 bytes total"));
    }

    #[test]
    fn truncate_error_body_cuts_a_multibyte_body_on_a_char_boundary() {
        let body = "…".repeat(MAX_ERROR_BODY);
        let truncated = truncate_error_body(&body);
        assert!(truncated.starts_with('…'));
        assert!(truncated.contains(&format!("{} bytes total", body.len())));
    }

    #[test]
    fn not_retryable_no_provider() {
        let err = ProviderError::NoProvider(Api::AnthropicMessages);
        assert!(!err.is_retryable());
    }

    #[test]
    fn not_retryable_missing_key() {
        let err = ProviderError::MissingApiKey(crate::types::Provider::Anthropic);
        assert!(!err.is_retryable());
    }

    #[test]
    fn not_retryable_other() {
        let err = ProviderError::Other("some error".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn retryable_stream_error_messages() {
        assert!(is_retryable_stream_error_message("Overloaded"));
        assert!(is_retryable_stream_error_message(
            "rate_limit_exceeded: please slow down"
        ));
        assert!(is_retryable_stream_error_message("rate limit reached"));
        assert!(is_retryable_stream_error_message(
            "internal_server_error from upstream"
        ));
        assert!(is_retryable_stream_error_message("Service Unavailable"));
        assert!(is_retryable_stream_error_message("Request timeout"));
        assert!(is_retryable_stream_error_message("OVERLOADED"));
        for message in [
            "error decoding response body",
            "error reading a body from connection",
            "stream error: error decoding response body: unexpected EOF during chunk size line",
        ] {
            assert!(is_retryable_stream_error_message(message), "{message}");
        }
    }

    #[test]
    fn a_prompt_too_long_error_is_a_context_overflow() {
        assert!(is_context_overflow_message(
            r#"anthropic returned 400 Bad Request (application/json): {"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1011825 tokens > 1000000 maximum"}}"#
        ));
        assert!(is_context_overflow_message(
            "context_length_exceeded: too many input tokens"
        ));
        assert!(!is_context_overflow_message(
            "invalid_request_error: missing field"
        ));
    }

    #[test]
    fn non_retryable_stream_error_messages() {
        assert!(!is_retryable_stream_error_message(
            "invalid_request_error: missing field"
        ));
        assert!(!is_retryable_stream_error_message(
            "authentication_error: bad key"
        ));
        assert!(!is_retryable_stream_error_message(
            "permission_denied for this model"
        ));
        assert!(!is_retryable_stream_error_message(""));
        assert!(!is_retryable_stream_error_message(
            "invalid_request_error: invalid_prompt: the prompt is too long"
        ));
    }

    #[test]
    fn provider_refusals_are_retryable_refusals() {
        for message in [
            "claude declined to respond (This request triggered restrictions on violative cyber \
             content and was blocked under Anthropic's Usage Policy.). retry on another model",
            "claude declined to respond. retry on another model",
            "invalid_request: cyber_policy: This content was flagged for possible cybersecurity risk.",
            "invalid_request_error: invalid_prompt: Invalid prompt: your prompt was flagged as \
             potentially violating our usage policy. Please try again with a different prompt",
        ] {
            assert_eq!(
                classify_stream_error_message(message),
                StreamErrorKind::Refusal,
                "{message}"
            );
            assert!(is_retryable_stream_error_message(message), "{message}");
        }
    }

    #[test]
    fn dropped_connections_are_transient() {
        for message in [
            "error decoding response body: error reading a body from connection: \
             Connection reset by peer (os error 104)",
            "error sending request: Broken pipe (os error 32)",
            "connection closed before message completed",
            STREAM_ENDED_EARLY,
        ] {
            assert_eq!(
                classify_stream_error_message(message),
                StreamErrorKind::Transient,
                "{message}"
            );
        }
        assert_eq!(
            classify_stream_error_message("authentication_error: bad key"),
            StreamErrorKind::Permanent
        );
    }

    #[test]
    fn retry_backoff_doubles_from_one_second_up_to_sixteen() {
        let secs: Vec<u64> = (1..=MAX_RETRIES + 2)
            .map(|attempt| retry_backoff(attempt).as_secs())
            .collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 16, 16]);
    }
}
