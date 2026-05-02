//! openrouter `/generation` cost reconciliation
//!
//! after a streamed turn lands, openrouter exposes a richer set of
//! billing data via `GET /generation?id=<id>` than what's carried in
//! the streaming response itself: canonical `total_cost` (incl. cache
//! discounts and per-provider markups), the underlying provider name
//! that actually served the request, native (upstream) token counts,
//! and timing breakdown.
//!
//! the streamer's running tally is an estimate. fetching the
//! `/generation` record once a turn finishes lets us replace that
//! estimate with the value openrouter actually billed, which matters
//! for byok / auto-routed setups where the upstream price isn't
//! deterministic from the model id.
//!
//! the actual http call lives in `openrouter_rs::api::generation`; we
//! convert its non-exhaustive struct into a small mush-typed result
//! so the runner doesn't have to import the SDK directly.

use openrouter_rs::api::generation::GenerationData;

use crate::types::{Dollars, TokenCount};

/// distilled cost reconciliation data for a single openrouter
/// generation. only the fields mush actually consumes; if a future
/// caller needs more, extend rather than expose the SDK type
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationCost {
    /// generation id this record belongs to
    pub id: String,
    /// canonical billed amount in usd, including any cache discounts
    /// openrouter applied
    pub total_cost: Dollars,
    /// underlying provider that actually served the request (eg.
    /// `anthropic`, `groq`, `fireworks`). useful for showing the user
    /// where their auto-routed turn landed
    pub provider_name: Option<String>,
    /// native (upstream) prompt tokens, when available. these can
    /// differ from openrouter's normalised `tokens_prompt` when the
    /// underlying provider uses a different tokeniser
    pub native_tokens_prompt: Option<TokenCount>,
    pub native_tokens_completion: Option<TokenCount>,
    pub native_tokens_reasoning: Option<TokenCount>,
    /// whether the request was served via byok rather than openrouter
    /// credits. callers may want to badge this in the UI
    pub is_byok: bool,
}

/// translate the SDK's [`GenerationData`] into mush's distilled view.
/// kept separate from the network call so it's easy to fixture-test
pub fn convert_generation(data: &GenerationData) -> GenerationCost {
    GenerationCost {
        id: data.id.clone(),
        total_cost: Dollars::new(data.total_cost),
        provider_name: data.provider_name.clone(),
        native_tokens_prompt: data.native_tokens_prompt.map(|n| TokenCount::new(n as u64)),
        native_tokens_completion: data
            .native_tokens_completion
            .map(|n| TokenCount::new(n as u64)),
        native_tokens_reasoning: data
            .native_tokens_reasoning
            .map(|n| TokenCount::new(n as u64)),
        is_byok: data.is_byok,
    }
}

/// fetch the canonical record for a single openrouter generation and
/// return mush's distilled view. the SDK call drives a fresh reqwest
/// client internally; we don't currently share an http pool with it
/// because the call rate is at most once per turn. error surfacing
/// stays diagnostic-friendly via `to_string()` so callers can decide
/// whether to log or surface
pub async fn fetch_generation_cost(
    api_key: &str,
    id: impl Into<String>,
) -> Result<GenerationCost, GenerationLookupError> {
    use openrouter_rs::api::generation::get_generation;
    const BASE_URL: &str = "https://openrouter.ai/api/v1";

    let data = get_generation(BASE_URL, api_key, id)
        .await
        .map_err(|e| GenerationLookupError::Sdk(e.to_string()))?;
    Ok(convert_generation(&data))
}

#[derive(Debug, thiserror::Error)]
pub enum GenerationLookupError {
    #[error("openrouter generation lookup failed: {0}")]
    Sdk(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// representative `/generation` response shape pulled from
    /// openrouter's docs. covers a streamed completion with cache
    /// discount and native token counts so the converter exercises
    /// every populated field
    const FIXTURE: &str = r#"{
        "id": "gen-abc123",
        "total_cost": 0.04321,
        "created_at": "2026-01-01T00:00:00Z",
        "model": "anthropic/claude-sonnet-4.5",
        "origin": "https://example.com",
        "usage": 0.04,
        "is_byok": false,
        "upstream_id": "msg_xyz",
        "cache_discount": 0.001,
        "streamed": true,
        "cancelled": false,
        "provider_name": "anthropic",
        "latency": 1234,
        "generation_time": 4321,
        "tokens_prompt": 1500,
        "tokens_completion": 220,
        "native_tokens_prompt": 1505,
        "native_tokens_completion": 219,
        "native_tokens_reasoning": 412,
        "finish_reason": "stop",
        "native_finish_reason": "end_turn"
    }"#;

    #[test]
    fn convert_generation_extracts_all_user_facing_fields() {
        let data: GenerationData = serde_json::from_str(FIXTURE).expect("fixture parses");
        let cost = convert_generation(&data);
        assert_eq!(cost.id, "gen-abc123");
        assert_eq!(cost.total_cost, Dollars::new(0.04321));
        assert_eq!(cost.provider_name.as_deref(), Some("anthropic"));
        assert_eq!(cost.native_tokens_prompt, Some(TokenCount::new(1505)));
        assert_eq!(cost.native_tokens_completion, Some(TokenCount::new(219)));
        assert_eq!(cost.native_tokens_reasoning, Some(TokenCount::new(412)));
        assert!(!cost.is_byok);
    }

    #[test]
    fn convert_generation_tolerates_missing_optional_fields() {
        // upstreams that don't break out native token counts (eg.
        // proxied openai-compatible endpoints) leave most fields null.
        // the converter must default them all rather than panic
        let body = r#"{
            "id": "gen-min",
            "total_cost": 0.0,
            "created_at": "2026-01-01T00:00:00Z",
            "model": "x/y",
            "origin": "https://example.com",
            "usage": 0.0,
            "is_byok": true
        }"#;
        let data: GenerationData = serde_json::from_str(body).expect("minimal fixture parses");
        let cost = convert_generation(&data);
        assert_eq!(cost.id, "gen-min");
        assert!(cost.provider_name.is_none());
        assert!(cost.native_tokens_prompt.is_none());
        assert!(cost.native_tokens_completion.is_none());
        assert!(cost.native_tokens_reasoning.is_none());
        assert!(cost.is_byok);
    }
}
