//! Recurring tasks: a `recurring_task` object holds the rule, and ordinary
//! tasks stand for its occurrences.
//!
//! The generator keeps one invariant per series: a task exists for the first
//! occurrence whose day is after today. So the task for next Saturday appears
//! on this Saturday, the day of the previous occurrence, whether or not this
//! Saturday's task is done. Occurrences missed while the service was down are
//! not created afterwards; only the next one is.
//!
//! An instance is matched to an occurrence by the *day* of its `occurrence`
//! property, not the instant, so a date-only anchor and a timed one compare the
//! same way and a hand-made instance at a slightly different hour still counts.

use std::{
    collections::HashSet,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use anytype::{
    client::AnytypeClient,
    error::AnytypeError,
    objects::Object,
    properties::{PropertyValue, SetProperty},
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use futures::StreamExt;
use rrule::RRuleSet;
use tracing::{debug, error, info, warn};

use crate::{
    model::{AnytypeDate, CalendarValue},
    state::{StateError, StateStore},
};

pub const SERIES_TYPE: &str = "recurring_task";
pub const TASK_TYPE: &str = "task";

/// Iterations before a rule is declared unusable. A daily rule reaches ten
/// years in 3650 steps, so this only trips on a rule that never passes today.
const MAX_STEPS: usize = 10_000;

/// A `recurring_task` object, as far as the generator needs it.
#[derive(Debug, Clone)]
pub struct Series {
    pub id: String,
    pub name: String,
    pub rrule: Option<String>,
    pub anchor: Option<AnytypeDate>,
    pub priority: Option<String>,
    pub tags: Vec<String>,
    pub reminder_leads: Vec<String>,
}

/// A task that belongs to a series.
#[derive(Debug, Clone)]
pub struct Instance {
    pub series_id: String,
    pub occurrence: Option<AnytypeDate>,
}

/// A task the generator wants to exist.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned<'a> {
    pub series: &'a Series,
    /// RFC 3339, UTC, in the same shape Anytype stores: a date-only anchor
    /// yields local midnight, e.g. `2026-10-12T20:00:00Z` for 13 Oct.
    pub occurrence: String,
    pub day: NaiveDate,
}

impl PartialEq for Series {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SeriesError {
    #[error("anytype: {0}")]
    Anytype(#[from] AnytypeError),
    #[error("{0}")]
    State(#[from] StateError),
}

/// The first occurrence of `rule`, anchored at `anchor`, whose local day in
/// `tz` is after `today`. `Ok(None)` means the rule has ended.
pub fn next_after(
    rule: &str,
    anchor: &AnytypeDate,
    tz: Tz,
    today: NaiveDate,
) -> Result<Option<(DateTime<Utc>, NaiveDate)>, String> {
    let (start, all_day) = match anchor.classify(tz) {
        CalendarValue::AllDay(day) => (day.and_hms_opt(0, 0, 0).expect("midnight exists"), true),
        CalendarValue::Instant(instant) => (instant.with_timezone(&tz).naive_local(), false),
    };
    let text = format!(
        "DTSTART;TZID={}:{}\nRRULE:{}",
        tz.name(),
        start.format("%Y%m%dT%H%M%S"),
        rule.trim()
    );
    let set = RRuleSet::from_str(&text).map_err(|err| format!("{rule:?}: {err}"))?;

    for occurrence in (&set).into_iter().take(MAX_STEPS) {
        let local = occurrence.with_timezone(&tz);
        let day = local.date_naive();
        if day > today {
            // Re-derive midnight for all-day rules so a DST change in `tz`
            // cannot shift the stored value off the day boundary.
            let utc = if all_day {
                tz.from_local_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight exists"))
                    .earliest()
                    .map(|dt| dt.with_timezone(&Utc))
                    .ok_or_else(|| format!("{rule:?}: midnight of {day} does not exist in {tz}"))?
            } else {
                local.with_timezone(&Utc)
            };
            return Ok(Some((utc, day)));
        }
    }
    // Either the rule ended (UNTIL/COUNT) or it never reaches today.
    Ok(None)
}

/// Decides which tasks are missing. Pure: every branch is testable without a
/// running Anytype. Problems with one series are reported and never stop the
/// others.
pub fn plan<'a>(
    series: &'a [Series],
    instances: &[Instance],
    tz: Tz,
    today: NaiveDate,
) -> (Vec<Planned<'a>>, Vec<String>) {
    let mut planned = Vec::new();
    let mut warnings = Vec::new();
    for one in series {
        let (Some(rule), Some(anchor)) = (&one.rrule, &one.anchor) else {
            warnings.push(format!(
                "{:?}: no rule or no start date, nothing to generate",
                one.name
            ));
            continue;
        };
        let next = match next_after(rule, anchor, tz, today) {
            Ok(Some(next)) => next,
            Ok(None) => continue,
            Err(err) => {
                warnings.push(format!("{:?}: bad rule {err}", one.name));
                continue;
            }
        };
        let exists = instances.iter().any(|instance| {
            instance.series_id == one.id
                && instance
                    .occurrence
                    .as_ref()
                    .is_some_and(|date| date.parsed.with_timezone(&tz).date_naive() == next.1)
        });
        if !exists {
            planned.push(Planned {
                series: one,
                occurrence: next.0.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                day: next.1,
            });
        }
    }
    (planned, warnings)
}

/// Reads series and their instances from one space, and creates instances.
pub struct AnytypeSeries {
    client: AnytypeClient,
    space_id: String,
}

impl AnytypeSeries {
    pub fn new(client: AnytypeClient, space_id: String) -> Self {
        Self { client, space_id }
    }

    async fn objects(&self, type_key: &str) -> Result<Vec<Object>, SeriesError> {
        let paged = self
            .client
            .search_in(&self.space_id)
            .types([type_key])
            .execute()
            .await?;
        let mut stream = paged.into_stream();
        let mut objects = Vec::new();
        while let Some(object) = stream.next().await {
            let object = object?;
            if !object.archived {
                objects.push(object);
            }
        }
        Ok(objects)
    }

    pub async fn series(&self) -> Result<Vec<Series>, SeriesError> {
        Ok(self
            .objects(SERIES_TYPE)
            .await?
            .into_iter()
            .map(|object| Series {
                id: object.id.clone(),
                name: object.name.clone().unwrap_or_default(),
                rrule: text(&object, "rrule"),
                anchor: date(&object, "start_date"),
                priority: object
                    .get_property_select("priority")
                    .map(|tag| tag.id.clone()),
                tags: tag_ids(&object, "tag"),
                reminder_leads: tag_ids(&object, "reminder_lead"),
            })
            .collect())
    }

    pub async fn instances(&self) -> Result<Vec<Instance>, SeriesError> {
        let mut instances = Vec::new();
        for object in self.objects(TASK_TYPE).await? {
            let Some(links) = object.get_property_array("series") else {
                continue;
            };
            for series_id in links {
                instances.push(Instance {
                    series_id,
                    occurrence: date(&object, "occurrence"),
                });
            }
        }
        Ok(instances)
    }

    /// Creates the task for one occurrence, copying what the series carries.
    pub async fn create(&self, planned: &Planned<'_>) -> Result<String, SeriesError> {
        let series = planned.series;
        let mut request = self
            .client
            .new_object(&self.space_id, TASK_TYPE)
            .name(series.name.clone())
            .set_objects("series", [series.id.clone()])
            .set_date("occurrence", planned.occurrence.clone())
            .set_date("scheduled", planned.occurrence.clone());
        if let Some(priority) = &series.priority {
            request = request.set_select("priority", priority.clone());
        }
        if !series.tags.is_empty() {
            request = request.set_multi_select("tag", series.tags.clone());
        }
        if !series.reminder_leads.is_empty() {
            request = request.set_multi_select("reminder_lead", series.reminder_leads.clone());
        }
        Ok(request.create().await?.id)
    }
}

/// Runs the plan on a timer inside the service.
pub struct SeriesGenerator {
    source: AnytypeSeries,
    state: Arc<StateStore>,
    tz: Tz,
    poll_interval: Duration,
    /// Warnings already logged, so a series without a date does not repeat
    /// itself every five minutes.
    reported: Mutex<HashSet<String>>,
}

impl SeriesGenerator {
    pub fn new(
        source: AnytypeSeries,
        state: Arc<StateStore>,
        tz: Tz,
        poll_interval: Duration,
    ) -> Self {
        Self {
            source,
            state,
            tz,
            poll_interval,
            reported: Mutex::new(HashSet::new()),
        }
    }

    /// One pass. Returns how many tasks were created.
    ///
    /// Each task is claimed in the state database before it is created. The
    /// claim covers the gap in which a just-created task is not yet visible to
    /// search, and it keeps a task the user deleted from coming back. A failed
    /// create releases its claim so the next pass retries.
    pub async fn check_at(&self, now: DateTime<Utc>) -> Result<usize, SeriesError> {
        let today = now.with_timezone(&self.tz).date_naive();
        let series = self.source.series().await?;
        let instances = self.source.instances().await?;
        let (planned, warnings) = plan(&series, &instances, self.tz, today);

        if let Ok(mut reported) = self.reported.lock() {
            for warning in warnings {
                if reported.insert(warning.clone()) {
                    warn!(%warning, "recurring task skipped");
                }
            }
        }

        let mut created = 0;
        for one in planned {
            if !self.state.claim_instance(&one.series.id, one.day)? {
                debug!(series = %one.series.name, day = %one.day, "already created once");
                continue;
            }
            match self.source.create(&one).await {
                Ok(id) => {
                    info!(series = %one.series.name, day = %one.day, %id, "created recurring task");
                    created += 1;
                }
                Err(err) => {
                    error!(series = %one.series.name, day = %one.day, error = %err, "cannot create recurring task");
                    self.state.release_instance(&one.series.id, one.day)?;
                }
            }
        }
        Ok(created)
    }

    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match self.check_at(Utc::now()).await {
                Ok(created) => debug!(created, "generator check completed"),
                Err(err) => warn!(error = %err, "generator check failed"),
            }
        }
    }
}

fn text(object: &Object, key: &str) -> Option<String> {
    match &object.get_property(key)?.value {
        PropertyValue::Text { text } => Some(text.clone()).filter(|t| !t.trim().is_empty()),
        _ => None,
    }
}

fn date(object: &Object, key: &str) -> Option<AnytypeDate> {
    match &object.get_property(key)?.value {
        PropertyValue::Date { date } => AnytypeDate::parse(date),
        _ => None,
    }
}

fn tag_ids(object: &Object, key: &str) -> Vec<String> {
    object
        .get_property_multi_select(key)
        .map(|tags| tags.iter().map(|tag| tag.id.clone()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Saratov;

    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(raw: &str) -> AnytypeDate {
        AnytypeDate::parse(raw).unwrap()
    }

    fn series(id: &str, rule: &str, anchor: &str) -> Series {
        Series {
            id: id.into(),
            name: id.into(),
            rrule: Some(rule.into()),
            anchor: Some(at(anchor)),
            priority: None,
            tags: vec![],
            reminder_leads: vec![],
        }
    }

    fn instance(series_id: &str, occurrence: &str) -> Instance {
        Instance {
            series_id: series_id.into(),
            occurrence: Some(at(occurrence)),
        }
    }

    // Saturday 2026-09-19 at 12:00 Saratov is 08:00Z.
    const GUITAR: &str = "2026-09-19T08:00:00Z";

    #[test]
    fn on_the_day_of_an_occurrence_the_next_one_is_due() {
        let (utc, next) = next_after("FREQ=WEEKLY", &at(GUITAR), Saratov, day(2026, 9, 19))
            .unwrap()
            .unwrap();
        assert_eq!(next, day(2026, 9, 26));
        assert_eq!(utc.to_rfc3339(), "2026-09-26T08:00:00+00:00");
    }

    #[test]
    fn the_day_before_an_occurrence_that_occurrence_is_the_next_one() {
        let (_, next) = next_after("FREQ=WEEKLY", &at(GUITAR), Saratov, day(2026, 9, 18))
            .unwrap()
            .unwrap();
        assert_eq!(next, day(2026, 9, 19));
    }

    /// Anytype stores 13 Sep date-only as 12 Sep 20:00Z; the next monthly
    /// occurrence must come back in that same shape.
    #[test]
    fn an_all_day_anchor_stays_local_midnight() {
        let (utc, next) = next_after(
            "FREQ=MONTHLY",
            &at("2026-09-12T20:00:00Z"),
            Saratov,
            day(2026, 9, 13),
        )
        .unwrap()
        .unwrap();
        assert_eq!(next, day(2026, 10, 13));
        assert_eq!(utc.to_rfc3339(), "2026-10-12T20:00:00+00:00");
    }

    #[test]
    fn last_day_of_month_follows_the_month_length() {
        let (_, next) = next_after(
            "FREQ=MONTHLY;BYMONTHDAY=-1",
            &at("2026-08-30T20:00:00Z"),
            Saratov,
            day(2026, 9, 30),
        )
        .unwrap()
        .unwrap();
        assert_eq!(next, day(2026, 10, 31));
    }

    /// After downtime only the next occurrence is due, never the missed ones.
    #[test]
    fn missed_occurrences_are_skipped() {
        let (_, next) = next_after("FREQ=WEEKLY", &at(GUITAR), Saratov, day(2026, 11, 4))
            .unwrap()
            .unwrap();
        assert_eq!(next, day(2026, 11, 7));
    }

    #[test]
    fn an_ended_rule_plans_nothing() {
        let ended = next_after(
            "FREQ=WEEKLY;COUNT=2",
            &at(GUITAR),
            Saratov,
            day(2026, 12, 1),
        )
        .unwrap();
        assert_eq!(ended, None);
    }

    #[test]
    fn a_missing_instance_is_planned_and_an_existing_one_is_not() {
        let all = [
            series("guitar", "FREQ=WEEKLY", GUITAR),
            series("rent", "FREQ=MONTHLY", "2026-09-12T20:00:00Z"),
        ];
        let instances = [instance("rent", "2026-10-12T20:00:00Z")];
        let (planned, warnings) = plan(&all, &instances, Saratov, day(2026, 9, 19));
        assert!(warnings.is_empty());
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].series.id, "guitar");
        assert_eq!(planned[0].day, day(2026, 9, 26));
    }

    /// An instance made by hand at another hour of the same day still counts.
    #[test]
    fn instances_match_by_day_not_by_instant() {
        let all = [series("guitar", "FREQ=WEEKLY", GUITAR)];
        let instances = [instance("guitar", "2026-09-26T15:30:00Z")];
        let (planned, _) = plan(&all, &instances, Saratov, day(2026, 9, 19));
        assert!(planned.is_empty());
    }

    #[test]
    fn an_instance_of_another_series_does_not_count() {
        let all = [series("guitar", "FREQ=WEEKLY", GUITAR)];
        let instances = [instance("rent", "2026-09-26T08:00:00Z")];
        let (planned, _) = plan(&all, &instances, Saratov, day(2026, 9, 19));
        assert_eq!(planned.len(), 1);
    }

    #[test]
    fn broken_series_are_reported_and_do_not_stop_the_rest() {
        let mut no_anchor = series("haircut", "FREQ=DAILY;INTERVAL=45", GUITAR);
        no_anchor.anchor = None;
        let all = [
            no_anchor,
            series("bad", "FREQ=SOMETIMES", GUITAR),
            series("guitar", "FREQ=WEEKLY", GUITAR),
        ];
        let (planned, warnings) = plan(&all, &[], Saratov, day(2026, 9, 19));
        assert_eq!(planned.len(), 1);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
