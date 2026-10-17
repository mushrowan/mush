use std::time::Instant;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Clock {
    frozen: Option<Instant>,
}

impl Clock {
    #[must_use]
    pub fn frozen_at(at: Instant) -> Self {
        Self { frozen: Some(at) }
    }

    #[must_use]
    pub fn now(self) -> Instant {
        self.frozen.unwrap_or_else(Instant::now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_clock_keeps_its_instant_and_the_default_follows_real_time() {
        let start = Instant::now();
        let frozen = Clock::frozen_at(start);

        assert_eq!(frozen.now(), start);
        assert_eq!(frozen.now(), start);
        assert!(Clock::default().now() >= start);
    }
}
