use std::time::{Duration, Instant};

const CHARS_PER_TOKEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPart {
    Thinking,
    Text,
    ToolArgs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnReadout {
    pub verb: String,
    pub elapsed: Option<Duration>,
    pub tokens: u64,
    pub arrow: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct TurnActivity {
    started_at: Option<Instant>,
    completed_output_tokens: u64,
    reported_tokens: u64,
    last_duration: Option<Duration>,
}

impl TurnActivity {
    pub fn begin(&mut self) {
        *self = Self {
            started_at: Some(Instant::now()),
            ..Self::default()
        };
    }

    pub fn end(&mut self) {
        if let Some(started) = self.started_at.take() {
            self.last_duration = Some(started.elapsed());
        }
    }

    #[must_use]
    pub fn last_duration(&self) -> Option<Duration> {
        self.last_duration
    }

    #[must_use]
    pub fn running(&self) -> bool {
        self.started_at.is_some()
    }

    pub fn record_message(&mut self, output_tokens: Option<u64>, streamed_chars: usize) {
        self.reported_tokens = self.tokens(streamed_chars);
        self.completed_output_tokens +=
            output_tokens.unwrap_or_else(|| estimate_tokens(streamed_chars));
        self.reported_tokens = self.reported_tokens.max(self.completed_output_tokens);
    }

    #[must_use]
    pub fn tokens(&self, streamed_chars: usize) -> u64 {
        self.reported_tokens
            .max(self.completed_output_tokens + estimate_tokens(streamed_chars))
    }

    #[cfg(test)]
    pub fn backdate(&mut self, by: Duration) {
        self.started_at = self.started_at.map(|started| started - by);
    }

    #[must_use]
    pub fn elapsed(&self) -> Option<Duration> {
        self.elapsed_at(Instant::now())
    }

    #[must_use]
    pub fn elapsed_at(&self, now: Instant) -> Option<Duration> {
        self.started_at
            .map(|started| now.saturating_duration_since(started))
    }
}

fn estimate_tokens(chars: usize) -> u64 {
    (chars / CHARS_PER_TOKEN) as u64
}

#[must_use]
pub fn format_elapsed(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[must_use]
pub fn format_tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{:.1}k", count as f64 / 1_000.0),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{:.1}m", count as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_formats_by_magnitude() {
        assert_eq!(format_elapsed(Duration::from_secs(7)), "7s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
        assert_eq!(format_elapsed(Duration::from_secs(3_725)), "1h 02m 05s");
    }

    #[test]
    fn tokens_format_by_magnitude() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_234), "1.2k");
        assert_eq!(format_tokens(45_678), "45k");
        assert_eq!(format_tokens(2_500_000), "2.5m");
    }

    #[test]
    fn reported_tokens_never_dip_when_usage_arrives_below_the_estimate() {
        let mut turn = TurnActivity::default();
        turn.begin();
        assert_eq!(turn.tokens(400), 100);
        turn.record_message(Some(60), 400);
        assert_eq!(turn.tokens(0), 100);
        assert_eq!(turn.tokens(200), 110);
    }

    #[test]
    fn a_message_without_usage_counts_its_streamed_text() {
        let mut turn = TurnActivity::default();
        turn.begin();
        turn.record_message(None, 800);
        assert_eq!(turn.tokens(0), 200);
    }

    #[test]
    fn begin_resets_the_count_and_end_stops_the_clock() {
        let mut turn = TurnActivity::default();
        turn.begin();
        turn.record_message(Some(50), 0);
        turn.begin();
        assert_eq!(turn.tokens(0), 0);
        assert!(turn.running());
        turn.end();
        assert!(!turn.running());
        assert!(turn.elapsed().is_none());
    }

    #[test]
    fn a_finished_turn_keeps_its_duration_until_the_next_one_starts() {
        let mut turn = TurnActivity::default();
        assert_eq!(turn.last_duration(), None);
        turn.begin();
        turn.backdate(Duration::from_secs(125));
        turn.end();
        let last = turn.last_duration().expect("a duration");
        assert!(last >= Duration::from_secs(125), "{last:?}");
        turn.end();
        assert_eq!(turn.last_duration(), Some(last));
        turn.begin();
        assert_eq!(turn.last_duration(), None);
    }
}
