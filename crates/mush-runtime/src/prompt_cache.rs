use std::time::Duration;

use mush_ai::types::Timestamp;

#[derive(Debug, Clone, Copy, Default)]
pub struct PromptCache {
    turn_lifetime: Option<Duration>,
    sent: Option<Warm>,
    warm: Option<Warm>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Warm {
    since: Timestamp,
    lifetime: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Unknown,
    Warm,
    Due,
    Expired,
}

impl PromptCache {
    pub fn turn_started(&mut self, lifetime: Option<Duration>) {
        self.turn_lifetime = lifetime;
        self.sent = None;
        if lifetime.is_none() {
            self.warm = None;
        }
    }

    pub fn request_sent(&mut self, at: Timestamp) {
        self.sent = self.turn_lifetime.map(|lifetime| Warm {
            since: at,
            lifetime,
        });
    }

    pub fn request_answered(&mut self) {
        if let Some(sent) = self.sent.take() {
            self.warm = Some(sent);
        }
    }

    pub fn forget(&mut self) {
        self.sent = None;
        self.warm = None;
    }

    #[must_use]
    pub fn expiry(&self, now: Timestamp) -> Expiry {
        let Some(warm) = self.warm else {
            return Expiry::Unknown;
        };
        let age = Duration::from_millis(now.as_ms().saturating_sub(warm.since.as_ms()));
        if age >= warm.lifetime {
            Expiry::Expired
        } else if age >= mush_ai::prompt_cache::reuse_deadline(warm.lifetime) {
            Expiry::Due
        } else {
            Expiry::Warm
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn later(at: Timestamp, minutes: u64) -> Timestamp {
        Timestamp::from_ms(at.as_ms() + minutes * 60_000)
    }

    #[test]
    fn an_answered_request_is_warm_until_its_deadline_then_due_until_it_expires() {
        let sent = Timestamp::now();
        let mut cache = PromptCache::default();
        cache.turn_started(Some(HOUR));
        cache.request_sent(sent);
        cache.request_answered();

        assert_eq!(cache.expiry(later(sent, 54)), Expiry::Warm);
        assert_eq!(cache.expiry(later(sent, 56)), Expiry::Due);
        assert_eq!(cache.expiry(later(sent, 61)), Expiry::Expired);
        assert_eq!(cache.expiry(Timestamp::zero()), Expiry::Warm);
        cache.forget();
        assert_eq!(cache.expiry(later(sent, 56)), Expiry::Unknown);
    }

    #[test]
    fn only_an_answered_request_warms_the_cache_and_a_turn_without_a_lifetime_forgets_it() {
        let first = Timestamp::now();
        let mut cache = PromptCache::default();
        cache.turn_started(Some(HOUR));
        cache.request_sent(first);
        assert_eq!(cache.expiry(later(first, 56)), Expiry::Unknown);

        cache.request_answered();
        assert_eq!(cache.expiry(later(first, 56)), Expiry::Due);

        cache.turn_started(Some(HOUR));
        cache.request_sent(later(first, 30));
        assert_eq!(cache.expiry(later(first, 56)), Expiry::Due);
        cache.request_answered();
        assert_eq!(cache.expiry(later(first, 56)), Expiry::Warm);

        cache.turn_started(None);
        cache.request_sent(later(first, 40));
        cache.request_answered();
        assert_eq!(cache.expiry(later(first, 56)), Expiry::Unknown);
    }
}
