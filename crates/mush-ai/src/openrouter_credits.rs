use openrouter_rs::api::credits::CreditsData;

use crate::types::Dollars;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreditsBalance {
    pub total_credits: Dollars,
    pub total_usage: Dollars,
}

impl CreditsBalance {
    #[must_use]
    pub fn remaining(&self) -> Dollars {
        let raw = self.total_credits.get() - self.total_usage.get();
        Dollars::new(raw.max(0.0))
    }
}

pub fn convert_credits(data: &CreditsData) -> CreditsBalance {
    CreditsBalance {
        total_credits: Dollars::new(data.total_credits),
        total_usage: Dollars::new(data.total_usage),
    }
}

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
        let body = r#"{
            "total_credits": 5.0,
            "total_usage": 12.34
        }"#;
        let data: CreditsData = serde_json::from_str(body).expect("fixture parses");
        let balance = convert_credits(&data);
        assert_eq!(balance.remaining(), Dollars::new(0.0));
    }
}
