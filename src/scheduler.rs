//! Background delivery of task reminders through Web Push.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use chrono_tz::Tz;
use tracing::{Instrument, debug, error, info, info_span, trace, warn};

use crate::{
    config::{CalendarConfig, RemindersConfig},
    events::{Event, EventStore},
    model::{CalendarValue, Task},
    push::{Notification, PushService},
    reminder::{ReminderAnchor, ReminderMoment, reminders_for},
    source::{SourceError, TaskSource},
    state::{ReminderOutcome, StateError, StateStore},
};

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("scheduler could not read tasks: {0}")]
    Source(#[from] SourceError),
    #[error("scheduler task read timed out after {0:?}")]
    Timeout(Duration),
    #[error("scheduler could not update durable state: {0}")]
    State(#[from] StateError),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CheckReport {
    pub tasks: usize,
    pub attempted: usize,
    /// Due, but left unclaimed because nobody is subscribed yet.
    pub waiting: usize,
    pub expired: usize,
    pub delivered: usize,
    pub subscriptions: usize,
}

#[async_trait]
pub trait NotificationSink: Send + Sync + 'static {
    async fn notify_all(&self, notification: &Notification) -> (usize, usize);

    /// How many recipients a send would reach right now.
    fn audience(&self) -> usize;
}

#[async_trait]
impl NotificationSink for PushService {
    async fn notify_all(&self, notification: &Notification) -> (usize, usize) {
        PushService::notify_all(self, notification).await
    }

    fn audience(&self) -> usize {
        self.subscription_count()
    }
}

pub struct PushScheduler {
    source: Arc<dyn TaskSource>,
    state: Arc<StateStore>,
    sink: Arc<dyn NotificationSink>,
    calendar: CalendarConfig,
    reminders: RemindersConfig,
    poll_interval: Duration,
    late_window: chrono::Duration,
    request_timeout: Duration,
    events: Option<Arc<dyn EventStore>>,
    passes: AtomicU64,
}

impl PushScheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: Arc<dyn TaskSource>,
        state: Arc<StateStore>,
        sink: Arc<dyn NotificationSink>,
        calendar: CalendarConfig,
        reminders: RemindersConfig,
        poll_interval: Duration,
        late_window: chrono::Duration,
        request_timeout: Duration,
    ) -> Self {
        Self {
            source,
            state,
            sink,
            calendar,
            reminders,
            poll_interval,
            late_window,
            request_timeout,
            events: None,
            passes: AtomicU64::new(0),
        }
    }

    /// Also remind of event starts. Only the leads set on an event count: an
    /// event without `reminder_lead` shows no alarm in the calendar and gets
    /// no push either.
    pub fn with_events(mut self, events: Arc<dyn EventStore>) -> Self {
        self.events = Some(events);
        self
    }

    /// Reminder moments of every event with leads, as tasks planned at the
    /// event's start. A failed read skips events for this pass and keeps the
    /// task reminders going.
    async fn event_entries(&self, now: DateTime<Utc>) -> Vec<(Task, Vec<ReminderMoment>)> {
        let Some(store) = &self.events else {
            return Vec::new();
        };
        let events = match tokio::time::timeout(self.request_timeout, store.list()).await {
            Ok(Ok(events)) => events,
            Ok(Err(err)) => {
                warn!(error = %err, "scheduler event read failed; events skipped this pass");
                return Vec::new();
            }
            Err(_) => {
                warn!(timeout = ?self.request_timeout, "scheduler event read timed out; events skipped this pass");
                return Vec::new();
            }
        };
        debug!(events = events.len(), "scheduler read events");
        // A deadline is reminded like a start, under the event's own name.
        let deadlines: Vec<Event> = events
            .iter()
            .filter_map(|event| {
                crate::events::deadline_entry(event).map(|entry| Event {
                    name: event.name.clone(),
                    ..entry
                })
            })
            .collect();
        let entries = events
            .iter()
            .map(|event| (event, ReminderAnchor::Start))
            .chain(
                deadlines
                    .iter()
                    .map(|entry| (entry, ReminderAnchor::Deadline)),
            );
        entries
            .filter_map(|(event, anchor)| {
                let task = event_as_task(event);
                let longest = task.reminder_leads.iter().max().copied()?;
                // A start at or after its trigger: occurrences from the start
                // of the late window up to the longest lead ahead (plus a day
                // for all-day events, reminded at `all_day_time`) cover every
                // trigger that can be due now.
                let replaced: Vec<DateTime<Utc>> = events
                    .iter()
                    .filter(|other| other.series.as_deref() == Some(event.object_id.as_str()))
                    .filter_map(|other| other.occurrence.as_ref())
                    .map(|occurrence| occurrence.parsed.with_timezone(&Utc))
                    .collect();
                let starts = match crate::events::occurrences_between(
                    event,
                    self.calendar.timezone,
                    &replaced,
                    now - self.late_window,
                    now + longest + chrono::Duration::days(1),
                ) {
                    Ok(starts) => starts,
                    Err(err) => {
                        warn!(object_id = %event.object_id, event = %event.name, error = %err, "event recurrence unreadable; no reminders");
                        return None;
                    }
                };
                let moments: Vec<ReminderMoment> = starts
                    .into_iter()
                    .flat_map(|start| {
                        let occurrence = Task { scheduled: Some(start), ..task.clone() };
                        reminders_for(&occurrence, &self.calendar, &self.reminders)
                    })
                    .map(|moment| ReminderMoment { anchor, ..moment })
                    .collect();
                Some((task, moments))
            })
            .collect()
    }

    /// Performs one fresh Anytype read and handles every reminder now due.
    pub async fn check_at(&self, now: DateTime<Utc>) -> Result<CheckReport, SchedulerError> {
        let pass = self.passes.fetch_add(1, Ordering::Relaxed) + 1;
        self.check_inner(now)
            .instrument(info_span!("scheduler_pass", pass))
            .await
    }

    async fn check_inner(&self, now: DateTime<Utc>) -> Result<CheckReport, SchedulerError> {
        let started = Instant::now();
        debug!(%now, "scheduler pass started");
        let batch = tokio::time::timeout(self.request_timeout, self.source.list_tasks())
            .await
            .map_err(|_| {
                error!(timeout = ?self.request_timeout, "scheduler task read timed out");
                SchedulerError::Timeout(self.request_timeout)
            })?
            .inspect_err(|err| error!(error = %err, "scheduler task read failed"))?;
        for warning in &batch.warnings {
            warn!(warning = %warning, "scheduler source reported a malformed value");
        }

        let mut report = CheckReport {
            tasks: batch.tasks.len(),
            ..CheckReport::default()
        };
        // A claim is what makes delivery at-most-once, so it must never be
        // spent on a send that cannot happen. With no subscriptions the
        // reminder stays unclaimed and remains eligible for the rest of the
        // late window, which is exactly the window in which someone installs
        // the app and subscribes.
        let audience = self.sink.audience();
        debug!(
            tasks = batch.tasks.len(),
            audience, "scheduler evaluating reminders"
        );
        let mut entries: Vec<(Task, Vec<ReminderMoment>)> = batch
            .tasks
            .iter()
            .map(|task| {
                (
                    task.clone(),
                    reminders_for(task, &self.calendar, &self.reminders),
                )
            })
            .collect();
        entries.extend(self.event_entries(now).await);
        for (task, moments) in &entries {
            // A task can carry several lead times, each claimed on its own.
            for &moment in moments {
                if moment.trigger_at > now {
                    // trace: every future reminder, every pass — too many
                    // lines at debug to keep a useful journal history.
                    trace!(
                        object_id = %task.object_id,
                        task = %task.name,
                        trigger_at = %moment.trigger_at,
                        anchor = ?moment.anchor,
                        in_seconds = moment.trigger_at.signed_duration_since(now).num_seconds(),
                        decision = "wait: trigger in the future",
                        "reminder decision"
                    );
                    continue;
                }

                let late_by = now.signed_duration_since(moment.trigger_at);
                let expired = late_by > self.late_window;
                if !expired && audience == 0 {
                    info!(
                        object_id = %task.object_id,
                        task = %task.name,
                        trigger_at = %moment.trigger_at,
                        late_by_seconds = late_by.num_seconds(),
                        decision = "hold: due but nobody is subscribed",
                        "reminder decision"
                    );
                    report.waiting += 1;
                    continue;
                }
                let outcome = if expired {
                    ReminderOutcome::Expired
                } else {
                    ReminderOutcome::Attempted
                };
                let claimed = self
                    .state
                    .claim_reminder(&task.object_id, moment.trigger_at, outcome)
                    .inspect_err(|err| {
                        error!(object_id = %task.object_id, trigger_at = %moment.trigger_at, error = %err, "cannot claim reminder");
                    })?;
                if !claimed {
                    // trace: a handled reminder stays due forever, so this
                    // repeats on every pass.
                    trace!(
                        object_id = %task.object_id,
                        trigger_at = %moment.trigger_at,
                        decision = "skip: already handled",
                        "reminder decision"
                    );
                    continue;
                }

                if expired {
                    report.expired += 1;
                    info!(
                        object_id = %task.object_id,
                        task = %task.name,
                        trigger_at = %moment.trigger_at,
                        late_by_seconds = late_by.num_seconds(),
                        late_window_seconds = self.late_window.num_seconds(),
                        decision = "expire: later than the late window",
                        "reminder decision"
                    );
                    continue;
                }

                report.attempted += 1;
                let notification = notification_for(task, moment, now, &self.calendar);
                debug!(
                    object_id = %task.object_id,
                    title = %notification.title,
                    body = %notification.body,
                    "reminder notification built"
                );
                let (delivered, subscriptions) = self.sink.notify_all(&notification).await;
                report.delivered += delivered;
                report.subscriptions += subscriptions;
                let log_delivery = |level_ok: bool| {
                    if level_ok {
                        info!(
                            object_id = %task.object_id,
                            task = %task.name,
                            trigger_at = %moment.trigger_at,
                            late_by_seconds = late_by.num_seconds(),
                            delivered,
                            subscriptions,
                            decision = "send",
                            "reminder push attempted"
                        );
                    } else {
                        error!(
                            object_id = %task.object_id,
                            task = %task.name,
                            trigger_at = %moment.trigger_at,
                            delivered,
                            subscriptions,
                            "reminder push reached no subscription; the claim is spent and it will not be retried"
                        );
                    }
                };
                log_delivery(delivered > 0);
            }
        }
        let news = report.attempted + report.expired + report.waiting > 0;
        if news {
            info!(
                ?report,
                elapsed_ms = started.elapsed().as_millis(),
                "scheduler pass finished"
            );
        } else {
            debug!(
                ?report,
                elapsed_ms = started.elapsed().as_millis(),
                "scheduler pass finished"
            );
        }
        Ok(report)
    }

    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(err) = self.check_at(Utc::now()).await {
                warn!(error = %err, error_debug = ?err, "scheduler pass failed; retrying next interval");
            }
        }
    }
}

fn event_as_task(event: &Event) -> Task {
    Task {
        object_id: event.object_id.clone(),
        name: event.name.clone(),
        scheduled: event.start.clone(),
        deadline: None,
        done: false,
        reminder_leads: event.leads(),
        tags: event.tags.clone(),
        assignees: Vec::new(),
        ical_uid: event.ical_uid.clone(),
        object_url: event.object_url.clone(),
        last_modified: event.last_modified,
    }
}

fn notification_for(
    task: &Task,
    moment: ReminderMoment,
    now: DateTime<Utc>,
    calendar: &CalendarConfig,
) -> Notification {
    Notification {
        title: task.name.clone(),
        body: describe(moment, now, calendar.timezone),
        url: task.object_url.clone(),
        tag: Some(task.object_id.clone()),
        day: Some(match moment.value {
            CalendarValue::AllDay(day) => day,
            CalendarValue::Instant(at) => at.with_timezone(&calendar.timezone).date_naive(),
        }),
    }
}

/// Describes when the anchor date falls, phrased against `now`.
///
/// The wording is deliberately not derived from the lead time that produced
/// the reminder. A push held back by the late window can arrive after the
/// deadline it warns about, and "through 15 minutes" would then be a lie; the
/// only honest source is the distance from the moment of sending.
fn describe(moment: ReminderMoment, now: DateTime<Utc>, tz: Tz) -> String {
    let today = now.with_timezone(&tz).date_naive();
    match moment.value {
        CalendarValue::AllDay(day) => {
            format!(
                "{} {}",
                label(moment.anchor, day < today),
                day_phrase(day, today)
            )
        }
        CalendarValue::Instant(at) => {
            let local = at.with_timezone(&tz);
            let day = local.date_naive();
            let time = local.format("%H:%M");
            let when = if at > now && day == today {
                // Only today needs "in N minutes": for any other day the day
                // itself already answers "why now".
                format!("{} — сегодня в {time}", in_words(at - now))
            } else {
                format!("{} в {time}", day_phrase(day, today))
            };
            format!("{} {when}", label(moment.anchor, at <= now))
        }
    }
}

fn label(anchor: ReminderAnchor, past: bool) -> &'static str {
    match (anchor, past) {
        (ReminderAnchor::Deadline, false) => "Дедлайн",
        (ReminderAnchor::Deadline, true) => "Дедлайн был",
        (ReminderAnchor::Scheduled, false) => "По плану",
        (ReminderAnchor::Scheduled, true) => "По плану было",
        (ReminderAnchor::Start, false) => "Начало",
        (ReminderAnchor::Start, true) => "Началось",
    }
}

fn day_phrase(day: NaiveDate, today: NaiveDate) -> String {
    const MONTHS: [&str; 12] = [
        "января",
        "февраля",
        "марта",
        "апреля",
        "мая",
        "июня",
        "июля",
        "августа",
        "сентября",
        "октября",
        "ноября",
        "декабря",
    ];
    match (day - today).num_days() {
        0 => "сегодня".to_string(),
        1 => "завтра".to_string(),
        -1 => "вчера".to_string(),
        _ => format!("{} {}", day.day(), MONTHS[day.month0() as usize]),
    }
}

/// Both units round to the nearest, from the raw seconds.
///
/// Truncating reads as an off-by-one to anyone who chose the lead time: the
/// scheduler wakes on a 30-second tick, so a reminder set an hour ahead is
/// sent at 59m40s and "через 59 минут" is what the phone shows. Deriving the
/// hours from the minutes instead would round twice, turning 1h29m40s into
/// "через 2 часа" by way of 90 minutes.
fn in_words(delta: chrono::Duration) -> String {
    let seconds = delta.num_seconds();
    let minutes = (seconds + 30) / 60;
    if minutes < 1 {
        return "меньше чем через минуту".to_string();
    }
    if minutes < 60 {
        return match plural(minutes) {
            Plural::One => "через минуту".to_string(),
            Plural::Few => format!("через {minutes} минуты"),
            Plural::Many => format!("через {minutes} минут"),
        };
    }
    let hours = (seconds + 1800) / 3600;
    match plural(hours) {
        Plural::One => "через час".to_string(),
        Plural::Few => format!("через {hours} часа"),
        Plural::Many => format!("через {hours} часов"),
    }
}

enum Plural {
    One,
    Few,
    Many,
}

/// Russian counts agree in three forms, chosen by the last digits.
fn plural(count: i64) -> Plural {
    match (count % 10, count % 100) {
        (1, tens) if tens != 11 => Plural::One,
        (2..=4, tens) if !(12..=14).contains(&tens) => Plural::Few,
        _ => Plural::Many,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use chrono_tz::Europe::Saratov;

    use super::{NotificationSink, PushScheduler, describe};
    use crate::{
        config::{CalendarConfig, RemindersConfig},
        model::{AnytypeDate, Task, TaskBatch},
        push::Notification,
        reminder::reminders_for,
        source::{SourceError, TaskSource},
        state::{ReminderOutcome, StateStore},
    };

    #[derive(Clone)]
    enum SourceStep {
        Tasks(Vec<Task>),
        Fail,
    }

    struct ScriptedSource {
        steps: Mutex<Vec<SourceStep>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TaskSource for ScriptedSource {
        async fn list_tasks(&self) -> Result<TaskBatch, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut steps = self.steps.lock().unwrap();
            let step = if steps.len() > 1 {
                steps.remove(0)
            } else {
                steps.first().cloned().unwrap_or(SourceStep::Tasks(vec![]))
            };
            match step {
                SourceStep::Tasks(tasks) => Ok(TaskBatch {
                    tasks,
                    warnings: vec![],
                    members: Default::default(),
                    account_holders: Default::default(),
                }),
                SourceStep::Fail => Err(SourceError::Transport("scripted failure".into())),
            }
        }
    }

    struct RecordingSink {
        notifications: Mutex<Vec<Notification>>,
        audience: AtomicUsize,
    }

    /// One subscriber, which is the situation every test but the
    /// nobody-is-subscribed one is about.
    impl Default for RecordingSink {
        fn default() -> Self {
            Self {
                notifications: Mutex::new(Vec::new()),
                audience: AtomicUsize::new(1),
            }
        }
    }

    impl RecordingSink {
        fn notifications(&self) -> Vec<Notification> {
            self.notifications.lock().unwrap().clone()
        }

        fn set_audience(&self, count: usize) {
            self.audience.store(count, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl NotificationSink for RecordingSink {
        async fn notify_all(&self, notification: &Notification) -> (usize, usize) {
            self.notifications
                .lock()
                .unwrap()
                .push(notification.clone());
            let reached = self.audience.load(Ordering::SeqCst);
            (reached, reached)
        }

        fn audience(&self) -> usize {
            self.audience.load(Ordering::SeqCst)
        }
    }

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

    fn task(id: &str, name: &str) -> Task {
        Task {
            object_id: id.into(),
            name: name.into(),
            scheduled: None,
            deadline: None,
            done: false,
            reminder_leads: Vec::new(),
            tags: Vec::new(),
            assignees: Vec::new(),
            ical_uid: None,
            object_url: None,
            last_modified: None,
        }
    }

    fn build_scheduler(
        steps: Vec<SourceStep>,
    ) -> (
        PushScheduler,
        Arc<RecordingSink>,
        Arc<StateStore>,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(StateStore::open(&directory.path().join("state.sqlite3")).unwrap());
        let sink = Arc::new(RecordingSink::default());
        let source = Arc::new(ScriptedSource {
            steps: Mutex::new(steps),
            calls: AtomicUsize::new(0),
        });
        let scheduler = PushScheduler::new(
            source,
            state.clone(),
            sink.clone(),
            calendar(),
            reminders(),
            Duration::from_secs(30),
            chrono::Duration::hours(1),
            Duration::from_secs(10),
        );
        (scheduler, sink, state, directory)
    }

    #[tokio::test]
    async fn a_due_task_is_claimed_and_sent_once_with_real_content() {
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 10).unwrap();
        let mut task = task("obj", "Купить хлеб");
        task.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        task.object_url = Some("https://object.any.coop/obj?spaceId=space".into());
        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(vec![task])]);

        scheduler.check_at(now).await.unwrap();
        scheduler.check_at(now).await.unwrap();

        let sent = sink.notifications();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].title, "Купить хлеб");
        assert_eq!(sent[0].body, "Дедлайн сегодня");
        assert_eq!(
            sent[0].url.as_deref(),
            Some("https://object.any.coop/obj?spaceId=space")
        );
        assert_eq!(sent[0].tag.as_deref(), Some("obj"));
    }

    use crate::events::{Event, EventStore};

    struct OneEvent(Event);

    #[async_trait]
    impl EventStore for OneEvent {
        async fn list(&self) -> Result<Vec<Event>, SourceError> {
            Ok(vec![self.0.clone()])
        }
        async fn get(&self, _: &str) -> Result<Option<Event>, SourceError> {
            unreachable!()
        }
        async fn create(
            &self,
            _: &str,
            _: &crate::events::EventPatch,
        ) -> Result<String, SourceError> {
            unreachable!()
        }
        async fn update(&self, _: &str, _: &crate::events::EventPatch) -> Result<(), SourceError> {
            unreachable!()
        }
        async fn create_replacement(
            &self,
            _: &crate::events::EventPatch,
        ) -> Result<String, SourceError> {
            unreachable!()
        }
        async fn archive(&self, _: &str) -> Result<(), SourceError> {
            unreachable!()
        }
    }

    fn event(id: &str, leads: &[&str]) -> Event {
        Event {
            object_id: id.into(),
            name: "Семинар".into(),
            // 10:00 Saratov.
            start: AnytypeDate::parse("2026-08-30T06:00:00Z"),
            end: None,
            location: None,
            tags: Vec::new(),
            reminder_names: leads.iter().map(|l| l.to_string()).collect(),
            ical_uid: None,
            object_url: None,
            last_modified: None,
            rrule: None,
            exdates: Vec::new(),
            series: None,
            occurrence: None,

            deadline: None,
        }
    }

    struct Events(Vec<Event>);

    #[async_trait]
    impl EventStore for Events {
        async fn list(&self) -> Result<Vec<Event>, SourceError> {
            Ok(self.0.clone())
        }
        async fn get(&self, _: &str) -> Result<Option<Event>, SourceError> {
            unreachable!()
        }
        async fn create(
            &self,
            _: &str,
            _: &crate::events::EventPatch,
        ) -> Result<String, SourceError> {
            unreachable!()
        }
        async fn update(&self, _: &str, _: &crate::events::EventPatch) -> Result<(), SourceError> {
            unreachable!()
        }
        async fn create_replacement(
            &self,
            _: &crate::events::EventPatch,
        ) -> Result<String, SourceError> {
            unreachable!()
        }
        async fn archive(&self, _: &str) -> Result<(), SourceError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn a_weekly_event_reminds_of_this_weeks_occurrence_unless_excluded_or_replaced() {
        // Weekly from 2 August, 10:00 Saratov; now is 15 minutes before the
        // occurrence of 30 August.
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 5, 45, 10).unwrap();
        let mut weekly = event("weekly", &["15m"]);
        weekly.start = AnytypeDate::parse("2026-08-02T06:00:00Z");
        weekly.rrule = Some("FREQ=WEEKLY".into());

        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(Vec::new())]);
        let scheduler = scheduler.with_events(Arc::new(Events(vec![weekly.clone()])));
        scheduler.check_at(now).await.unwrap();
        let sent = sink.notifications();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].body.contains("10:00"), "{}", sent[0].body);
        assert_eq!(sent[0].day, chrono::NaiveDate::from_ymd_opt(2026, 8, 30));

        let mut excluded = weekly.clone();
        excluded.exdates = vec![AnytypeDate::parse("2026-08-30T06:00:00Z").unwrap()];
        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(Vec::new())]);
        let scheduler = scheduler.with_events(Arc::new(Events(vec![excluded])));
        scheduler.check_at(now).await.unwrap();
        assert!(sink.notifications().is_empty());

        // Moved to 12:00 that day: the master stays silent, the replacement
        // reminds at its own time.
        let mut moved = event("moved", &["15m"]);
        moved.name = "Семинар (перенесён)".into();
        moved.start = AnytypeDate::parse("2026-08-30T08:00:00Z");
        moved.series = Some("weekly".into());
        moved.occurrence = AnytypeDate::parse("2026-08-30T06:00:00Z");
        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(Vec::new())]);
        let scheduler = scheduler.with_events(Arc::new(Events(vec![weekly, moved])));
        scheduler.check_at(now).await.unwrap();
        assert!(sink.notifications().is_empty());
        scheduler
            .check_at(Utc.with_ymd_and_hms(2026, 8, 30, 7, 45, 10).unwrap())
            .await
            .unwrap();
        let sent = sink.notifications();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].title, "Семинар (перенесён)");
    }

    #[tokio::test]
    async fn an_event_start_is_reminded_only_with_its_own_leads() {
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 5, 45, 10).unwrap();
        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(Vec::new())]);
        let scheduler = scheduler.with_events(Arc::new(OneEvent(event("ev", &["15m"]))));
        scheduler.check_at(now).await.unwrap();
        let sent = sink.notifications();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].title, "Семинар");
        assert!(sent[0].body.starts_with("Начало "), "{}", sent[0].body);
        assert!(sent[0].body.contains("10:00"), "{}", sent[0].body);

        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(Vec::new())]);
        let scheduler = scheduler.with_events(Arc::new(OneEvent(event("ev", &[]))));
        scheduler.check_at(now).await.unwrap();
        assert!(sink.notifications().is_empty());
    }

    /// An event's deadline is reminded with the event's leads, counted from
    /// the deadline, and says it is the deadline.
    #[tokio::test]
    async fn an_event_deadline_is_reminded_with_the_events_leads() {
        let mut ev = event("ev", &["15m"]);
        // 29.08 18:00 Saratov.
        ev.deadline = AnytypeDate::parse("2026-08-29T14:00:00Z");
        let now = Utc.with_ymd_and_hms(2026, 8, 29, 13, 45, 10).unwrap();
        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(Vec::new())]);
        let scheduler = scheduler.with_events(Arc::new(OneEvent(ev)));
        scheduler.check_at(now).await.unwrap();
        let sent = sink.notifications();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].title, "Семинар");
        assert!(sent[0].body.starts_with("Дедлайн "), "{}", sent[0].body);
        assert!(sent[0].body.contains("18:00"), "{}", sent[0].body);
    }

    /// A claim is permanent, so spending one while no browser is subscribed
    /// would silently destroy the reminder. This is the ordinary first-deploy
    /// sequence: the service starts before the phone has ever subscribed.
    #[tokio::test]
    async fn a_reminder_is_not_burned_while_nobody_is_subscribed() {
        let mut due = task("obj", "Купить хлеб");
        due.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        let (scheduler, sink, state, _directory) =
            build_scheduler(vec![SourceStep::Tasks(vec![due])]);
        sink.set_audience(0);
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 10).unwrap();

        let report = scheduler.check_at(now).await.unwrap();
        assert_eq!(report.waiting, 1);
        assert_eq!(report.attempted, 0);
        assert!(sink.notifications().is_empty());

        // The phone subscribes a few minutes later, still inside the late window.
        sink.set_audience(1);
        scheduler
            .check_at(now + chrono::Duration::minutes(5))
            .await
            .unwrap();

        assert_eq!(sink.notifications().len(), 1);
        let trigger = Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 0).unwrap();
        assert!(
            !state
                .claim_reminder("obj", trigger, ReminderOutcome::Attempted)
                .unwrap(),
            "the delivered reminder must still be claimed exactly once"
        );
    }

    /// Each lead time is claimed and delivered on its own, so an early
    /// warning does not consume the reminder that matters.
    #[tokio::test]
    async fn two_lead_times_on_one_task_send_two_notifications() {
        let mut due = task("obj", "Сдать отчёт");
        due.deadline = AnytypeDate::parse("2026-09-03T18:00:00+04:00");
        due.reminder_leads = vec![chrono::Duration::hours(2), chrono::Duration::days(1)];
        let (scheduler, sink, _, _directory) = build_scheduler(vec![SourceStep::Tasks(vec![due])]);

        // Only the day-before reminder is due here.
        scheduler
            .check_at(Utc.with_ymd_and_hms(2026, 9, 2, 14, 0, 0).unwrap())
            .await
            .unwrap();
        assert_eq!(sink.notifications().len(), 1);

        // The two-hour one lands the next day, and the first is not repeated.
        scheduler
            .check_at(Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap())
            .await
            .unwrap();
        let sent = sink.notifications();
        assert_eq!(sent.len(), 2);
        // Both point at one deadline, but each says why it arrived when it did.
        assert_eq!(sent[0].body, "Дедлайн завтра в 18:00");
        assert_eq!(sent[1].body, "Дедлайн через 2 часа — сегодня в 18:00");
    }

    #[tokio::test]
    async fn one_hour_is_inclusive_but_older_is_expired() {
        let mut exact = task("exact", "Exact");
        exact.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        let (scheduler, sink, _, _directory) =
            build_scheduler(vec![SourceStep::Tasks(vec![exact])]);
        scheduler
            .check_at(Utc.with_ymd_and_hms(2026, 8, 30, 6, 0, 0).unwrap())
            .await
            .unwrap();
        assert_eq!(sink.notifications().len(), 1);

        let mut old = task("old", "Old");
        old.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        let (scheduler, sink, state, _directory) =
            build_scheduler(vec![SourceStep::Tasks(vec![old])]);
        let now =
            Utc.with_ymd_and_hms(2026, 8, 30, 6, 0, 0).unwrap() + chrono::Duration::milliseconds(1);
        scheduler.check_at(now).await.unwrap();
        assert!(sink.notifications().is_empty());
        let trigger = Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 0).unwrap();
        assert!(
            !state
                .claim_reminder("old", trigger, ReminderOutcome::Expired)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn future_and_completed_tasks_are_not_claimed() {
        let mut future = task("future", "Future");
        future.deadline = AnytypeDate::parse("2026-08-31T00:00:00+04:00");
        let mut done = task("done", "Done");
        done.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        done.done = true;
        let (scheduler, sink, state, _directory) =
            build_scheduler(vec![SourceStep::Tasks(vec![future, done])]);

        scheduler
            .check_at(Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 10).unwrap())
            .await
            .unwrap();

        assert!(sink.notifications().is_empty());
        let done_trigger = Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 0).unwrap();
        assert!(
            state
                .claim_reminder("done", done_trigger, ReminderOutcome::Attempted)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn an_edited_trigger_sends_again_but_source_failure_claims_nothing() {
        let mut before = task("obj", "Task");
        before.deadline = AnytypeDate::parse("2026-08-30T09:30:00+04:00");
        let mut after = before.clone();
        after.deadline = AnytypeDate::parse("2026-08-30T10:00:00+04:00");
        let (scheduler, sink, _, _directory) = build_scheduler(vec![
            SourceStep::Fail,
            SourceStep::Tasks(vec![before]),
            SourceStep::Tasks(vec![after]),
        ]);
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 5, 30, 10).unwrap();

        assert!(scheduler.check_at(now).await.is_err());
        scheduler.check_at(now).await.unwrap();
        scheduler.check_at(now).await.unwrap();

        assert_eq!(sink.notifications().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn interval_loop_checks_immediately_and_again_after_poll_interval() {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(StateStore::open(&directory.path().join("state.sqlite3")).unwrap());
        let sink = Arc::new(RecordingSink::default());
        let source = Arc::new(ScriptedSource {
            steps: Mutex::new(vec![SourceStep::Tasks(vec![])]),
            calls: AtomicUsize::new(0),
        });
        let scheduler = Arc::new(PushScheduler::new(
            source.clone(),
            state,
            sink,
            calendar(),
            reminders(),
            Duration::from_secs(30),
            chrono::Duration::hours(1),
            Duration::from_secs(10),
        ));

        let handle = tokio::spawn(scheduler.run());
        tokio::task::yield_now().await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
        handle.abort();
    }

    /// Wording is relative to the moment of sending, so the same reminder
    /// reads differently depending on when it actually goes out.
    #[test]
    fn a_body_says_why_now_as_well_as_when() {
        let mut timed = task("timed", "Сдать отчёт");
        timed.deadline = AnytypeDate::parse("2026-09-03T18:00:00+04:00");
        let moment = reminders_for(&timed, &calendar(), &reminders())[0];

        // Two hours ahead, on the day itself.
        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 9, 3, 12, 0, 0).unwrap(),
                Saratov
            ),
            "Дедлайн через 2 часа — сегодня в 18:00"
        );
        // The day before: the day itself already explains the push.
        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 9, 2, 12, 0, 0).unwrap(),
                Saratov
            ),
            "Дедлайн завтра в 18:00"
        );
        // Further out, a plain date.
        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap(),
                Saratov
            ),
            "Дедлайн 3 сентября в 18:00"
        );
        // Held back past the deadline: it must not still promise the future.
        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 9, 3, 15, 0, 0).unwrap(),
                Saratov
            ),
            "Дедлайн был сегодня в 18:00"
        );
    }

    #[test]
    fn an_all_day_task_names_the_day_and_nothing_else() {
        let mut task = task("all-day", "Купить хлеб");
        task.deadline = AnytypeDate::parse("2026-08-30T00:00:00+04:00");
        let moment = reminders_for(&task, &calendar(), &reminders())[0];

        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 0).unwrap(),
                Saratov
            ),
            "Дедлайн сегодня"
        );
        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 29, 5, 0, 0).unwrap(),
                Saratov
            ),
            "Дедлайн завтра"
        );
        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 31, 5, 0, 0).unwrap(),
                Saratov
            ),
            "Дедлайн был вчера"
        );
    }

    /// Without a deadline the scheduled date is the anchor, and it is not a
    /// deadline, so it must not be worded as one.
    #[test]
    fn the_scheduled_fallback_is_worded_as_a_plan() {
        let mut scheduled = task("scheduled", "Сходить в зал");
        scheduled.scheduled = AnytypeDate::parse("2026-08-30T15:13:00+04:00");
        let moment = reminders_for(&scheduled, &calendar(), &reminders())[0];

        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 30, 10, 58, 0).unwrap(),
                Saratov
            ),
            "По плану через 15 минут — сегодня в 15:13"
        );
        assert_eq!(
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 30, 12, 0, 0).unwrap(),
                Saratov
            ),
            "По плану было сегодня в 15:13"
        );
    }

    /// Russian counts agree in three forms and "1" reads better without it.
    #[test]
    fn counts_agree_with_their_nouns() {
        let mut task = task("t", "T");
        task.deadline = AnytypeDate::parse("2026-08-30T23:00:00+04:00");
        let moment = reminders_for(&task, &calendar(), &reminders())[0];
        let at = |h: u32, m: u32| {
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 30, h, m, 0).unwrap(),
                Saratov,
            )
        };

        assert!(at(18, 59).contains("через минуту"), "{}", at(18, 59));
        assert!(at(18, 57).contains("через 3 минуты"), "{}", at(18, 57));
        assert!(at(18, 45).contains("через 15 минут"), "{}", at(18, 45));
        assert!(at(18, 0).contains("через час"), "{}", at(18, 0));
        assert!(at(16, 0).contains("через 3 часа"), "{}", at(16, 0));
        assert!(at(11, 0).contains("через 8 часов"), "{}", at(11, 0));
    }

    /// The scheduler wakes on a 30-second tick, so a reminder is always sent a
    /// little after its trigger. Truncating the remainder made an hour's lead
    /// arrive as "через 59 минут", which reads as an off-by-one bug to whoever
    /// picked that lead. Observed in production before the rounding was fixed.
    #[test]
    fn a_reminder_sent_seconds_late_still_names_the_lead_the_user_chose() {
        let mut task = task("t", "T");
        // 19:00 UTC.
        task.deadline = AnytypeDate::parse("2026-08-30T23:00:00+04:00");
        let moment = reminders_for(&task, &calendar(), &reminders())[0];
        let at = |h: u32, m: u32, s: u32| {
            describe(
                moment,
                Utc.with_ymd_and_hms(2026, 8, 30, h, m, s).unwrap(),
                Saratov,
            )
        };

        // 59m40s left, from a lead of one hour.
        assert!(at(18, 0, 20).contains("через час"), "{}", at(18, 0, 20));
        // 14m40s left, from a lead of fifteen minutes.
        assert!(
            at(18, 45, 20).contains("через 15 минут"),
            "{}",
            at(18, 45, 20)
        );
        // 1h29m40s: rounding the hours off the already-rounded minutes would
        // reach 90 and answer "через 2 часа".
        assert!(at(17, 30, 20).contains("через час"), "{}", at(17, 30, 20));
    }
}
