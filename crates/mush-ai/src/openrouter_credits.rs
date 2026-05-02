//! openrouter `/credits` lookup
//!
//! returns the authenticated account's running tally of credits
//! purchased and consumed. drives the optional balance display in
//! the TUI status bar so the user knows whether their next turn will
//! deduct from prepaid credits or roll over to byok.
//!
//! the SDK call lives in `openrouter_rs::api::credits`; we convert
//! its non-exhaustive struct into a small mush-typed view so the
//! runner doesn't import the SDK directly.

use openrouter_rs::api::credits::CreditsData;

use crate::types::Dollars;

/// distilled balance snapshot for the authenticated openrouter
/// account. only the fields mush actually consumes; if a future
/// caller needs more, extend rather than expose the SDK type
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreditsBalance {
    /// total prepaid credits purchased over the account's lifetime
    pub total_credits: Dollars,
    /// total credits consumed by all completed generations
    pub total_usage: Dollars,
}

impl CreditsBalance {
    /// remaining credits = purchased - used. clamped at zero so a
    /// usage spike that nudges the account into byok territory
    /// doesn't display as a negative balance
    #[must_use]
    pub fn remaining(&self) -> Dollars {
        let raw = self.total_credits.get() - self.total_usage.get();
        Dollars::new(raw.max(0.0))
    }
}

/// translate the SDK's [`CreditsData`] into mush's distilled view.
/// kept separate from the network call so it's easy to fixture-test
pub fn convert_credits(data: &CreditsData) -> CreditsBalance {
    CreditsBalance {
        total_credits: Dollars::new(data.total_credits),
        total_usage: Dollars::new(data.total_usage),
    }
}

/// fetch the authenticated openrouter account's credit balance.
/// the SDK call drives a fresh reqwest client internally; we don't
/// share an http pool with it because the call rate is at most once
/// per session refresh. error surfacing stays diagnostic-friendly via
/// `to_string()` so callers can decide whether to log or surface
pub async fn fetch_credits(api_key: &str) -> Result<CreditsBalance, CreditsLookupError> {
    use openrouter_rs::api::credits::get_credits;
    const BASE_URL: &str = "https://openrouter.ai/api/v1";

    let data = get_credits(BASE_URL, api_key)
        .await
        .map_err(|e| CreditsLookupError::Sdk(e.to_string()))?;
    Ok(convert_credits(&data))
}

#[derive(Debug, thiserror::Error)]
pub enum CreditsLookupError {
    #[error("openrouter credits lookup failed: {0}")]
    Sdk(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// representative `/credits` response shape pulled from
    /// openrouter's docs. account has $20 purchased, $7.50 consumed,
    /// so the remaining balance should be $12.50
    const FIXTURE: &str = r#"{
        "total_credits": 20.0,
        "total_usage": 7.5
    }"#;

    #[test]
    fn convert_credits_extracts_purchased_and_used() {
        let data: CreditsData = serde_json::from_str(FIXTURE).expect("fixture parses");
        let balance = convert_credits(&data);
        assert_eq!(balance.total_credits, Dollars::new(20.0));
        assert_eq!(balance.total_usage, Dollars::new(7.5));
    }

    #[test]
    fn remaining_is_purchased_minus_used() {
        let data: CreditsData = serde_json::from_str(FIXTURE).expect("fixture parses");
        let balance = convert_credits(&data);
        assert_eq!(balance.remaining(), Dollars::new(12.5));
    }

    #[test]
    fn remaining_clamps_at_zero_when_usage_exceeds_purchases() {
        // accounts that switched to byok mid-month can show usage
        // higher than the prepaid pool. don't render a negative
        // remaining; the user already knows they're past the limit
        let body = r#"{
            "total_credits": 5.0,
            "total_usage": 12.34
        }"#;
        let data: CreditsData = serde_json::from_str(body).expect("fixture parses");
        let balance = convert_credits(&data);
        assert_eq!(balance.remaining(), Dollars::new(0.0));
    }
}
