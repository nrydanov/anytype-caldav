//! Dates as a capturing client sends them.

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone};

use crate::{config::CalendarConfig, writeback};

/// A date from a capture, in the shape Anytype stores. Accepts an RFC 3339
/// instant (`2026-09-17T18:00:00+04:00`), which is what Apple's reminders and
/// Shortcuts produce, and a plain day (`2026-09-17`), stored as that whole day.
pub fn anytype_date(value: &str, config: &CalendarConfig) -> Option<String> {
    let value = value.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(value) {
        return writeback::to_anytype(
            writeback::Moment::At(at.with_timezone(&chrono::Utc)),
            config.date_only_timezone,
        );
    }
    let day = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    writeback::to_anytype(writeback::Moment::Day(day), config.date_only_timezone)
}

/// Local midnight of `day` in the calendar's timezone, for tests and callers
/// that need the instant rather than the text.
pub fn midnight(day: NaiveDate, config: &CalendarConfig) -> Option<DateTime<chrono::Utc>> {
    config
        .timezone
        .from_local_datetime(&day.and_time(NaiveTime::MIN))
        .earliest()
        .map(|at| at.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Saratov;

    use super::*;

    fn config() -> CalendarConfig {
        CalendarConfig {
            timezone: Saratov,
            name: "t".into(),
            date_only_timezone: Saratov,
        }
    }

    #[test]
    fn an_instant_keeps_its_moment_and_a_day_becomes_the_whole_day() {
        assert_eq!(
            anytype_date("2026-09-17T18:00:00+04:00", &config()).as_deref(),
            Some("2026-09-17T14:00:00Z")
        );
        // Local midnight, which is how Anytype stores a whole day.
        assert_eq!(
            anytype_date("2026-09-17", &config()).as_deref(),
            Some("2026-09-16T20:00:00Z")
        );
        assert_eq!(anytype_date("завтра", &config()), None);
    }
}
