use std::hash::{BuildHasher, Hasher, RandomState};
use std::time::Duration;

pub const FEATURE_HINT_DELAY: Duration = Duration::from_secs(5);

pub const FEATURE_HINTS: &[&str] = &[
    "/btw <question> asks something on the side without interrupting the agent",
    "sending a message while the agent works steers its current task",
    "/heartbeat every 10m <instruction> repeats a task on a schedule",
    "/schedule add \"in 30m\" <prompt> runs a prompt later, once or on a cron",
    "the agent can hand tasks to child agents with rlm.spawn and run them in parallel",
    "/agents lists the child agents of this session and what they're doing",
    "/tree shows the conversation tree, /branch returns to an earlier message",
    "/fork starts a new session from an earlier prompt",
    "/goal <objective> keeps the agent working until it calls goal.complete()",
    "/autonomous on lets the agent keep going when no one is expected to answer",
    "python variables and helpers survive between turns and across compactions",
    "rlm.note keeps a note the agent still sees after compaction and kernel restarts",
    "/cost shows the tokens and money this session has used",
    "/compact <instructions> summarises old messages with a focus you choose",
    "long sessions are compacted before the context fills up",
    "closing the terminal detaches. the agent keeps running, mush --resume reattaches",
    "mush send <session> <text> queues a message for a running session from another shell",
    "ctrl+f searches the conversation",
    "/undo reverts the last turn",
];

#[derive(Debug, Default)]
pub struct FeatureHintDeck {
    remaining: Vec<usize>,
    previous: Option<usize>,
}

impl FeatureHintDeck {
    pub fn draw(&mut self) -> &'static str {
        if self.remaining.is_empty() {
            self.refill(random_u64());
        }
        let index = self.remaining.pop().unwrap_or_default();
        self.previous = Some(index);
        FEATURE_HINTS[index]
    }

    fn refill(&mut self, seed: u64) {
        let mut hints: Vec<usize> = (0..FEATURE_HINTS.len()).collect();
        let mut state = seed | 1;
        for index in (1..hints.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let target = (state % (index as u64 + 1)) as usize;
            hints.swap(index, target);
        }
        if hints.len() > 1 && hints.last() == self.previous.as_ref() {
            let last = hints.len() - 1;
            hints.swap(0, last);
        }
        self.remaining = hints;
    }
}

fn random_u64() -> u64 {
    RandomState::new().build_hasher().finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_hint_comes_up_once_before_any_repeats() {
        let mut deck = FeatureHintDeck::default();
        let seen: HashSet<&str> = (0..FEATURE_HINTS.len()).map(|_| deck.draw()).collect();
        assert_eq!(seen.len(), FEATURE_HINTS.len());
    }

    #[test]
    fn a_refill_never_repeats_the_hint_just_shown() {
        for _ in 0..50 {
            let mut deck = FeatureHintDeck::default();
            let mut last = "";
            for _ in 0..FEATURE_HINTS.len() {
                last = deck.draw();
            }
            assert_ne!(deck.draw(), last);
        }
    }

    #[test]
    fn the_shuffle_is_seeded() {
        let mut a = FeatureHintDeck::default();
        let mut b = FeatureHintDeck::default();
        a.refill(7);
        b.refill(7);
        assert_eq!(a.remaining, b.remaining);
        a.refill(8);
        assert_ne!(a.remaining, b.remaining);
    }
}
