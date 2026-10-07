use std::collections::BTreeSet;

use chrono::{Datelike, TimeZone};

const LONGEST_LEAP_DAY_WAIT_DAYS: u64 = 8 * 366;

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
        let after = chrono::Local
            .timestamp_millis_opt(i64::try_from(after_ms).ok()?)
            .single()?;
        let first = after.date_naive();
        first
            .iter_days()
            .take_while(|day| *day <= first + chrono::Days::new(LONGEST_LEAP_DAY_WAIT_DAYS))
            .filter(|day| self.day_matches(*day))
            .find_map(|day| {
                self.hour.iter().find_map(|&hour| {
                    self.minute.iter().find_map(|&minute| {
                        day.and_hms_opt(hour, minute, 0)
                            .and_then(|time| chrono::Local.from_local_datetime(&time).earliest())
                            .filter(|at| *at > after)
                    })
                })
            })
            .and_then(|at| u64::try_from(at.timestamp_millis()).ok())
    }

    fn day_matches(&self, day: chrono::NaiveDate) -> bool {
        let weekday = day.weekday().num_days_from_sunday();
        let weekday_matches =
            self.day_of_week.contains(&weekday) || (weekday == 0 && self.day_of_week.contains(&7));
        self.day_of_month.contains(&day.day())
            && self.month.contains(&day.month())
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
    fn a_leap_day_schedule_waits_for_the_next_leap_year() {
        let cron = Cron::parse("0 9 29 2 *").unwrap();
        let first = cron.next_after(local("2026-08-13 00:00:00"));
        assert_eq!(first, Some(local("2028-02-29 09:00:00")));
        assert_eq!(
            cron.next_after(first.unwrap()),
            Some(local("2032-02-29 09:00:00"))
        );
        assert_eq!(
            cron.next_after(local("2096-03-01 00:00:00")),
            Some(local("2104-02-29 09:00:00"))
        );
    }

    #[test]
    fn a_date_that_never_exists_never_comes_round() {
        assert_eq!(
            Cron::parse("0 9 31 4 *")
                .unwrap()
                .next_after(local("2026-08-13 00:00:00")),
            None
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
