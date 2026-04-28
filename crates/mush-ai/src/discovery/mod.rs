//! dynamic model discovery
//!
//! providers expose a `/v1/models` endpoint that lists their currently
//! available models. mush keeps a static catalogue (in [`crate::models`])
//! but discovery lets us pick up new releases without a code change.
//!
//! each backend implements [`ModelDiscovery`]; tests rely on the
//! pure-function parsers (`parse_*`) so we don't need a live network.

pub mod anthropic;
pub mod openrouter;

use std::time::SystemTime;

use crate::types::{Model, Provider};

/// outcome of a discovery fetch for one provider
#[derive(Debug, Clone)]
pub struct DiscoveryReport {
    pub provider: Provider,
    pub fetched_at: SystemTime,
    pub models: Vec<Model>,
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum DiscoveryError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("authentication failed ({status}): {body}")]
    Auth { status: u16, body: String },

    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },

    #[error("malformed response: {0}")]
    Malformed(String),

    #[error("missing credentials for {0}")]
    MissingCredentials(Provider),

    #[error("invalid header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),
}

/// fetch the list of models a provider currently offers
pub trait ModelDiscovery: Send + Sync {
    /// the provider this discovery instance describes
    fn provider(&self) -> Provider;

    /// hit the upstream `/v1/models` endpoint and return parsed models
    fn fetch(
        &self,
    ) -> impl std::future::Future<Output = Result<DiscoveryReport, DiscoveryError>> + Send;
}
