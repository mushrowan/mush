use serde::{Deserialize, Serialize};

pub mod admission;
pub mod inbox;
pub mod review;
pub mod status;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Quota,
    RateLimit,
    Authentication,
    Transport,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Continue,
    Fallback,
    Retry,
    Stop,
}

pub fn decide(state: &str, failure: Option<Failure>, fallback: bool, retries: u32) -> Action {
    if matches!(state, "done" | "blocked" | "cancelled") {
        return Action::Stop;
    }
    match failure {
        None => Action::Continue,
        Some(Failure::Quota) if !fallback => Action::Fallback,
        Some(Failure::RateLimit | Failure::Transport) if retries < 2 => Action::Retry,
        Some(
            Failure::Quota
            | Failure::RateLimit
            | Failure::Authentication
            | Failure::Transport
            | Failure::Other,
        ) => Action::Stop,
    }
}

pub fn classify(error: &str) -> Failure {
    let error = error.to_ascii_lowercase();
    if [
        "usage_limit_reached",
        "insufficient_quota",
        "usage_not_included",
        "credit_balance_exhausted",
        "organization_spend_limit_exceeded",
        "project_spend_limit_exceeded",
        "organization_usage_limit_exceeded",
        "enforced_spend_limit_reached",
        "you have reached your specified api usage limits",
        "you have reached your specified workspace api usage limits",
        "you've hit your usage limit",
        "reached your usage limit",
        "credit balance is too low",
        "quota exceeded",
        "usage not included",
    ]
    .iter()
    .any(|s| error.contains(s))
    {
        Failure::Quota
    } else if ["rate_limit", "rate limit", "too many requests", "http 429"]
        .iter()
        .any(|s| error.contains(s))
    {
        Failure::RateLimit
    } else if [
        "unauthorized",
        "authentication_error",
        "invalid_api_key",
        "token expired",
        "http 401",
    ]
    .iter()
    .any(|s| error.contains(s))
    {
        Failure::Authentication
    } else if [
        "error decoding response body",
        "error reading a body from connection",
        "unexpected eof during chunk size line",
    ]
    .iter()
    .any(|s| error.contains(s))
    {
        Failure::Transport
    } else {
        Failure::Other
    }
}

#[cfg(test)]
pub(crate) fn tempdir() -> std::io::Result<tempfile::TempDir> {
    if cfg!(target_os = "macos") {
        tempfile::tempdir_in("/private/tmp")
    } else {
        tempfile::tempdir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_and_blocked_workers_stay_stopped() {
        assert_eq!(decide("done", None, false, 0), Action::Stop);
        assert_eq!(decide("blocked", None, false, 0), Action::Stop);
    }

    #[test]
    fn unfinished_success_continues() {
        assert_eq!(decide("working", None, false, 0), Action::Continue);
    }

    #[test]
    fn quota_switches_once_and_then_stops() {
        assert_eq!(
            decide("working", Some(Failure::Quota), false, 0),
            Action::Fallback
        );
        assert_eq!(
            decide("working", Some(Failure::Quota), true, 0),
            Action::Stop
        );
    }

    #[test]
    fn transient_failures_have_a_retry_budget() {
        assert_eq!(
            decide("working", Some(Failure::RateLimit), false, 0),
            Action::Retry
        );
        assert_eq!(
            decide("working", Some(Failure::RateLimit), false, 2),
            Action::Stop
        );
        assert_eq!(
            decide("working", Some(Failure::Authentication), false, 0),
            Action::Stop
        );
    }

    #[test]
    fn quota_is_distinct_from_rate_limits_and_expired_auth() {
        for error in [
            "usage_limit_reached",
            "insufficient_quota",
            "You have reached your usage limit",
            "Your credit balance is too low",
            "enforced_spend_limit_reached",
            "organization_spend_limit_exceeded",
        ] {
            assert_eq!(classify(error), Failure::Quota, "{error}");
        }
        assert_eq!(classify("rate_limit_exceeded"), Failure::RateLimit);
        assert_eq!(classify("429 too many requests"), Failure::RateLimit);
        assert_eq!(classify("401 unauthorized"), Failure::Authentication);
        assert_eq!(classify("unknown model: test"), Failure::Other);
    }

    #[test]
    fn response_body_errors_retry_with_a_bound() {
        for message in [
            "error decoding response body",
            "stream error: error decoding response body: unexpected EOF during chunk size line",
            "error reading a body from connection",
        ] {
            assert_eq!(classify(message), Failure::Transport);
            for fallback in [false, true] {
                assert_eq!(
                    decide("working", Some(classify(message)), fallback, 0),
                    Action::Retry
                );
                assert_eq!(
                    decide("working", Some(classify(message)), fallback, 1),
                    Action::Retry
                );
                assert_eq!(
                    decide("working", Some(classify(message)), fallback, 2),
                    Action::Stop
                );
            }
        }
        assert_eq!(classify("invalid_request: cyber_policy"), Failure::Other);
    }
}
