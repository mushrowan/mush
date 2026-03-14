//! anthropic oauth usage polling
//!
//! fetches rolling usage limits (5-hour and 7-day windows) from
//! the undocumented /api/oauth/usage endpoint. results are cached
//! to avoid hitting the aggressive rate limit.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::OAuthError;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA_HEADER: &str = "oauth-2025-04-20";
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// a single usage window (5-hour or 7-day)
#[derive(Debug, Clone)]
pub struct UsageWindow {
    /// percentage used (0.0 - 100.0)
    pub utilization: f32,
    /// when the window resets (end of the rolling period)
    pub resets_at: DateTime<Utc>,
}

impl UsageWindow {
    /// ideal pace: how far through the window we are, as a percentage
    pub fn pace(&self, window: chrono::TimeDelta) -> f32 {
        let now = Utc::now();
        let start = self.resets_at - window;
        let elapsed = (now - start).num_seconds().max(0) as f32;
        let total = window.num_seconds() as f32;
        if total <= 0.0 {
            return 0.0;
        }
        (elapsed / total * 100.0).clamp(0.0, 100.0)
    }
}

/// usage data from the anthropic api
#[derive(Debug, Clone)]
pub struct OAuthUsage {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
}

impl OAuthUsage {
    pub const FIVE_HOUR: chrono::TimeDelta = chrono::TimeDelta::hours(5);
    pub const SEVEN_DAY: chrono::TimeDelta = chrono::TimeDelta::weeks(1);
}

/// cached usage poller with a shared http client
pub struct UsagePoller {
    cached: Arc<Mutex<Option<CachedResult>>>,
    client: reqwest::Client,
}

enum CachedResult {
    Ok {
        data: OAuthUsage,
        fetched_at: Instant,
    },
    Err {
        fetched_at: Instant,
    },
}

impl UsagePoller {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            cached: Arc::new(Mutex::new(None)),
            client,
        }
    }

    /// get usage data, returning cached if fresh enough.
    /// returns None if no oauth token is available or the api errors.
    pub async fn get_usage(&self) -> Option<OAuthUsage> {
        // check cache under a short lock
        {
            let cache = self.cached.lock().ok()?;
            match cache.as_ref() {
                Some(CachedResult::Ok { data, fetched_at })
                    if fetched_at.elapsed() < MIN_POLL_INTERVAL =>
                {
                    return Some(data.clone());
                }
                Some(CachedResult::Err { fetched_at })
                    if fetched_at.elapsed() < MIN_POLL_INTERVAL =>
                {
                    return None;
                }
                _ => {}
            }
        }

        match self.fetch().await {
            Ok(usage) => {
                if let Ok(mut cache) = self.cached.lock() {
                    *cache = Some(CachedResult::Ok {
                        data: usage.clone(),
                        fetched_at: Instant::now(),
                    });
                }
                Some(usage)
            }
            Err(_) => {
                // cache the error so we don't retry every second
                if let Ok(mut cache) = self.cached.lock() {
                    // preserve stale data if we had it
                    let had_data = matches!(cache.as_ref(), Some(CachedResult::Ok { .. }));
                    if !had_data {
                        *cache = Some(CachedResult::Err {
                            fetched_at: Instant::now(),
                        });
                    }
                }
                // return stale data if available
                self.cached.lock().ok().and_then(|c| match c.as_ref() {
                    Some(CachedResult::Ok { data, .. }) => Some(data.clone()),
                    _ => None,
                })
            }
        }
    }

    /// force a refresh after an api call completes
    pub async fn refresh(&self) {
        if let Ok(usage) = self.fetch().await
            && let Ok(mut cache) = self.cached.lock()
        {
            *cache = Some(CachedResult::Ok {
                data: usage,
                fetched_at: Instant::now(),
            });
        }
    }

    async fn fetch(&self) -> Result<OAuthUsage, OAuthError> {
        let token = super::get_anthropic_oauth_token()
            .await?
            .ok_or_else(|| OAuthError::TokenExchange("no anthropic oauth token".into()))?;

        let response = self
            .client
            .get(USAGE_URL)
            .header("Authorization", format!("Bearer {token}"))
            .header("anthropic-beta", BETA_HEADER)
            .header("Content-Type", "application/json")
            .timeout(Duration::from_secs(5))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(OAuthError::TokenExchange(format!(
                "usage api returned {status}: {text}"
            )));
        }

        let raw: RawUsageResponse = response
            .json()
            .await
            .map_err(|e| OAuthError::TokenExchange(format!("failed to parse usage: {e}")))?;

        Ok(OAuthUsage {
            five_hour: raw.five_hour.and_then(|w| w.try_into_window()),
            seven_day: raw.seven_day.and_then(|w| w.try_into_window()),
        })
    }
}

// -- response parsing --

#[derive(Deserialize)]
struct RawUsageResponse {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
}

#[derive(Deserialize)]
struct RawWindow {
    utilization: f32,
    resets_at: String,
}

impl RawWindow {
    fn try_into_window(self) -> Option<UsageWindow> {
        let resets_at = DateTime::parse_from_rfc3339(&self.resets_at)
            .ok()?
            .with_timezone(&Utc);
        Some(UsageWindow {
            utilization: self.utilization,
            resets_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Datelike;

    #[test]
    fn parse_full_response() {
        let json = r#"{
            "five_hour": {
                "utilization": 37.0,
                "resets_at": "2026-02-08T04:59:59.000000+00:00"
            },
            "seven_day": {
                "utilization": 26.0,
                "resets_at": "2026-02-12T14:59:59.771647+00:00"
            },
            "seven_day_opus": null,
            "seven_day_sonnet": null,
            "extra_usage": {
                "is_enabled": false,
                "monthly_limit": null,
                "used_credits": null,
                "utilization": null
            }
        }"#;

        let raw: RawUsageResponse = serde_json::from_str(json).unwrap();
        let five = raw.five_hour.unwrap().try_into_window().unwrap();
        assert!((five.utilization - 37.0).abs() < 0.1);
        assert_eq!(five.resets_at.year(), 2026);
        assert_eq!(five.resets_at.month(), 2);

        let seven = raw.seven_day.unwrap().try_into_window().unwrap();
        assert!((seven.utilization - 26.0).abs() < 0.1);
    }

    #[test]
    fn parse_null_windows() {
        let json = r#"{ "five_hour": null, "seven_day": null }"#;
        let raw: RawUsageResponse = serde_json::from_str(json).unwrap();
        assert!(raw.five_hour.is_none());
        assert!(raw.seven_day.is_none());
    }

    #[test]
    fn pace_midway_through_window() {
        let window = UsageWindow {
            utilization: 30.0,
            resets_at: Utc::now() + chrono::TimeDelta::minutes(150),
        };
        let pace = window.pace(OAuthUsage::FIVE_HOUR);
        assert!((pace - 50.0).abs() < 2.0, "pace was {pace}, expected ~50");
    }

    #[test]
    fn pace_at_start() {
        let window = UsageWindow {
            utilization: 0.0,
            resets_at: Utc::now() + OAuthUsage::FIVE_HOUR - chrono::TimeDelta::seconds(30),
        };
        let pace = window.pace(OAuthUsage::FIVE_HOUR);
        assert!(pace < 2.0, "pace was {pace}, expected near 0");
    }

    #[test]
    fn pace_near_end() {
        let window = UsageWindow {
            utilization: 80.0,
            resets_at: Utc::now() + chrono::TimeDelta::minutes(10),
        };
        let pace = window.pace(OAuthUsage::FIVE_HOUR);
        assert!(pace > 95.0, "pace was {pace}, expected near 100");
    }

    #[test]
    fn poller_creates() {
        let _poller = UsagePoller::new(reqwest::Client::new());
    }
}
