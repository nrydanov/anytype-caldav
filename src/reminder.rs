//! Shared reminder timing for both iCalendar `VALARM` and Web Push.

use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::{
    config::{CalendarConfig, RemindersConfig},
    model::{CalendarValue, Task},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderAnchor {
    Deadline,
    Scheduled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderMoment {
    pub trigger_at: DateTime<Utc>,
    pub anchor: ReminderAnchor,
    /// The anchor date the trigger was derived from. Carried along so a caller
    /// that needs to describe the reminder cannot pick a different field than
    /// the one that produced the timing.
    pub value: CalendarValue,
}

/// Computes every reminder instant for one task, earliest first.
///
/// A task carries its own lead times when the reminder property is filled in;
/// otherwise the configured default applies. An all-day value has no default
/// lead at all: `all_day_time` already *is* the hour at which a task with no
/// time of day should announce itself, so subtracting the default from it
/// would quietly move every such reminder to 08:30. An explicit lead does
/// subtract from that hour, which is what choosing one means.
pub fn reminders_for(
    task: &Task,
    calendar: &CalendarConfig,
    reminders: &RemindersConfig,
) -> Vec<ReminderMoment> {
    if !reminders.enabled || task.done {
        return Vec::new();
    }

    let (anchor, value) = match (task.deadline.as_ref(), task.scheduled.as_ref()) {
        (Some(value), _) => (ReminderAnchor::Deadline, value),
        (None, Some(value)) => (ReminderAnchor::Scheduled, value),
        (None, None) => return Vec::new(),
    };

    let value = value.classify(calendar.date_only_timezone);
    let (base, default_lead) = match value {
        CalendarValue::Instant(at) => (at, reminders.lead_time),
        CalendarValue::AllDay(day) => {
            // A DST spring-forward can leave the chosen time of day
            // non-existent; the reminder is dropped rather than silently
            // shifted to another hour.
            let local = calendar
                .timezone
                .from_local_datetime(&day.and_time(reminders.all_day_time))
                .earliest();
            match local {
                Some(local) => (local.with_timezone(&Utc), Duration::zero()),
                None => return Vec::new(),
            }
        }
    };

    let leads: &[Duration] = if task.reminder_leads.is_empty() {
        std::slice::from_ref(&default_lead)
    } else {
        &task.reminder_leads
    };

    let mut moments: Vec<ReminderMoment> = leads
        .iter()
        .map(|lead| ReminderMoment {
            trigger_at: base - *lead,
            anchor,
            value,
        })
        .collect();
    // Two options can resolve to the same instant. The state store would
    // collapse them into one claim anyway, so collapse them here, where the
    // feed can see it too.
    moments.sort_by_key(|moment| moment.trigger_at);
    moments.dedup_by_key(|moment| moment.trigger_at);
    moments
}

#[cfg(test)]
mod tests {
    use super::{ReminderAnchor, reminders_for};
    use chrono::{Duration, TimeZone, Utc};
    use chrono_tz::Europe::Saratov;

    use crate::{
        config::{CalendarConfig, RemindersConfig},
        model::{AnytypeDate, Task},
    };

    fn calendar() -> CalendarConfig {
        CalendarConfig {
            timezone: Saratov,
            name: "Anytype Tasks".into(),
            date_only_timezone: Saratov,
        }
    }

    fn reminders() -> RemindersConfig {
        RemindersConfig {
            enabled: true,
            lead_time: chrono::Duration::minutes(30),
            all_day_time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        }
    }

    fn task() -> Task {
        Task {
            object_id: "task".into(),
            name: "Task".into(),
            scheduled: None,
            deadline: None,
            done: false,
            reminder_leads: Vec::new(),
            object_url: None,
            last_modified: None,
        }
    }

    fn triggers(task: &Task) -> Vec<chrono::DateTime<Utc>> {
        reminders_for(task, &calendar(), &reminders())
            .into_iter()
            .map(|moment| moment.trigger_at)
            .collect()
    }

    #[test]
    fn timed_deadline_uses_lead_time_and_wins_over_scheduled() {
        let mut task = task();
        task.scheduled = AnytypeDate::parse("2026-08-20T00:00:00+04:00");
        task.deadline = AnytypeDate::parse("2026-08-30T14:30:00+04:00");

        let moments = reminders_for(&task, &calendar(), &reminders());

        assert_eq!(moments.len(), 1);
        assert_eq!(moments[0].anchor, ReminderAnchor::Deadline);
        assert_eq!(
            moments[0].trigger_at,
            Utc.with_ymd_and_hms(2026, 8, 30, 10, 0, 0).unwrap()
        );
    }

    #[test]
    fn all_day_deadline_uses_local_civil_time() {
        let mut task = task();
        task.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");

        assert_eq!(
            triggers(&task),
            vec![Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 0).unwrap()]
        );
    }

    #[test]
    fn completed_undated_and_disabled_tasks_have_no_reminder() {
        let mut task = task();
        assert!(reminders_for(&task, &calendar(), &reminders()).is_empty());

        task.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        task.done = true;
        assert!(reminders_for(&task, &calendar(), &reminders()).is_empty());

        task.done = false;
        let mut disabled = reminders();
        disabled.enabled = false;
        assert!(reminders_for(&task, &calendar(), &disabled).is_empty());
    }

    /// Several options on one task produce several reminders, earliest first.
    #[test]
    fn per_task_leads_replace_the_default_and_are_ordered() {
        let mut task = task();
        task.deadline = AnytypeDate::parse("2026-09-03T18:00:00+04:00");
        task.reminder_leads = vec![Duration::hours(2), Duration::days(1)];

        assert_eq!(
            triggers(&task),
            vec![
                // 1d before 03.09 18:00 +04 = 02.09 14:00 UTC
                Utc.with_ymd_and_hms(2026, 9, 2, 14, 0, 0).unwrap(),
                // 2h before = 03.09 16:00 +04 = 12:00 UTC
                Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap(),
            ]
        );
    }

    /// The default lead is deliberately not applied to an all-day value, but
    /// an explicitly chosen one is, counting back from `all_day_time`.
    #[test]
    fn an_explicit_lead_counts_back_from_the_all_day_hour() {
        let mut task = task();
        task.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        task.reminder_leads = vec![Duration::days(1)];

        assert_eq!(
            triggers(&task),
            vec![Utc.with_ymd_and_hms(2026, 8, 29, 5, 0, 0).unwrap()]
        );
    }

    #[test]
    fn options_landing_on_one_instant_produce_one_reminder() {
        let mut task = task();
        task.deadline = AnytypeDate::parse("2026-08-30T14:30:00+04:00");
        task.reminder_leads = vec![Duration::minutes(60), Duration::hours(1)];

        assert_eq!(
            triggers(&task),
            vec![Utc.with_ymd_and_hms(2026, 8, 30, 9, 30, 0).unwrap()]
        );
    }
}
