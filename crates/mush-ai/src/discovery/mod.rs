pub mod anthropic;
pub mod cache;
pub mod codex;
pub mod merge;
pub mod openai;
pub mod openrouter;
pub mod orchestrate;
pub mod zai;

pub use cache::{DiscoveredEntry, DiscoveryCache, ProviderCache, cache_path};
pub use merge::{
    MergedModel, ModelSource, find_merged_model_by_id, merge, merged_catalogue, merged_models,
    resolve_model_by_id, shared_catalogue,
};
pub use orchestrate::{
    DiscoveryOutcome, DiscoveryRunResult, DiscoveryRunSummary, refresh_and_save,
};

use std::time::SystemTime;

use crate::types::{Model, Provider};

#[derive(Debug, Clone)]
pub struct DiscoveryReport {
    pub provider: Provider,
    pub fetched_at: SystemTime,
    pub models: Vec<DiscoveredModel>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredModel {
    pub model: Model,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl From<Model> for DiscoveredModel {
    fn from(model: Model) -> Self {
        Self { model, raw: None }
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum DiscoveryError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("http middleware failed: {0}")]
    Middleware(String),

    #[error("authentication failed ({status}): {body}")]
    Auth { status: u16, body: String },

    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },

    #[error("malformed response: {0}")]
    Malformed(String),

    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),
}

impl From<reqwest_middleware::Error> for DiscoveryError {
    fn from(err: reqwest_middleware::Error) -> Self {
        match err {
            reqwest_middleware::Error::Reqwest(e) => Self::Http(e),
            reqwest_middleware::Error::Middleware(e) => Self::Middleware(e.to_string()),
        }
    }
}

pub(crate) fn discovered<E: serde::de::DeserializeOwned>(
    raw: serde_json::Value,
    to_model: impl FnOnce(E) -> Model,
) -> Result<DiscoveredModel, DiscoveryError> {
    let entry = E::deserialize(&raw).map_err(|e| DiscoveryError::Malformed(e.to_string()))?;
    Ok(DiscoveredModel {
        model: to_model(entry),
        raw: Some(raw),
    })
}

pub(crate) async fn execute_request(
    request: reqwest_middleware::RequestBuilder,
) -> Result<String, DiscoveryError> {
    let resp = request.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if status.is_success() {
        return Ok(body);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(DiscoveryError::Auth {
            status: status.as_u16(),
            body,
        });
    }
    Err(DiscoveryError::Upstream {
        status: status.as_u16(),
        body,
    })
}
