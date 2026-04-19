//! startup profiling with phase timing
//!
//! when enabled via `--profile-startup` or `MUSH_PROFILE_STARTUP=1`,
//! prints a breakdown of where startup time is spent

use std::fmt;
use std::time::{Duration, Instant};

const BAR_WIDTH: usize = 30;

/// collects named timing phases during startup
pub struct PhaseTimer {
    start: Instant,
    last: Instant,
    phases: Vec<(String, Duration)>,
}

/// formatted report of startup phase timings
pub struct StartupReport {
    phases: Vec<(String, Duration)>,
    total: Duration,
}

impl Default for PhaseTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl PhaseTimer {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last: now,
            phases: Vec::new(),
        }
    }

    /// end the current phase and record its duration
    pub fn phase(&mut self, name: impl Into<String>) {
        let now = Instant::now();
        self.phases.push((name.into(), now - self.last));
        self.last = now;
    }

    /// finish timing and produce a report
    pub fn finish(self) -> StartupReport {
        let total = self.start.elapsed();
        StartupReport {
            phases: self.phases,
            total,
        }
    }
}

impl StartupReport {
    #[allow(dead_code)]
    pub fn phases(&self) -> &[(String, Duration)] {
        &self.phases
    }

    #[allow(dead_code)]
    pub fn total(&self) -> Duration {
        self.total
    }
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms >= 1000 {
        let secs = d.as_secs_f64();
        format!("{secs:.1}s")
    } else {
        format!("{ms}ms")
    }
}

impl fmt::Display for StartupReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "startup profile:")?;

        if self.phases.is_empty() {
            writeln!(f, "  (no phases recorded)")?;
            writeln!(f, "  total  {}", format_duration(self.total))?;
            return Ok(());
        }

        let max_name = self.phases.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
        let max_dur = self
            .phases
            .iter()
            .map(|(_, d)| d.as_nanos())
            .max()
            .unwrap_or(1)
            .max(1);

        for (name, dur) in &self.phases {
            let dur_str = format_duration(*dur);
            let bar_len =
                (dur.as_nanos() as f64 / max_dur as f64 * BAR_WIDTH as f64).ceil() as usize;
            let bar_len = bar_len.max(1);
            let bar: String = "█".repeat(bar_len);

            writeln!(f, "  {name:<max_name$}  {dur_str:>6}  {bar}")?;
        }

        writeln!(f, "  {:<max_name$}  ------", "")?;
        write!(
            f,
            "  {:<max_name$}  {:>6}",
            "total",
            format_duration(self.total)
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_report(phases: &[(&str, u64)]) -> StartupReport {
        let total: u64 = phases.iter().map(|(_, ms)| ms).sum();
        StartupReport {
            phases: phases
                .iter()
                .map(|(n, ms)| ((*n).to_string(), Duration::from_millis(*ms)))
                .collect(),
            total: Duration::from_millis(total),
        }
    }

    #[test]
    fn phase_timer_records_in_order() {
        let mut timer = PhaseTimer::new();
        timer.phase("first");
        timer.phase("second");
        timer.phase("third");
        let report = timer.finish();

        let names: Vec<&str> = report.phases().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn phase_timer_total_covers_all_phases() {
        let mut timer = PhaseTimer::new();
        timer.phase("a");
        timer.phase("b");
        let report = timer.finish();

        let sum: Duration = report.phases().iter().map(|(_, d)| *d).sum();
        assert!(report.total() >= sum);
    }

    #[test]
    fn report_display_includes_all_phase_names() {
        let report = make_report(&[("config", 5), ("tools", 100), ("mcp", 800)]);
        let text = report.to_string();

        assert!(text.contains("config"), "missing 'config'");
        assert!(text.contains("tools"), "missing 'tools'");
        assert!(text.contains("mcp"), "missing 'mcp'");
    }

    #[test]
    fn report_display_includes_total() {
        let report = make_report(&[("config", 5), ("tools", 100), ("mcp", 800)]);
        let text = report.to_string();

        assert!(text.contains("905ms"), "missing total '905ms'");
    }

    #[test]
    fn report_display_longest_phase_gets_full_bar() {
        let report = make_report(&[("fast", 10), ("slow", 990)]);
        let text = report.to_string();
        let lines: Vec<&str> = text.lines().collect();

        let slow_line = lines
            .iter()
            .find(|l| l.contains("slow"))
            .expect("no slow line");
        let fast_line = lines
            .iter()
            .find(|l| l.contains("fast"))
            .expect("no fast line");

        let count_blocks = |s: &str| s.chars().filter(|c| *c == '█').count();
        assert!(
            count_blocks(slow_line) > count_blocks(fast_line),
            "slow should have more blocks than fast"
        );
    }

    #[test]
    fn report_display_empty_phases() {
        let report = StartupReport {
            phases: vec![],
            total: Duration::ZERO,
        };
        let text = report.to_string();
        assert!(text.contains("0ms") || text.contains("total"));
    }
}
