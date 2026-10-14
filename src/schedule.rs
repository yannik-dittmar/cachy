//! Schedules: cron expressions that give the times of the daemon's scheduled
//! checks, in the system's time zone.

use std::fmt;

use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use jiff::Zoned;

/// A schedule: a cron expression of five fields (minute, hour, day of month,
/// month, day of week), or a nickname such as `@daily`.
#[derive(Debug)]
pub struct Schedule {
    cron: Cron,
    /// The expression as configured.
    text: String,
}

impl Schedule {
    /// Parses `text`, which must give at least one time from now on.
    pub fn parse(text: &str) -> Result<Schedule, String> {
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(text)
            .map_err(|e| format!("invalid schedule `{text}`: {e}"))?;
        let schedule = Schedule {
            cron,
            text: text.to_owned(),
        };
        if schedule.next_after(&Zoned::now()).is_none() {
            return Err(format!("schedule `{text}` never occurs"));
        }
        Ok(schedule)
    }

    /// The first time of this schedule after `time`, if there is one.
    pub fn next_after(&self, time: &Zoned) -> Option<Zoned> {
        self.cron.find_next_occurrence(time, false).ok()
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(schedule: &str, after: &str) -> String {
        let after: Zoned = format!("{after}[UTC]").parse().unwrap();
        let next = Schedule::parse(schedule)
            .unwrap()
            .next_after(&after)
            .unwrap();
        next.strftime("%Y-%m-%dT%H:%M").to_string()
    }

    #[test]
    fn next_times_follow_the_cron_fields() {
        assert_eq!(next("0 3 * * *", "2026-01-01T02:59"), "2026-01-01T03:00");
        // Strictly after: a check at its time is not due again.
        assert_eq!(next("0 3 * * *", "2026-01-01T03:00"), "2026-01-02T03:00");
        assert_eq!(next("*/15 * * * *", "2026-01-01T03:01"), "2026-01-01T03:15");
        // 2026-01-03 is a Saturday; 0 is Sunday.
        assert_eq!(next("30 4 * * 0", "2026-01-03T12:00"), "2026-01-04T04:30");
        assert_eq!(next("@daily", "2026-01-01T12:00"), "2026-01-02T00:00");
    }

    #[test]
    fn invalid_expressions_are_refused() {
        for text in [
            "61 * * * *",
            "* * * *",
            "0 0 * * * *",
            "nonsense",
            "0 0 30 2 *",
        ] {
            assert!(Schedule::parse(text).is_err(), "{text}");
        }
    }
}
