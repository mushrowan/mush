use std::collections::BTreeSet;

use chrono::{Datelike, TimeZone, Timelike};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: BTreeSet<u32>,
    hour: BTreeSet<u32>,
    day_of_month: BTreeSet<u32>,
    month: BTreeSet<u32>,
    day_of_week: BTreeSet<u32>,
}

impl Cron {
    pub fn parse(expression: &str) -> Result<Self, String> {
        let expression = alias(expression.trim());
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, day_of_month, month, day_of_week] = fields[..] else {
            return Err(
                "a cron schedule is five fields: minute hour day month weekday, or @hourly, @daily, @weekly, @monthly"
                    .into(),
            );
        };
        Ok(Self {
            minute: field(minute, 0, 59)?,
            hour: field(hour, 0, 23)?,
            day_of_month: field(day_of_month, 1, 31)?,
            month: field(month, 1, 12)?,
            day_of_week: field(day_of_week, 0, 7)?,
        })
    }

    #[must_use]
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        let start = chrono::Local
            .timestamp_millis_opt(i64::try_from(after_ms).ok()?)
            .single()?
            .with_second(0)?
            .with_nanosecond(0)?
            + chrono::Duration::minutes(1);
        let mut candidate = start;
        let deadline = start + chrono::Duration::days(366);
        while candidate <= deadline {
            if self.matches(candidate) {
                return u64::try_from(candidate.timestamp_millis()).ok();
            }
            candidate += chrono::Duration::minutes(1);
        }
        None
    }

    fn matches(&self, at: chrono::DateTime<chrono::Local>) -> bool {
        let weekday = at.weekday().num_days_from_sunday();
        let weekday_matches =
            self.day_of_week.contains(&weekday) || (weekday == 0 && self.day_of_week.contains(&7));
        self.minute.contains(&at.minute())
            && self.hour.contains(&at.hour())
            && self.day_of_month.contains(&at.day())
            && self.month.contains(&at.month())
            && weekday_matches
    }
}

fn alias(expression: &str) -> &str {
    match expression {
        "@hourly" => "0 * * * *",
        "@daily" | "@midnight" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        other => other,
    }
}

fn field(text: &str, min: u32, max: u32) -> Result<BTreeSet<u32>, String> {
    let mut values = BTreeSet::new();
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, number(step, 1, max)?),
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((from, to)) = range.split_once('-') {
            let (from, to) = (number(from, min, max)?, number(to, min, max)?);
            if from > to {
                return Err(format!("{range} counts backwards"));
            }
            (from, to)
        } else {
            let value = number(range, min, max)?;
            (value, value)
        };
        values.extend((start..=end).step_by(step as usize));
    }
    if values.is_empty() {
        Err(format!("{text} matches nothing"))
    } else {
        Ok(values)
    }
}

fn number(text: &str, min: u32, max: u32) -> Result<u32, String> {
    let value: u32 = text
        .parse()
        .map_err(|_| format!("{text:?} is not a cron number"))?;
    if value < min || value > max {
        return Err(format!("{value} is outside {min}-{max}"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> u64 {
        u64::try_from(
            chrono::DateTime::parse_from_rfc3339(text)
                .unwrap()
                .timestamp_millis(),
        )
        .unwrap()
    }

    fn local(text: &str) -> u64 {
        use chrono::NaiveDateTime;
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap();
        u64::try_from(
            chrono::Local
                .from_local_datetime(&naive)
                .single()
                .unwrap()
                .timestamp_millis(),
        )
        .unwrap()
    }

    #[test]
    fn every_minute_is_the_next_minute() {
        let cron = Cron::parse("* * * * *").unwrap();
        assert_eq!(
            cron.next_after(local("2026-08-13 09:17:42")),
            Some(local("2026-08-13 09:18:00"))
        );
    }

    #[test]
    fn a_daily_time_rolls_to_tomorrow_once_it_has_passed() {
        let cron = Cron::parse("30 9 * * *").unwrap();
        assert_eq!(
            cron.next_after(local("2026-08-13 09:00:00")),
            Some(local("2026-08-13 09:30:00"))
        );
        assert_eq!(
            cron.next_after(local("2026-08-13 09:30:00")),
            Some(local("2026-08-14 09:30:00"))
        );
    }

    #[test]
    fn steps_lists_and_ranges_all_parse() {
        let cron = Cron::parse("0,30 9-11/2 * * *").unwrap();
        let mut runs = Vec::new();
        let mut cursor = local("2026-08-13 00:00:00");
        for _ in 0..4 {
            cursor = cron.next_after(cursor).unwrap();
            runs.push(cursor);
        }
        assert_eq!(
            runs,
            vec![
                local("2026-08-13 09:00:00"),
                local("2026-08-13 09:30:00"),
                local("2026-08-13 11:00:00"),
                local("2026-08-13 11:30:00"),
            ]
        );
    }

    #[test]
    fn sunday_is_both_zero_and_seven() {
        let zero = Cron::parse("0 8 * * 0").unwrap();
        let seven = Cron::parse("0 8 * * 7").unwrap();
        let from = local("2026-08-13 00:00:00");
        assert_eq!(zero.next_after(from), seven.next_after(from));
        assert_eq!(zero.next_after(from), Some(local("2026-08-16 08:00:00")));
    }

    #[test]
    fn the_aliases_expand() {
        assert_eq!(
            Cron::parse("@hourly").unwrap(),
            Cron::parse("0 * * * *").unwrap()
        );
        assert_eq!(
            Cron::parse("@daily").unwrap(),
            Cron::parse("0 0 * * *").unwrap()
        );
        assert_eq!(
            Cron::parse("@weekly").unwrap(),
            Cron::parse("0 0 * * 0").unwrap()
        );
        assert_eq!(
            Cron::parse("@monthly").unwrap(),
            Cron::parse("0 0 1 * *").unwrap()
        );
    }

    #[test]
    fn a_bad_expression_says_what_is_allowed() {
        assert!(Cron::parse("0 9 * *").unwrap_err().contains("five fields"));
        assert!(
            Cron::parse("99 * * * *")
                .unwrap_err()
                .contains("outside 0-59")
        );
        assert!(
            Cron::parse("x * * * *")
                .unwrap_err()
                .contains("not a cron number")
        );
        assert!(
            Cron::parse("9-5 * * * *")
                .unwrap_err()
                .contains("backwards")
        );
        assert!(at("2026-08-13T09:00:00Z") > 0);
    }
}
