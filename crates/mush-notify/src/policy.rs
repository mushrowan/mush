use std::time::Duration;

use crate::{Config, Mode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Unknown,
    Active,
    Locked,
    IdleSince(Duration),
}

#[derive(Clone, Copy, Debug)]
pub struct SessionActivity {
    pub local: bool,
    pub activity: Activity,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub observed_at: Duration,
    pub sessions: Vec<SessionActivity>,
}

impl Snapshot {
    fn away(&self, now: Duration, config: &Config) -> bool {
        let Some(age) = now.checked_sub(self.observed_at) else {
            return false;
        };
        if age > Duration::from_secs(config.freshness_seconds) || self.sessions.len() > 32 {
            return false;
        }
        let mut local = false;
        for session in &self.sessions {
            if !session.local {
                continue;
            }
            local = true;
            match session.activity {
                Activity::Locked => {}
                Activity::IdleSince(since)
                    if since != Duration::ZERO
                        && since <= self.observed_at
                        && now.checked_sub(since).is_some_and(|idle| {
                            idle >= Duration::from_secs(config.idle_seconds)
                        }) => {}
                Activity::Unknown | Activity::Active | Activity::IdleSince(_) => return false,
            }
        }
        local
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    LocalOnly,
    WouldNotify,
    Send,
}

pub struct Policy {
    config: Config,
    last_completion: Option<Completion>,
    pending: Option<(Completion, Duration)>,
    last_attempt: Option<Duration>,
}

impl Policy {
    pub fn new(config: Config) -> Result<Self, crate::ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            last_completion: None,
            pending: None,
            last_attempt: None,
        })
    }

    pub fn completion(&mut self, completion: Completion, now: Option<Duration>) -> bool {
        if self
            .last_completion
            .is_some_and(|last| last.0 >= completion.0)
        {
            return false;
        }
        self.last_completion = Some(completion);
        self.pending = None;
        if self.config.mode == Mode::Disabled {
            return false;
        }
        let Some(now) = now else { return false };
        self.pending = Some((completion, now));
        true
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    pub fn evaluate(
        &mut self,
        now: Option<Duration>,
        snapshot: Option<&Snapshot>,
        available: bool,
    ) -> Decision {
        let Some((_, finished)) = self.pending else {
            return Decision::LocalOnly;
        };
        let Some(now) = now else {
            self.cancel();
            return Decision::LocalOnly;
        };
        let Some(elapsed) = now.checked_sub(finished) else {
            self.cancel();
            return Decision::LocalOnly;
        };
        if elapsed < Duration::from_secs(self.config.grace_seconds) {
            return Decision::LocalOnly;
        }
        self.pending = None;
        if !available
            || !snapshot.is_some_and(|s| {
                s.observed_at
                    .checked_sub(finished)
                    .is_some_and(|age| age >= Duration::from_secs(self.config.grace_seconds))
                    && s.away(now, &self.config)
            })
            || self.last_attempt.is_some_and(|last| {
                now.checked_sub(last)
                    .is_none_or(|age| age < Duration::from_secs(self.config.rate_seconds))
            })
        {
            return Decision::LocalOnly;
        }
        self.last_attempt = Some(now);
        match self.config.mode {
            Mode::Disabled => Decision::LocalOnly,
            Mode::DryRun => Decision::WouldNotify,
            Mode::Send => Decision::Send,
        }
    }

    pub(crate) fn evaluate_completion(
        &mut self,
        completion: Completion,
        now: Option<Duration>,
        snapshot: Option<&Snapshot>,
        available: bool,
    ) -> Decision {
        if !self.pending.is_some_and(|(id, _)| id == completion) {
            return Decision::LocalOnly;
        }
        self.evaluate(now, snapshot, available)
    }
}
