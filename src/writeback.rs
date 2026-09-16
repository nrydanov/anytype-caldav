//! Turns a VTODO a calendar client sent back into changes to an Anytype task.
//!
//! The rule is to write only what differs from what the renderer would send
//! now, and to write it into the property it came from. Calino patches the
//! bytes it last received, so an unchanged field comes back unchanged and is
//! left alone — including a deadline the renderer moved into DESCRIPTION.
//!
//! Pure: no Anytype, no HTTP. Every branch is testable on its own.

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use icalendar::{Calendar, CalendarDateTime, Component, DatePerhapsTime, Todo, TodoStatus};

use crate::{
    config::CalendarConfig,
    model::{CalendarValue, Task},
    render::{Origin, Wire},
};

/// The fields of a VTODO the facade understands.
#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub uid: Option<String>,
    pub summary: Option<String>,
    pub done: bool,
    pub start: Option<DatePerhapsTime>,
    pub due: Option<DatePerhapsTime>,
    /// CATEGORIES values.
    pub categories: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum WriteError {
    #[error("body is not an iCalendar document: {0}")]
    NotCalendar(String),
    #[error("body carries no VTODO; only tasks can be written")]
    NoTodo,
    #[error("body carries {0} VTODOs; recurrence overrides are not supported yet")]
    Overrides(usize),
}

/// What to change on an Anytype task. `Some(None)` clears a date.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Patch {
    pub name: Option<String>,
    pub done: Option<bool>,
    /// RFC 3339 UTC, in the shape Anytype stores (date-only = local midnight).
    pub scheduled: Option<Option<String>>,
    pub deadline: Option<Option<String>>,
    /// Option names for the tag property; an empty list clears the tags.
    pub tags: Option<Vec<String>>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.done.is_none()
            && self.tags.is_none()
            && self.scheduled.is_none()
            && self.deadline.is_none()
    }
}

pub fn parse(body: &str) -> Result<Incoming, WriteError> {
    let calendar: Calendar = terminated(body).parse().map_err(WriteError::NotCalendar)?;
    let todos: Vec<&Todo> = calendar.todos().collect();
    let todo = match todos.as_slice() {
        [] => return Err(WriteError::NoTodo),
        [one] => *one,
        many => return Err(WriteError::Overrides(many.len())),
    };

    // Calino writes STATUS, PERCENT-COMPLETE and COMPLETED together; any one
    // of them means done, as its own reader decides.
    let done = matches!(
        todo.get_status(),
        Some(TodoStatus::Completed | TodoStatus::Cancelled)
    ) || todo.get_percent_complete().is_some_and(|p| p >= 100)
        || todo.get_completed().is_some();

    Ok(Incoming {
        uid: todo.get_uid().map(str::to_string),
        summary: todo
            .get_summary()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        done,
        start: todo.get_start(),
        due: todo.get_due(),
        categories: crate::events::categories(todo),
    })
}

/// The parser rejects a document whose last line has no line break; some
/// clients and shell tools send one.
pub(crate) fn terminated(body: &str) -> std::borrow::Cow<'_, str> {
    if body.ends_with('\n') {
        body.into()
    } else {
        format!("{body}\r\n").into()
    }
}

/// A date on the wire, normalised so two spellings of one moment compare equal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Moment {
    Day(NaiveDate),
    At(DateTime<Utc>),
}

pub(crate) fn moment(value: &DatePerhapsTime, tz: Tz) -> Option<Moment> {
    Some(match value {
        DatePerhapsTime::Date(day) => Moment::Day(*day),
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(at)) => Moment::At(*at),
        DatePerhapsTime::DateTime(CalendarDateTime::Floating(local)) => Moment::At(
            tz.from_local_datetime(local)
                .earliest()?
                .with_timezone(&Utc),
        ),
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone { date_time, tzid }) => {
            let zone: Tz = tzid.parse().unwrap_or(tz);
            Moment::At(
                zone.from_local_datetime(date_time)
                    .earliest()?
                    .with_timezone(&Utc),
            )
        }
    })
}

/// Anytype's storage shape: a date-only value is local midnight in
/// `date_only_timezone`, serialised as UTC.
pub fn to_anytype(value: Moment, date_only_tz: Tz) -> Option<String> {
    let utc = match value {
        Moment::Day(day) => date_only_tz
            .from_local_datetime(&day.and_time(NaiveTime::MIN))
            .earliest()?
            .with_timezone(&Utc),
        Moment::At(at) => at,
    };
    Some(utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// A client may turn a whole-day value into an instant at the edge of the day
/// (the renderer itself writes an all-day deadline as 23:59:59 beside a timed
/// plan). Written back into a property that holds a whole day, such an
/// instant means that day again.
fn keep_whole_day(value: Moment, property: Option<CalendarValue>, tz: Tz) -> Moment {
    match (value, property) {
        (Moment::At(at), Some(CalendarValue::AllDay(_))) => {
            let local = at.with_timezone(&tz);
            let edge = (local.hour(), local.minute(), local.second());
            if edge == (0, 0, 0) || edge == (23, 59, 59) {
                Moment::Day(local.date_naive())
            } else {
                value
            }
        }
        _ => value,
    }
}

/// Changes for an existing task, given what the renderer currently sends.
pub fn for_update(
    current: &Task,
    wire: &Wire,
    incoming: &Incoming,
    config: &CalendarConfig,
) -> Patch {
    let tz = config.timezone;
    let date_tz = config.date_only_timezone;
    let mut patch = Patch::default();

    if let Some(name) = &incoming.summary
        && name != &current.name
    {
        patch.name = Some(name.clone());
    }
    if incoming.done != current.done {
        patch.done = Some(incoming.done);
    }
    patch.tags = crate::events::tags_patch(&current.tags, &incoming.categories);

    let now_start = wire.start.as_ref().and_then(|(d, _)| moment(d, tz));
    let now_due = wire.due.as_ref().and_then(|(d, _)| moment(d, tz));
    let new_due = incoming.due.as_ref().and_then(|d| moment(d, tz));
    // Calino writes DTSTART only when it differs from DUE.
    let new_start = incoming
        .start
        .as_ref()
        .and_then(|d| moment(d, tz))
        .filter(|start| Some(*start) != new_due);

    let scheduled_now = current.scheduled.as_ref().map(|d| d.classify(date_tz));
    let deadline_now = current.deadline.as_ref().map(|d| d.classify(date_tz));
    let write = |value: Option<Moment>, property: Option<CalendarValue>| -> Option<String> {
        value.and_then(|v| to_anytype(keep_whole_day(v, property, tz), date_tz))
    };

    // A start appearing where there was none turns the task into a plan with
    // a deadline: the client now holds two dates, and each needs a property.
    if new_start.is_some() && now_start.is_none() {
        patch.scheduled = Some(write(new_start, scheduled_now));
        patch.deadline = Some(write(new_due, deadline_now));
        return patch;
    }

    if new_start != now_start {
        patch.scheduled = Some(write(new_start, scheduled_now));
    }
    if new_due != now_due {
        match wire.due.as_ref().map(|(_, origin)| *origin) {
            Some(Origin::Deadline) => patch.deadline = Some(write(new_due, deadline_now)),
            Some(Origin::Both) => {
                patch.scheduled = Some(write(new_due, scheduled_now));
                patch.deadline = Some(write(new_due, deadline_now));
            }
            // A plan, or no date before: a date set in a calendar is a plan.
            Some(Origin::Scheduled) | None => {
                patch.scheduled = Some(write(new_due, scheduled_now));
            }
        }
    }
    patch
}

/// Fields for a task created in the client.
pub fn for_create(incoming: &Incoming, config: &CalendarConfig) -> Patch {
    let tz = config.timezone;
    let date_tz = config.date_only_timezone;
    let due = incoming.due.as_ref().and_then(|d| moment(d, tz));
    let start = incoming
        .start
        .as_ref()
        .and_then(|d| moment(d, tz))
        .filter(|start| Some(*start) != due);
    let (scheduled, deadline) = match (start, due) {
        (Some(start), due) => (Some(start), due),
        (None, due) => (due, None),
    };
    Patch {
        name: Some(
            incoming
                .summary
                .clone()
                .unwrap_or_else(|| "(unnamed)".to_string()),
        ),
        done: Some(incoming.done),
        scheduled: scheduled.map(|v| to_anytype(v, date_tz)),
        deadline: deadline.map(|v| to_anytype(v, date_tz)),
        tags: (!incoming.categories.is_empty()).then(|| incoming.categories.clone()),
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Saratov;

    use super::*;
    use crate::{config::RemindersConfig, model::AnytypeDate, render::VTodoRenderer};

    fn config() -> CalendarConfig {
        CalendarConfig {
            timezone: Saratov,
            name: "t".into(),
            date_only_timezone: Saratov,
        }
    }

    fn renderer() -> VTodoRenderer {
        VTodoRenderer::new(
            config(),
            RemindersConfig {
                enabled: false,
                lead_time: chrono::Duration::minutes(30),
                all_day_time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            },
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        )
    }

    fn task(scheduled: Option<&str>, deadline: Option<&str>) -> Task {
        Task {
            object_id: "obj".into(),
            name: "Pay rent".into(),
            scheduled: scheduled.and_then(AnytypeDate::parse),
            deadline: deadline.and_then(AnytypeDate::parse),
            done: false,
            reminder_leads: vec![],
            tags: vec![],
            ical_uid: None,
            object_url: None,
            last_modified: None,
        }
    }

    /// What Calino sends back: the rendered resource, edited as text.
    fn round_trip(task: &Task, edit: impl Fn(String) -> String) -> Incoming {
        parse(&edit(renderer().render_one(task))).unwrap()
    }

    fn update(task: &Task, edit: impl Fn(String) -> String) -> Patch {
        let incoming = round_trip(task, edit);
        for_update(task, &renderer().wire(task), &incoming, &config())
    }

    // 13 Sep date-only is stored as 12 Sep 20:00Z.
    const SEP13: &str = "2026-09-12T20:00:00Z";

    #[test]
    fn an_unchanged_resource_changes_nothing() {
        for t in [
            task(Some(SEP13), None),
            task(None, Some(SEP13)),
            task(Some("2026-09-13T08:00:00Z"), Some("2026-09-14T20:00:00Z")),
            task(Some("2026-09-20T08:00:00Z"), Some(SEP13)), // planned after deadline
            task(Some(SEP13), Some(SEP13)),                  // collapsed all-day pair
            task(Some("2026-09-13T08:00:00Z"), Some(SEP13)), // same-day described
            task(None, None),
        ] {
            assert!(update(&t, |s| s).is_empty(), "{t:?}");
        }
    }

    /// Calino writes all three completion properties.
    #[test]
    fn ticking_a_task_marks_it_done_and_nothing_else() {
        let t = task(Some(SEP13), None);
        let patch = update(&t, |s| {
            s.replace("STATUS:NEEDS-ACTION", "STATUS:COMPLETED")
                .replace("PERCENT-COMPLETE:0", "PERCENT-COMPLETE:100")
        });
        assert_eq!(
            patch,
            Patch {
                done: Some(true),
                ..Patch::default()
            }
        );
    }

    #[test]
    fn unticking_reopens() {
        let mut t = task(Some(SEP13), None);
        t.done = true;
        let patch = update(&t, |s| {
            s.replace("STATUS:COMPLETED", "STATUS:NEEDS-ACTION")
                .replace("PERCENT-COMPLETE:100", "")
        });
        assert_eq!(patch.done, Some(false));
    }

    #[test]
    fn dragging_a_planned_all_day_task_moves_the_plan() {
        let t = task(Some(SEP13), None);
        let patch = update(&t, |s| {
            s.replace("DUE;VALUE=DATE:20260913", "DUE;VALUE=DATE:20260915")
        });
        assert_eq!(
            patch,
            Patch {
                scheduled: Some(Some("2026-09-14T20:00:00Z".into())),
                ..Patch::default()
            }
        );
    }

    #[test]
    fn dragging_a_deadline_only_task_moves_the_deadline() {
        let t = task(None, Some(SEP13));
        let patch = update(&t, |s| {
            s.replace("DUE;VALUE=DATE:20260913", "DUE;VALUE=DATE:20260915")
        });
        assert_eq!(patch.deadline, Some(Some("2026-09-14T20:00:00Z".into())));
        assert_eq!(patch.scheduled, None);
    }

    #[test]
    fn a_collapsed_pair_moves_together() {
        let t = task(Some(SEP13), Some(SEP13));
        let patch = update(&t, |s| {
            s.replace("DUE;VALUE=DATE:20260913", "DUE;VALUE=DATE:20260915")
        });
        assert_eq!(patch.scheduled, Some(Some("2026-09-14T20:00:00Z".into())));
        assert_eq!(patch.deadline, Some(Some("2026-09-14T20:00:00Z".into())));
    }

    /// The deadline lives in DESCRIPTION; moving the card moves the plan only.
    #[test]
    fn a_described_deadline_is_never_touched() {
        let t = task(Some("2026-09-20T08:00:00Z"), Some(SEP13));
        let patch = update(&t, |s| {
            s.replace("DUE:20260920T080000Z", "DUE:20260921T080000Z")
        });
        assert_eq!(patch.scheduled, Some(Some("2026-09-21T08:00:00Z".into())));
        assert_eq!(patch.deadline, None);
    }

    #[test]
    fn a_timed_plan_and_deadline_move_independently() {
        // 15:00Z is 19:00 in Saratov: a real time, not a date-only midnight.
        let t = task(Some("2026-09-13T08:00:00Z"), Some("2026-09-14T15:00:00Z"));
        let patch = update(&t, |s| {
            s.replace("DUE:20260914T150000Z", "DUE:20260915T150000Z")
        });
        assert_eq!(patch.deadline, Some(Some("2026-09-15T15:00:00Z".into())));
        assert_eq!(patch.scheduled, None);
    }

    /// The renderer writes a whole-day deadline as 23:59:59 beside a timed
    /// plan; a client moving it by a day must not turn it into an instant.
    #[test]
    fn a_whole_day_deadline_stays_whole_through_its_instant_form() {
        let t = task(Some("2026-09-13T08:00:00Z"), Some("2026-09-13T20:00:00Z")); // deadline 14 Sep
        let wire = renderer().render_one(&t);
        assert!(wire.contains("DUE:20260914T195959Z"), "{wire}");
        let patch = update(&t, |s| {
            s.replace("DUE:20260914T195959Z", "DUE:20260915T195959Z")
        });
        assert_eq!(patch.deadline, Some(Some("2026-09-14T20:00:00Z".into())));
    }

    #[test]
    fn removing_the_date_clears_where_it_came_from() {
        let t = task(None, Some(SEP13));
        let patch = update(&t, |s| s.replace("DUE;VALUE=DATE:20260913\r\n", ""));
        assert_eq!(patch.deadline, Some(None));
    }

    #[test]
    fn renaming_changes_the_name() {
        let t = task(None, None);
        let patch = update(&t, |s| {
            s.replace("SUMMARY:Pay rent", "SUMMARY:Pay rent and water")
        });
        assert_eq!(patch.name.as_deref(), Some("Pay rent and water"));
    }

    #[test]
    fn a_new_dated_task_is_a_plan() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:abc-123\r\nSUMMARY:Buy milk\r\nDUE;VALUE=DATE:20260920\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let incoming = parse(body).unwrap();
        assert_eq!(incoming.uid.as_deref(), Some("abc-123"));
        let patch = for_create(&incoming, &config());
        assert_eq!(patch.name.as_deref(), Some("Buy milk"));
        assert_eq!(patch.done, Some(false));
        assert_eq!(patch.scheduled, Some(Some("2026-09-19T20:00:00Z".into())));
        assert_eq!(patch.deadline, None);
    }

    #[test]
    fn a_new_task_with_start_and_due_is_a_plan_with_a_deadline() {
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:x\r\nSUMMARY:Essay\r\nDTSTART:20260920T080000Z\r\nDUE:20260925T200000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let patch = for_create(&parse(body).unwrap(), &config());
        assert_eq!(patch.scheduled, Some(Some("2026-09-20T08:00:00Z".into())));
        assert_eq!(patch.deadline, Some(Some("2026-09-25T20:00:00Z".into())));
    }

    #[test]
    fn a_zoned_date_time_is_read_in_its_zone() {
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:x\r\nDUE;TZID=Europe/Moscow:20260920T120000\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let patch = for_create(&parse(body).unwrap(), &config());
        assert_eq!(patch.scheduled, Some(Some("2026-09-20T09:00:00Z".into())));
    }

    #[test]
    fn events_and_overrides_are_refused() {
        let event = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:e\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse(event), Err(WriteError::NoTodo));
        let two = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nEND:VTODO\r\nBEGIN:VTODO\r\nUID:a\r\nRECURRENCE-ID:20260920T080000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse(two), Err(WriteError::Overrides(2)));
    }
}
