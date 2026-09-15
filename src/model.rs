//! Source-neutral task model.
//!
//! Nothing from the Anytype SDK appears here, so the renderer and the feed
//! service can be tested without a source at all.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;

/// A date exactly as Anytype returned it, before any normalization.
///
/// The raw text is retained because the Anytype HTTP API models a date
/// property as an RFC3339 string with no flag for whether it carries a
/// meaningful time component. The original offset is therefore the only
/// signal available for date-only classification, and parsing to a
/// `DateTime<Utc>` would discard it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnytypeDate {
    pub raw: String,
    pub parsed: DateTime<FixedOffset>,
}

impl AnytypeDate {
    pub fn parse(raw: &str) -> Option<Self> {
        DateTime::parse_from_rfc3339(raw.trim())
            .ok()
            .map(|parsed| Self {
                raw: raw.trim().to_string(),
                parsed,
            })
    }

    /// Decides how the value is written into the calendar.
    ///
    /// A value that lands on midnight in `date_only_tz` is treated as an
    /// all-day value. Everything else becomes a UTC instant: a fixed point in
    /// time is unambiguous in every client and needs no `VTIMEZONE`, which
    /// `TZID=...` would have required by RFC 5545 §3.6.5.
    pub fn classify(&self, date_only_tz: Tz) -> CalendarValue {
        let local = self.parsed.with_timezone(&date_only_tz);
        if local.time() == NaiveTime::MIN {
            CalendarValue::AllDay(local.date_naive())
        } else {
            CalendarValue::Instant(self.parsed.with_timezone(&Utc))
        }
    }
}

/// The calendar-facing form of a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarValue {
    /// Emitted as `VALUE=DATE`.
    AllDay(NaiveDate),
    /// Emitted as a UTC date-time with a `Z` suffix.
    Instant(DateTime<Utc>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub object_id: String,
    pub name: String,
    pub scheduled: Option<AnytypeDate>,
    pub deadline: Option<AnytypeDate>,
    pub done: bool,
    /// Lead times chosen on the task itself. Empty means "use the configured
    /// default"; several values mean several reminders.
    pub reminder_leads: Vec<chrono::Duration>,
    /// Option names of the tags property, in Anytype's order.
    pub tags: Vec<String>,
    pub object_url: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
}

impl Task {
    /// Stable across renames and date edits: a calendar client updates the
    /// existing component instead of creating a duplicate.
    pub fn uid(&self) -> String {
        format!("{}@anytype-task-exporter", self.object_id)
    }
}

/// One complete, successful read of the source.
#[derive(Debug, Clone, Default)]
pub struct TaskBatch {
    pub tasks: Vec<Task>,
    /// Values that could not be parsed, reported once per refresh rather than
    /// per object, so a systemic problem is one log line and not thousands.
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::{Europe::Saratov, UTC};

    fn date(raw: &str) -> AnytypeDate {
        AnytypeDate::parse(raw).expect("parseable date")
    }

    #[test]
    fn keeps_the_raw_text_and_the_original_offset() {
        let value = date("2026-08-29T14:30:00+04:00");
        assert_eq!(value.raw, "2026-08-29T14:30:00+04:00");
        assert_eq!(value.parsed.offset().local_minus_utc(), 4 * 3600);
    }

    #[test]
    fn rejects_unparseable_text() {
        assert!(AnytypeDate::parse("not a date").is_none());
        assert!(AnytypeDate::parse("").is_none());
        assert!(AnytypeDate::parse("2026-08-29").is_none());
    }

    #[test]
    fn midnight_in_the_reference_zone_is_all_day() {
        let value = date("2026-08-29T00:00:00+04:00");
        match value.classify(Saratov) {
            CalendarValue::AllDay(day) => {
                assert_eq!(day.to_string(), "2026-08-29");
            }
            other => panic!("expected all-day, got {other:?}"),
        }
    }

    #[test]
    fn a_time_of_day_becomes_a_utc_instant() {
        let value = date("2026-08-29T14:30:00+04:00");
        match value.classify(Saratov) {
            // 14:30 in UTC+4 is 10:30 UTC.
            CalendarValue::Instant(instant) => {
                assert_eq!(instant.to_rfc3339(), "2026-08-29T10:30:00+00:00");
            }
            other => panic!("expected an instant, got {other:?}"),
        }
    }

    /// Pins the behaviour that the open question about Anytype's date-only
    /// storage will settle. One value, two reference zones, two answers: this
    /// is exactly why `date_only_timezone` is configurable.
    #[test]
    fn the_reference_zone_decides_whether_midnight_utc_is_all_day() {
        let value = date("2026-08-29T00:00:00Z");

        assert!(
            matches!(value.classify(UTC), CalendarValue::AllDay(_)),
            "midnight UTC is all-day when the reference zone is UTC"
        );
        assert!(
            matches!(value.classify(Saratov), CalendarValue::Instant(_)),
            "the same value is 04:00 in Saratov, so it is a timed value there"
        );
    }

    #[test]
    fn classification_crosses_the_day_boundary_with_the_reference_zone() {
        // 22:00 UTC is 02:00 the next day in Saratov, so the calendar day
        // must come from the reference zone, not from the raw value.
        let value = date("2026-08-28T20:00:00Z");
        match value.classify(Saratov) {
            CalendarValue::AllDay(day) => assert_eq!(day.to_string(), "2026-08-29"),
            other => panic!("expected all-day, got {other:?}"),
        }
    }

    /// Observed in a real space: Anytype persists a date-only value as midnight
    /// in the user's local zone, serialized as UTC. A deadline the user set to
    /// 14 September arrives as `2026-09-13T20:00:00Z`, which is exactly
    /// midnight in Saratov. This is why `date_only_timezone` defaults to the
    /// display timezone rather than to UTC.
    #[test]
    fn a_real_anytype_date_only_value_is_all_day_in_saratov() {
        let value = date("2026-09-13T20:00:00Z");

        match value.classify(Saratov) {
            CalendarValue::AllDay(day) => assert_eq!(day.to_string(), "2026-09-14"),
            other => panic!("expected all-day, got {other:?}"),
        }
        assert!(
            matches!(value.classify(UTC), CalendarValue::Instant(_)),
            "reading the same value as UTC would wrongly make it a 20:00 timed task"
        );
    }

    #[test]
    fn uid_is_stable_across_name_and_date_changes() {
        let mut task = Task {
            object_id: "obj-1".into(),
            name: "Before".into(),
            scheduled: Some(date("2026-08-29T00:00:00+04:00")),
            deadline: None,
            done: false,
            reminder_leads: Vec::new(),
            tags: Vec::new(),
            object_url: None,
            last_modified: None,
        };
        let before = task.uid();

        task.name = "After".into();
        task.scheduled = Some(date("2027-01-01T09:00:00+04:00"));
        task.done = true;

        assert_eq!(before, task.uid());
        assert_eq!(before, "obj-1@anytype-task-exporter");
    }
}
