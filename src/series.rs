//! Recurring tasks and events: a `recurring_task` or `recurring_event` object
//! holds the rule, and ordinary tasks or events stand for its occurrences.
//! Events are generated the way tasks are; a meeting made for an occurrence is
//! an event of its own, with its own document, created from the event
//! template named as the series, or from the event type's only template.
//!
//! The generator keeps one invariant per series: a task exists for the first
//! occurrence whose day is after today. So the task for next Saturday appears
//! on this Saturday, the day of the previous occurrence, whether or not this
//! Saturday's task is done. Today's occurrence is made too when it has no
//! instance, so a series that starts today shows today. Occurrences missed
//! while the service was down are not created afterwards; only today's and the
//! next one are.
//!
//! An instance is matched to an occurrence by the *day* of its `occurrence`
//! property, not the instant, so a date-only anchor and a timed one compare the
//! same way and a hand-made instance at a slightly different hour still counts.
//!
//! Meetings ahead follow their series: when its time, length, name, place,
//! tags or reminders change, the meetings the generator made are rewritten,
//! and when the series is archived or its rule drops a day, those meetings are
//! archived. A meeting somebody changed by hand is left as it is; the state
//! file keeps what the generator last wrote to each meeting to tell. The body
//! of a meeting does not count for a change of its fields, but a meeting
//! whose body differs from the one it was made with is never archived. Tasks
//! are not maintained this way.
//!
//! An event series' `exdate` leaves days out: no meeting is made for them. A
//! meeting deleted by hand joins the `exdate` of its series, so that a
//! calendar expanding the rule does not show that day again.

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anytype::{
    client::AnytypeClient,
    error::AnytypeError,
    objects::{Icon, Object},
    properties::{PropertyValue, SetProperty},
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use futures::StreamExt;
use rrule::RRuleSet;
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::{
    model::{AnytypeDate, CalendarValue},
    state::{Claim, StateError, StateStore},
};

pub const SERIES_TYPE: &str = "recurring_task";
pub const TASK_TYPE: &str = "task";
pub const EVENT_SERIES_TYPE: &str = "recurring_event";
pub const EVENT_TYPE: &str = "event";

/// What a series makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Task,
    Event,
}

impl Kind {
    pub fn series_type(self) -> &'static str {
        match self {
            Kind::Task => SERIES_TYPE,
            Kind::Event => EVENT_SERIES_TYPE,
        }
    }

    pub fn instance_type(self) -> &'static str {
        match self {
            Kind::Task => TASK_TYPE,
            Kind::Event => EVENT_TYPE,
        }
    }
}

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
    /// An event series' end on its first day; the length of every meeting.
    pub end: Option<AnytypeDate>,
    pub address: Option<String>,
    /// Given to every event the series makes.
    pub icon: Option<Icon>,
    /// Every property the object has, for `carried`.
    pub extra: Vec<(String, PropertyValue)>,
    /// Occurrences left out of the series (`exdate`, one per line): no
    /// meeting is made for their day.
    pub exdates: Vec<AnytypeDate>,
}

impl Series {
    /// Whether the occurrence on `day` is left out of the series.
    pub fn excludes(&self, tz: Tz, day: NaiveDate) -> bool {
        self.exdates
            .iter()
            .any(|excluded| excluded.parsed.with_timezone(&tz).date_naive() == day)
    }
}

/// Keys the generator sets from its own fields, and those Anytype keeps for
/// itself: `carried` leaves them alone.
const OWN_KEYS: &[&str] = &[
    "name",
    "rrule",
    "start_date",
    "end_date",
    "address",
    "tag",
    "reminder_lead",
    "series",
    "occurrence",
    "ical_uid",
    "exdate",
    "backlinks",
    "links",
    "mentions",
    "creator",
    "created_date",
    "added_date",
    "last_modified_date",
    "last_modified_by",
    "last_opened_date",
];

/// What an event series hands to every event it makes beyond the fields the
/// generator sets itself: each property that the event type also has and that
/// holds a value. A date moves with the occurrence, so a deadline two days
/// before the first meeting is two days before every one. Anytype refuses a
/// property the type lacks, and with it the whole event, so those are left out.
pub fn carried(
    series: &Series,
    event_keys: &HashSet<String>,
    occurrence: DateTime<Utc>,
) -> Vec<(String, PropertyValue)> {
    let shift = series
        .anchor
        .as_ref()
        .map(|anchor| occurrence - anchor.parsed.with_timezone(&Utc));
    series
        .extra
        .iter()
        .filter(|(key, _)| event_keys.contains(key) && !OWN_KEYS.contains(&key.as_str()))
        .filter_map(|(key, value)| {
            let value = match value {
                PropertyValue::Date { date } => {
                    let moved = AnytypeDate::parse(date)?.parsed.with_timezone(&Utc) + shift?;
                    PropertyValue::Date {
                        date: moved.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    }
                }
                PropertyValue::Text { text } if text.trim().is_empty() => return None,
                PropertyValue::MultiSelect { multi_select } if multi_select.is_empty() => {
                    return None;
                }
                PropertyValue::Objects { objects } if objects.is_empty() => return None,
                PropertyValue::Files { files } if files.is_empty() => return None,
                other => other.clone(),
            };
            Some((key.clone(), value))
        })
        .collect()
}

/// Puts one value read from an object onto a request for another.
fn with_value<R: SetProperty>(request: R, key: String, value: PropertyValue) -> R {
    match value {
        PropertyValue::Text { text } => request.set_text(key, text),
        PropertyValue::Number { number } => request.set_number(key, number),
        PropertyValue::Select { select } => request.set_select(key, select.id),
        PropertyValue::MultiSelect { multi_select } => {
            request.set_multi_select(key, multi_select.into_iter().map(|tag| tag.id))
        }
        PropertyValue::Date { date } => request.set_date(key, date),
        PropertyValue::Files { files } => request.set_files(key, files),
        PropertyValue::Checkbox { checkbox } => request.set_checkbox(key, checkbox),
        PropertyValue::Url { url } => request.set_url(key, url),
        PropertyValue::Email { email } => request.set_email(key, email),
        PropertyValue::Phone { phone } => request.set_phone(key, phone),
        PropertyValue::Objects { objects } => request.set_objects(key, objects),
    }
}

/// The name of the event made for one occurrence: the series' name and the
/// day, `Собрание Core Team 24.09.2026`, so that meetings of one series can be
/// told apart in lists and links.
pub fn event_name(series: &Series, day: NaiveDate) -> String {
    format!("{} {}", series.name.trim(), day.format("%d.%m.%Y"))
}

/// The end of the event made for an occurrence at `occurrence`, as long as the
/// series' first one. `None` when the series has no end after its start.
pub fn event_end(series: &Series, occurrence: DateTime<Utc>) -> Option<String> {
    let (start, end) = (series.anchor.as_ref()?, series.end.as_ref()?);
    let length = end.parsed.signed_duration_since(start.parsed);
    (length > chrono::Duration::zero())
        .then(|| (occurrence + length).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// An icon as it can be given to a new object. The API reads an image icon
/// back as the file's download URL, and takes only the file's id.
pub fn reusable_icon(icon: Icon) -> Icon {
    match icon {
        Icon::File { file } => Icon::File {
            file: file.rsplit('/').next().unwrap_or_default().to_string(),
        },
        other => other,
    }
}

/// A task or event that belongs to a series.
#[derive(Debug, Clone)]
pub struct Instance {
    /// The task's own id, kept for the log: which task satisfied an occurrence.
    pub object_id: String,
    pub series_id: String,
    pub occurrence: Option<AnytypeDate>,
    /// What it holds of the fields the generator sets on a meeting.
    pub shape: Shape,
}

/// The fields the generator sets on a meeting. What a meeting holds is
/// compared with what the generator wrote to tell whether anybody changed it
/// since. Instants are Unix seconds, so the form in which Anytype gives a date
/// back does not count as a change, and tags are sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shape {
    pub name: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub address: Option<String>,
    pub tags: Vec<String>,
    pub reminder_leads: Vec<String>,
}

impl Shape {
    /// The meeting a series makes for one occurrence.
    pub fn of_meeting(series: &Series, occurrence: DateTime<Utc>, day: NaiveDate) -> Self {
        Self {
            name: event_name(series, day),
            start: Some(occurrence.timestamp()),
            end: event_end(series, occurrence)
                .and_then(|end| AnytypeDate::parse(&end))
                .map(|end| end.parsed.timestamp()),
            address: series
                .address
                .clone()
                .filter(|address| !address.trim().is_empty()),
            tags: sorted(series.tags.clone()),
            reminder_leads: sorted(series.reminder_leads.clone()),
        }
    }

    pub fn of_object(object: &Object) -> Self {
        Self {
            name: object.name.clone().unwrap_or_default().trim().to_string(),
            start: date(object, "start_date").map(|d| d.parsed.timestamp()),
            end: date(object, "end_date").map(|d| d.parsed.timestamp()),
            address: text(object, "address"),
            tags: sorted(tag_ids(object, "tag")),
            reminder_leads: sorted(tag_ids(object, "reminder_lead")),
        }
    }

    /// The form kept in the state file.
    pub fn fingerprint(&self) -> String {
        serde_json::json!([
            self.name,
            self.start,
            self.end,
            self.address,
            self.tags,
            self.reminder_leads
        ])
        .to_string()
    }
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
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
    Ok(upcoming(rule, anchor, tz, today, today)?
        .into_iter()
        .find(|(_, day)| *day > today))
}

/// The occurrences whose local day is `today` or later and no later than
/// `until`, and always the first one after `today` even when it falls later.
/// Today's is there so that a series starting today makes today's instance;
/// one already made is matched and skipped. Empty means the rule has ended.
pub fn upcoming(
    rule: &str,
    anchor: &AnytypeDate,
    tz: Tz,
    today: NaiveDate,
    until: NaiveDate,
) -> Result<Vec<(DateTime<Utc>, NaiveDate)>, String> {
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

    let mut found = Vec::new();
    for occurrence in (&set).into_iter().take(MAX_STEPS) {
        let local = occurrence.with_timezone(&tz);
        let day = local.date_naive();
        if day > until && found.iter().any(|(_, found)| *found > today) {
            break;
        }
        if day >= today {
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
            found.push((utc, day));
        }
    }
    // Empty: either the rule ended (UNTIL/COUNT) or it never reaches today.
    Ok(found)
}

/// Decides which tasks or events are missing, for every occurrence from
/// today to `until` and at least the next one. Pure: every branch is
/// testable without a running Anytype. Problems with one series are reported
/// and never stop the others.
pub fn plan_until<'a>(
    series: &'a [Series],
    instances: &[Instance],
    tz: Tz,
    today: NaiveDate,
    until: NaiveDate,
) -> (Vec<Planned<'a>>, Vec<String>) {
    let mut planned = Vec::new();
    let mut warnings = Vec::new();
    for one in series {
        let (Some(rule), Some(anchor)) = (&one.rrule, &one.anchor) else {
            debug!(
                series_id = %one.id,
                series = %one.name,
                rrule = ?one.rrule,
                anchor = ?one.anchor.as_ref().map(|a| &a.raw),
                decision = "skip: missing rule or start date",
                "series decision"
            );
            warnings.push(format!(
                "{:?}: no rule or no start date, nothing to generate",
                one.name
            ));
            continue;
        };
        let wanted = match upcoming(rule, anchor, tz, today, until) {
            Ok(wanted) if !wanted.is_empty() => wanted,
            Ok(_) => {
                debug!(
                    series_id = %one.id,
                    series = %one.name,
                    rrule = %rule,
                    anchor = %anchor.raw,
                    %today,
                    decision = "skip: rule has no occurrence after today",
                    "series decision"
                );
                continue;
            }
            Err(err) => {
                debug!(
                    series_id = %one.id,
                    series = %one.name,
                    rrule = %rule,
                    anchor = %anchor.raw,
                    error = %err,
                    decision = "skip: rule does not parse",
                    "series decision"
                );
                warnings.push(format!("{:?}: bad rule {err}", one.name));
                continue;
            }
        };
        let own: Vec<&Instance> = instances.iter().filter(|i| i.series_id == one.id).collect();
        for next in wanted
            .into_iter()
            .filter(|(_, day)| !one.excludes(tz, *day))
        {
            let matching = own.iter().find(|instance| {
                instance
                    .occurrence
                    .as_ref()
                    .is_some_and(|date| date.parsed.with_timezone(&tz).date_naive() == next.1)
            });
            let exists = matching.is_some();
            debug!(
                series_id = %one.id,
                series = %one.name,
                rrule = %rule,
                anchor = %anchor.raw,
                %today,
                %until,
                next_day = %next.1,
                next_utc = %next.0,
                instances = own.len(),
                matched_instance = ?matching.map(|i| &i.object_id),
                decision = if exists { "skip: occurrence already has an instance" } else { "create" },
                "series decision"
            );
            if !exists {
                planned.push(Planned {
                    series: one,
                    occurrence: next.0.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    day: next.1,
                });
            }
        }
    }
    (planned, warnings)
}

/// Decides which tasks are missing for the next occurrence alone.
pub fn plan<'a>(
    series: &'a [Series],
    instances: &[Instance],
    tz: Tz,
    today: NaiveDate,
) -> (Vec<Planned<'a>>, Vec<String>) {
    plan_until(series, instances, tz, today, today)
}

/// What becomes of a future meeting the generator made, once its series
/// changed. A meeting somebody changed by hand is never touched.
#[derive(Debug, Clone, PartialEq)]
pub enum Upkeep<'a> {
    /// The series now makes this meeting differently: another time, length,
    /// name, place, tags or reminders.
    Update {
        object_id: String,
        planned: Planned<'a>,
    },
    /// The series is gone, or its rule no longer has this day. The meeting
    /// goes only if its body is as it was made, `body` being the digest
    /// recorded then: notes written in it keep it.
    Archive {
        object_id: String,
        series_id: String,
        day: NaiveDate,
        body: Option<String>,
    },
    /// A meeting made before fingerprints were kept, exactly as its series
    /// makes it now: from here on it counts as unchanged, and its body as it
    /// is now counts as made.
    Adopt {
        object_id: String,
        series_id: String,
        day: NaiveDate,
        fingerprint: String,
    },
    /// Occurrences whose meeting was made or skipped once and is gone now,
    /// deleted by hand: they join the series' `exdate`, so a calendar that
    /// expands the rule leaves them out as well.
    Exclude {
        series: &'a Series,
        occurrences: Vec<String>,
    },
}

/// How old a claim without a meeting has to be to count as a meeting deleted:
/// a meeting just made may not be found by search yet.
const SETTLED: chrono::TimeDelta = chrono::TimeDelta::minutes(10);

/// The occurrence of `series` on `day`. `Err` when the series has no rule or
/// start, or its rule does not parse: then nothing can be said about the day.
fn occurrence_on(series: &Series, tz: Tz, day: NaiveDate) -> Result<Option<DateTime<Utc>>, ()> {
    let (Some(rule), Some(anchor)) = (&series.rrule, &series.anchor) else {
        return Err(());
    };
    if series.excludes(tz, day) {
        return Ok(None);
    }
    Ok(upcoming(rule, anchor, tz, day, day)
        .map_err(|_| ())?
        .into_iter()
        .find(|(_, found)| *found == day)
        .map(|(at, _)| at))
}

/// Decides what to do with the meetings still ahead that the generator made:
/// those with a claim on their series and day. A meeting counts as unchanged
/// when it holds what the generator last wrote to it, as recorded in the
/// claim. Pure, like `plan_until`.
pub fn plan_upkeep<'a>(
    series: &'a [Series],
    instances: &[Instance],
    claims: &HashMap<(String, NaiveDate), Claim>,
    tz: Tz,
    now: DateTime<Utc>,
) -> Vec<Upkeep<'a>> {
    let today = now.with_timezone(&tz).date_naive();
    let mut upkeep = Vec::new();
    for instance in instances {
        let Some(occurrence) = &instance.occurrence else {
            continue;
        };
        let day = occurrence.parsed.with_timezone(&tz).date_naive();
        let start = instance
            .shape
            .start
            .unwrap_or_else(|| occurrence.parsed.timestamp());
        if day < today || start <= now.timestamp() {
            continue;
        }
        // Without a claim the meeting was made by hand.
        let Some(claim) = claims.get(&(instance.series_id.clone(), day)) else {
            continue;
        };
        let recorded = &claim.fingerprint;
        let current = instance.shape.fingerprint();
        let unchanged = recorded.as_deref() == Some(current.as_str());
        let owner = series.iter().find(|one| one.id == instance.series_id);
        let wanted = match owner.map(|one| (one, occurrence_on(one, tz, day))) {
            None => None,
            Some((one, Ok(Some(at)))) => Some((one, at)),
            Some((_, Ok(None))) => None,
            // A rule being edited says nothing about the meetings.
            Some((_, Err(()))) => continue,
        };
        let decision = match wanted {
            None if unchanged => {
                upkeep.push(Upkeep::Archive {
                    object_id: instance.object_id.clone(),
                    series_id: instance.series_id.clone(),
                    day,
                    body: claim.body.clone(),
                });
                "archive: the series no longer has this day"
            }
            None => "keep: no longer in the series, but changed by hand",
            Some((one, at)) => {
                let expected = Shape::of_meeting(one, at, day).fingerprint();
                if current == expected {
                    if recorded.is_none() {
                        upkeep.push(Upkeep::Adopt {
                            object_id: instance.object_id.clone(),
                            series_id: one.id.clone(),
                            day,
                            fingerprint: expected,
                        });
                        "adopt: as the series makes it"
                    } else {
                        "keep: as the series makes it"
                    }
                } else if unchanged {
                    upkeep.push(Upkeep::Update {
                        object_id: instance.object_id.clone(),
                        planned: Planned {
                            series: one,
                            occurrence: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                            day,
                        },
                    });
                    "update: the series changed"
                } else {
                    "keep: changed by hand"
                }
            }
        };
        debug!(
            series_id = %instance.series_id,
            object_id = %instance.object_id,
            %day,
            decision,
            "upkeep decision"
        );
    }

    let mut excluded: Vec<(&Series, Vec<String>)> = Vec::new();
    for ((series_id, day), claim) in claims {
        if *day < today || claim.claimed_at > now - SETTLED {
            continue;
        }
        let Some(one) = series.iter().find(|one| &one.id == series_id) else {
            continue;
        };
        let has_meeting = instances.iter().any(|instance| {
            &instance.series_id == series_id
                && instance
                    .occurrence
                    .as_ref()
                    .is_some_and(|o| o.parsed.with_timezone(&tz).date_naive() == *day)
        });
        let Ok(Some(at)) = occurrence_on(one, tz, *day) else {
            continue;
        };
        if has_meeting {
            continue;
        }
        debug!(series_id, %day, decision = "exclude: its meeting was deleted", "upkeep decision");
        let occurrence = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match excluded.iter_mut().find(|(found, _)| found.id == one.id) {
            Some((_, occurrences)) => occurrences.push(occurrence),
            None => excluded.push((one, vec![occurrence])),
        }
    }
    for (one, mut occurrences) in excluded {
        occurrences.sort();
        upkeep.push(Upkeep::Exclude {
            series: one,
            occurrences,
        });
    }
    upkeep
}

/// Reads series and their instances from one space, and creates instances.
pub struct AnytypeSeries {
    client: AnytypeClient,
    space_id: String,
    kind: Kind,
}

impl AnytypeSeries {
    pub fn new(client: AnytypeClient, space_id: String, kind: Kind) -> Self {
        Self {
            client,
            space_id,
            kind,
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    async fn objects(&self, type_key: &str) -> Result<Vec<Object>, SeriesError> {
        let started = Instant::now();
        debug!(type_key, space_id = %self.space_id, "anytype search started");
        let paged = self
            .client
            .search_in(&self.space_id)
            .types([type_key])
            .execute()
            .await
            .inspect_err(|err| {
                error!(type_key, error = %err, elapsed_ms = started.elapsed().as_millis(), "anytype search failed");
            })?;
        let mut stream = paged.into_stream();
        let mut objects = Vec::new();
        let mut archived = 0usize;
        while let Some(object) = stream.next().await {
            let object = object.inspect_err(|err| {
                error!(type_key, error = %err, read = objects.len(), "anytype search page failed");
            })?;
            if object.archived {
                archived += 1;
            } else {
                objects.push(object);
            }
        }
        debug!(
            type_key,
            live = objects.len(),
            archived,
            elapsed_ms = started.elapsed().as_millis(),
            "anytype search finished"
        );
        Ok(objects)
    }

    pub async fn series(&self) -> Result<Vec<Series>, SeriesError> {
        Ok(self
            .objects(self.kind.series_type())
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
                end: date(&object, "end_date"),
                address: text(&object, "address"),
                icon: object.icon.clone().map(reusable_icon),
                extra: object
                    .properties
                    .iter()
                    .map(|property| (property.key.clone(), property.value.clone()))
                    .collect(),
                exdates: text(&object, "exdate")
                    .map(|values| values.lines().filter_map(AnytypeDate::parse).collect())
                    .unwrap_or_default(),
            })
            .collect())
    }

    pub async fn instances(&self) -> Result<Vec<Instance>, SeriesError> {
        let mut instances = Vec::new();
        for object in self.objects(self.kind.instance_type()).await? {
            let Some(links) = object.get_property_array("series") else {
                continue;
            };
            if links.len() > 1 {
                warn!(task_id = %object.id, task = ?object.name, series = ?links, "task links to more than one series; it counts for each");
            }
            for series_id in links {
                instances.push(Instance {
                    object_id: object.id.clone(),
                    series_id,
                    occurrence: date(&object, "occurrence"),
                    shape: Shape::of_object(&object),
                });
            }
        }
        Ok(instances)
    }

    /// Creates the task or event for one occurrence.
    pub async fn create(&self, planned: &Planned<'_>) -> Result<String, SeriesError> {
        match self.kind {
            Kind::Task => self.create_task(planned).await,
            Kind::Event => self.create_event(planned).await,
        }
    }

    /// The event template named as the series, or else the event type's only
    /// template. The REST API does not say which template is a type's default,
    /// so a type with several and none named as the series gets none.
    async fn template_for(
        &self,
        event_type: &anytype::types::Type,
        name: &str,
    ) -> Result<Option<String>, SeriesError> {
        let templates = self
            .client
            .templates(&self.space_id, &event_type.id)
            .list()
            .await?
            .collect_all()
            .await?;
        let mut named = templates
            .iter()
            .filter(|t| t.name.as_deref().map(str::trim) == Some(name.trim()));
        let found = named.next().map(|t| t.id.clone());
        if named.next().is_some() {
            warn!(
                series = name,
                "several event templates carry the series' name; using the first"
            );
        }
        Ok(match (found, templates.as_slice()) {
            (Some(id), _) => Some(id),
            (None, [only]) => Some(only.id.clone()),
            (None, _) => None,
        })
    }

    /// Creates the event for one occurrence: the series' name and day, its
    /// place, tags and reminders, and the length of its first meeting.
    async fn create_event(&self, planned: &Planned<'_>) -> Result<String, SeriesError> {
        let series = planned.series;
        let started = Instant::now();
        let event_type = self
            .client
            .types(&self.space_id)
            .list()
            .await?
            .collect_all()
            .await?
            .into_iter()
            .find(|t| t.key == EVENT_TYPE);
        let template = match &event_type {
            Some(event_type) => self.template_for(event_type, &series.name).await?,
            None => None,
        };
        let event_keys: HashSet<String> = event_type
            .iter()
            .flat_map(|t| t.properties.iter().map(|p| p.key.clone()))
            .collect();
        let at = DateTime::parse_from_rfc3339(&planned.occurrence)
            .map(|at| at.with_timezone(&Utc))
            .expect("the planner writes RFC 3339");
        debug!(
            series_id = %series.id,
            series = %series.name,
            occurrence = %planned.occurrence,
            template = ?template,
            "anytype create event started"
        );
        let mut request = self
            .client
            .new_object(&self.space_id, EVENT_TYPE)
            .name(event_name(series, planned.day))
            .set_objects("series", [series.id.clone()])
            .set_date("occurrence", planned.occurrence.clone())
            .set_date("start_date", planned.occurrence.clone());
        if let Some(end) = event_end(series, at) {
            request = request.set_date("end_date", end);
        }
        if let Some(address) = &series.address {
            request = request.set_text("address", address.clone());
        }
        if !series.tags.is_empty() {
            request = request.set_multi_select("tag", series.tags.clone());
        }
        if !series.reminder_leads.is_empty() {
            request = request.set_multi_select("reminder_lead", series.reminder_leads.clone());
        }
        let carried = carried(series, &event_keys, at);
        debug!(series_id = %series.id, keys = ?carried.iter().map(|(k, _)| k).collect::<Vec<_>>(), "series properties carried to the event");
        for (key, value) in carried {
            request = with_value(request, key, value);
        }
        if let Some(template) = template {
            request = request.template(template);
        }
        if let Some(icon) = &series.icon {
            request = request.icon(icon.clone());
        }
        let object = request.create().await.inspect_err(|err| {
            error!(
                series_id = %series.id,
                day = %planned.day,
                error = %err,
                elapsed_ms = started.elapsed().as_millis(),
                "anytype create event failed"
            );
        })?;
        debug!(
            series_id = %series.id,
            event_id = %object.id,
            elapsed_ms = started.elapsed().as_millis(),
            "anytype create event finished"
        );
        Ok(object.id)
    }

    /// Rewrites a meeting the way its series makes it now. What the series
    /// carries beyond its own fields is left as it was.
    pub async fn update_event(
        &self,
        object_id: &str,
        planned: &Planned<'_>,
    ) -> Result<(), SeriesError> {
        let series = planned.series;
        let at = DateTime::parse_from_rfc3339(&planned.occurrence)
            .map(|at| at.with_timezone(&Utc))
            .expect("the planner writes RFC 3339");
        let mut request = self
            .client
            .update_object(&self.space_id, object_id)
            .name(event_name(series, planned.day))
            .set_date("occurrence", planned.occurrence.clone())
            .set_date("start_date", planned.occurrence.clone())
            .set_text("address", series.address.clone().unwrap_or_default())
            .set_multi_select("tag", series.tags.clone())
            .set_multi_select("reminder_lead", series.reminder_leads.clone());
        if let Some(end) = event_end(series, at) {
            request = request.set_date("end_date", end);
        }
        request.update().await.inspect_err(|err| {
            error!(object_id, series_id = %series.id, day = %planned.day, error = %err, "anytype update event failed");
        })?;
        Ok(())
    }

    /// A digest of a meeting's body, which search does not return.
    pub async fn body(&self, object_id: &str) -> Result<String, SeriesError> {
        let object = self
            .client
            .object(&self.space_id, object_id)
            .get()
            .await
            .inspect_err(|err| {
                error!(object_id, error = %err, "anytype get event body failed");
            })?;
        Ok(crate::feed::etag_for(
            object.markdown.as_deref().unwrap_or_default(),
        ))
    }

    /// Adds occurrences to a series' `exdate`, keeping those it has.
    pub async fn exclude(
        &self,
        series: &Series,
        occurrences: &[String],
    ) -> Result<(), SeriesError> {
        let text = series
            .exdates
            .iter()
            .map(|excluded| excluded.raw.clone())
            .chain(occurrences.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n");
        self.client
            .update_object(&self.space_id, &series.id)
            .set_text("exdate", text)
            .update()
            .await
            .inspect_err(|err| {
                error!(series_id = %series.id, error = %err, "anytype update series exdate failed");
            })?;
        Ok(())
    }

    /// Archives a meeting, as deleting it in Anytype does.
    pub async fn archive(&self, object_id: &str) -> Result<(), SeriesError> {
        self.client
            .object(&self.space_id, object_id)
            .delete()
            .await
            .inspect_err(|err| {
                error!(object_id, error = %err, "anytype archive event failed");
            })?;
        Ok(())
    }

    /// Creates the task for one occurrence, copying what the series carries.
    async fn create_task(&self, planned: &Planned<'_>) -> Result<String, SeriesError> {
        let series = planned.series;
        let started = Instant::now();
        debug!(
            series_id = %series.id,
            series = %series.name,
            occurrence = %planned.occurrence,
            day = %planned.day,
            priority = ?series.priority,
            tags = ?series.tags,
            reminder_leads = ?series.reminder_leads,
            "anytype create task started"
        );
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
        let object = request.create().await.inspect_err(|err| {
            error!(
                series_id = %series.id,
                day = %planned.day,
                error = %err,
                elapsed_ms = started.elapsed().as_millis(),
                "anytype create task failed"
            );
        })?;
        debug!(
            series_id = %series.id,
            task_id = %object.id,
            elapsed_ms = started.elapsed().as_millis(),
            "anytype create task finished"
        );
        Ok(object.id)
    }
}

/// Runs the plan on a timer inside the service.
pub struct SeriesGenerator {
    source: AnytypeSeries,
    state: Arc<StateStore>,
    tz: Tz,
    poll_interval: Duration,
    /// How far ahead occurrences are made; zero makes the next one alone.
    horizon: chrono::Duration,
    /// Warnings already logged, so a series without a date does not repeat
    /// itself every five minutes.
    reported: Mutex<HashSet<String>>,
    passes: AtomicU64,
}

impl SeriesGenerator {
    pub fn new(
        source: AnytypeSeries,
        state: Arc<StateStore>,
        tz: Tz,
        poll_interval: Duration,
        horizon: chrono::Duration,
    ) -> Self {
        Self {
            source,
            state,
            tz,
            poll_interval,
            horizon,
            reported: Mutex::new(HashSet::new()),
            passes: AtomicU64::new(0),
        }
    }

    /// One pass. Returns how many tasks were created.
    ///
    /// Each task is claimed in the state database before it is created. The
    /// claim covers the gap in which a just-created task is not yet visible to
    /// search, and it keeps a task the user deleted from coming back. A failed
    /// create releases its claim so the next pass retries.
    pub async fn check_at(&self, now: DateTime<Utc>) -> Result<usize, SeriesError> {
        let pass = self.passes.fetch_add(1, Ordering::Relaxed) + 1;
        self.check_inner(now)
            .instrument(info_span!("generator_pass", pass))
            .await
    }

    async fn check_inner(&self, now: DateTime<Utc>) -> Result<usize, SeriesError> {
        let started = Instant::now();
        let today = now.with_timezone(&self.tz).date_naive();
        debug!(%now, %today, tz = %self.tz, "generator pass started");
        let series = self.source.series().await?;
        let instances = self.source.instances().await?;
        let until = today + self.horizon;
        let (planned, warnings) = plan_until(&series, &instances, self.tz, today, until);

        if let Ok(mut reported) = self.reported.lock() {
            for warning in &warnings {
                if reported.insert(warning.clone()) {
                    warn!(%warning, kind = ?self.source.kind(), "series skipped");
                } else {
                    debug!(%warning, kind = ?self.source.kind(), "series skipped (already reported)");
                }
            }
        }

        let planned_count = planned.len();
        let (mut created, mut already, mut failed) = (0usize, 0usize, 0usize);
        for one in planned {
            let claimed = self.state.claim_instance(&one.series.id, one.day).inspect_err(|err| {
                error!(series_id = %one.series.id, day = %one.day, error = %err, "cannot claim occurrence");
            })?;
            if !claimed {
                // Either created by an earlier pass and not yet visible to
                // search, or created and then deleted by hand.
                info!(
                    series_id = %one.series.id,
                    series = %one.series.name,
                    day = %one.day,
                    "occurrence already claimed; not creating it again"
                );
                already += 1;
                continue;
            }
            debug!(series_id = %one.series.id, day = %one.day, "occurrence claimed");
            match self.source.create(&one).await {
                Ok(id) => {
                    if self.source.kind() == Kind::Event {
                        self.record(&one);
                        self.record_body(&one.series.id, one.day, &id).await;
                    }
                    info!(
                        series_id = %one.series.id,
                        series = %one.series.name,
                        day = %one.day,
                        occurrence = %one.occurrence,
                        instance_id = %id,
                        kind = ?self.source.kind(),
                        "created an occurrence of a series"
                    );
                    created += 1;
                }
                Err(err) => {
                    error!(
                        series_id = %one.series.id,
                        series = %one.series.name,
                        day = %one.day,
                        error = %err,
                        kind = ?self.source.kind(),
                        "cannot create an occurrence; releasing the claim so the next pass retries"
                    );
                    failed += 1;
                    self.state
                        .release_instance(&one.series.id, one.day)
                        .inspect_err(|err| {
                            error!(series_id = %one.series.id, day = %one.day, error = %err, "cannot release claim; this occurrence will not be retried");
                        })?;
                }
            }
        }

        if self.source.kind() == Kind::Event {
            self.upkeep(&series, &instances, now).await?;
        }

        let summary_is_news = created + failed + already > 0;
        macro_rules! summary {
            ($level:ident) => {
                $level!(
                    %today,
                    series = series.len(),
                    instances = instances.len(),
                    warnings = warnings.len(),
                    planned = planned_count,
                    created,
                    already_claimed = already,
                    failed,
                    elapsed_ms = started.elapsed().as_millis(),
                    "generator pass finished"
                )
            };
        }
        if summary_is_news {
            summary!(info);
        } else {
            summary!(debug);
        }
        Ok(created)
    }

    /// Keeps a claim's fingerprint of the meeting just written for it. A
    /// failure only leaves that meeting out of the upkeep, so it is logged.
    fn record(&self, planned: &Planned<'_>) {
        let Ok(at) = DateTime::parse_from_rfc3339(&planned.occurrence) else {
            return;
        };
        let shape = Shape::of_meeting(planned.series, at.with_timezone(&Utc), planned.day);
        if let Err(err) =
            self.state
                .record_instance(&planned.series.id, planned.day, &shape.fingerprint())
        {
            warn!(series_id = %planned.series.id, day = %planned.day, error = %err, "cannot record the meeting's fingerprint");
        }
    }

    /// Keeps a claim's digest of its meeting's body as it is now. A failure
    /// only keeps that meeting from ever being archived, so it is logged.
    async fn record_body(&self, series_id: &str, day: NaiveDate, object_id: &str) {
        let recorded = match self.source.body(object_id).await {
            Ok(digest) => self
                .state
                .record_body(series_id, day, &digest)
                .map_err(|e| e.to_string()),
            Err(err) => Err(err.to_string()),
        };
        if let Err(err) = recorded {
            warn!(series_id, %day, object_id, error = %err, "cannot record the meeting's body");
        }
    }

    /// Brings the meetings ahead that nobody changed in line with their
    /// series. One failure is logged and does not stop the rest.
    async fn upkeep(
        &self,
        series: &[Series],
        instances: &[Instance],
        now: DateTime<Utc>,
    ) -> Result<(), SeriesError> {
        let claims = self.state.claimed_instances()?;
        let (mut updated, mut archived, mut kept, mut adopted, mut excluded, mut failed) =
            (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
        for one in plan_upkeep(series, instances, &claims, self.tz, now) {
            match one {
                Upkeep::Update { object_id, planned } => {
                    match self.source.update_event(&object_id, &planned).await {
                        Ok(()) => {
                            info!(object_id, series = %planned.series.name, day = %planned.day, occurrence = %planned.occurrence, "updated a meeting after its series changed");
                            self.record(&planned);
                            updated += 1;
                        }
                        Err(_) => failed += 1,
                    }
                }
                Upkeep::Archive {
                    object_id,
                    series_id,
                    day,
                    body,
                } => {
                    let now_body = match self.source.body(&object_id).await {
                        Ok(digest) => digest,
                        Err(_) => {
                            failed += 1;
                            continue;
                        }
                    };
                    if body.as_deref() != Some(now_body.as_str()) {
                        debug!(object_id, series_id, %day, recorded = body.is_some(), "kept a meeting its series no longer has: its body was written in or not recorded");
                        kept += 1;
                        continue;
                    }
                    match self.source.archive(&object_id).await {
                        Ok(()) => {
                            info!(object_id, series_id, %day, "archived a meeting its series no longer has");
                            // Should the series have the day again, the meeting is made anew.
                            self.state.release_instance(&series_id, day)?;
                            archived += 1;
                        }
                        Err(_) => failed += 1,
                    }
                }
                Upkeep::Adopt {
                    object_id,
                    series_id,
                    day,
                    fingerprint,
                } => {
                    self.state.record_instance(&series_id, day, &fingerprint)?;
                    self.record_body(&series_id, day, &object_id).await;
                    adopted += 1;
                }
                Upkeep::Exclude {
                    series,
                    occurrences,
                } => match self.source.exclude(series, &occurrences).await {
                    Ok(()) => {
                        info!(series_id = %series.id, series = %series.name, ?occurrences, "left deleted meetings out of their series");
                        excluded += occurrences.len();
                    }
                    Err(_) => failed += 1,
                },
            }
        }
        // A kept meeting is kept again on every pass, so it alone is no news.
        if updated + archived + adopted + excluded + failed > 0 {
            info!(
                updated,
                archived, kept, adopted, excluded, failed, "upkeep of meetings finished"
            );
        } else if kept > 0 {
            debug!(kept, "upkeep of meetings finished");
        }
        Ok(())
    }

    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(err) = self.check_at(Utc::now()).await {
                error!(error = %err, error_debug = ?err, "generator pass failed; retrying next interval");
            }
        }
    }
}

pub(crate) fn text(object: &Object, key: &str) -> Option<String> {
    match &object.get_property(key)?.value {
        PropertyValue::Text { text } => Some(text.clone()).filter(|t| !t.trim().is_empty()),
        _ => None,
    }
}

pub(crate) fn date(object: &Object, key: &str) -> Option<AnytypeDate> {
    match &object.get_property(key)?.value {
        PropertyValue::Date { date } => AnytypeDate::parse(date),
        _ => None,
    }
}

pub(crate) fn tag_ids(object: &Object, key: &str) -> Vec<String> {
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
            end: None,
            address: None,
            icon: None,
            extra: Vec::new(),
            exdates: Vec::new(),
        }
    }

    #[test]
    fn a_meeting_is_named_by_its_day_and_lasts_as_long_as_the_first() {
        // Thursdays at 18:00 Saratov (14:00Z), two hours long.
        let mut weekly = series(
            "Собрание Core Team ",
            "FREQ=WEEKLY;BYDAY=TH",
            "2026-09-17T14:00:00Z",
        );
        assert_eq!(
            event_name(&weekly, day(2026, 9, 24)),
            "Собрание Core Team 24.09.2026"
        );

        let next = Utc.with_ymd_and_hms(2026, 9, 24, 14, 0, 0).unwrap();
        assert_eq!(
            event_end(&weekly, next),
            None,
            "no end on the series, no end on the meeting"
        );
        weekly.end = Some(at("2026-09-17T16:00:00Z"));
        assert_eq!(
            event_end(&weekly, next).as_deref(),
            Some("2026-09-24T16:00:00Z")
        );
        // An end before the start says nothing about the length.
        weekly.end = Some(at("2026-09-17T13:00:00Z"));
        assert_eq!(event_end(&weekly, next), None);
    }

    /// Everything the event type also has is carried, dates moved with the
    /// meeting; what the generator sets itself, what Anytype keeps, what the
    /// type lacks and what is empty are not.
    #[test]
    fn an_event_carries_the_series_properties_with_dates_moved() {
        let mut weekly = series("s", "FREQ=WEEKLY;BYDAY=TH", "2026-09-17T14:00:00Z");
        let text = |text: &str| PropertyValue::Text { text: text.into() };
        weekly.extra = vec![
            // Tuesday 23:59 Saratov before the Thursday meeting.
            (
                "deadline".into(),
                PropertyValue::Date {
                    date: "2026-09-15T19:59:00Z".into(),
                },
            ),
            ("agenda".into(), text("Бюджет")),
            ("notes".into(), text(" ")),
            ("rrule".into(), text("FREQ=WEEKLY")),
            (
                "created_date".into(),
                PropertyValue::Date {
                    date: "2026-09-01T00:00:00Z".into(),
                },
            ),
            ("room".into(), text("не в типе")),
        ];
        let keys: HashSet<String> = ["deadline", "agenda", "notes", "rrule", "created_date"]
            .into_iter()
            .map(String::from)
            .collect();
        let next = Utc.with_ymd_and_hms(2026, 9, 24, 14, 0, 0).unwrap();
        let carried = carried(&weekly, &keys, next);
        let as_text: Vec<String> = carried.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
        assert_eq!(carried.len(), 2, "{as_text:?}");
        assert!(
            matches!(&carried[0], (k, PropertyValue::Date { date }) if k == "deadline" && date == "2026-09-22T19:59:00Z"),
            "{as_text:?}"
        );
        assert!(
            matches!(&carried[1], (k, PropertyValue::Text { text }) if k == "agenda" && text == "Бюджет"),
            "{as_text:?}"
        );
    }

    #[test]
    fn a_horizon_plans_every_occurrence_up_to_it_and_skips_those_made() {
        let weekly = series("s", "FREQ=WEEKLY;BYDAY=TH", "2026-09-17T14:00:00Z");
        let today = day(2026, 9, 18);
        let days = |planned: &[Planned]| planned.iter().map(|p| p.day).collect::<Vec<_>>();

        let (planned, _) = plan_until(
            std::slice::from_ref(&weekly),
            &[],
            Saratov,
            today,
            day(2026, 10, 18),
        );
        assert_eq!(
            days(&planned),
            [
                day(2026, 9, 24),
                day(2026, 10, 1),
                day(2026, 10, 8),
                day(2026, 10, 15)
            ]
        );

        let made = [
            instance("s", "2026-09-24T14:00:00Z"),
            instance("s", "2026-10-08T14:00:00Z"),
        ];
        let (planned, _) = plan_until(
            std::slice::from_ref(&weekly),
            &made,
            Saratov,
            today,
            day(2026, 10, 18),
        );
        assert_eq!(days(&planned), [day(2026, 10, 1), day(2026, 10, 15)]);

        // A horizon shorter than the gap still makes the next occurrence.
        let monthly = series("m", "FREQ=MONTHLY", "2026-09-17T14:00:00Z");
        let (planned, _) = plan_until(
            std::slice::from_ref(&monthly),
            &[],
            Saratov,
            today,
            day(2026, 9, 25),
        );
        assert_eq!(days(&planned), [day(2026, 10, 17)]);
    }

    #[test]
    fn an_image_icon_is_given_on_by_its_file_id() {
        let read = Icon::File {
            file: "http://127.0.0.1:31012/v1/spaces/space/files/bafyreifile".into(),
        };
        assert_eq!(
            reusable_icon(read),
            Icon::File {
                file: "bafyreifile".into()
            }
        );
        let emoji = Icon::Emoji {
            emoji: "🔥".into()
        };
        assert_eq!(reusable_icon(emoji.clone()), emoji);
    }

    #[test]
    fn events_are_planned_like_tasks() {
        let weekly = series("s", "FREQ=WEEKLY;BYDAY=TH", "2026-09-17T14:00:00Z");
        // On Friday 18.09 the next Thursday is missing and planned.
        let (planned, _) = plan(
            std::slice::from_ref(&weekly),
            &[],
            Saratov,
            day(2026, 9, 18),
        );
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].day, day(2026, 9, 24));
        assert_eq!(planned[0].occurrence, "2026-09-24T14:00:00Z");
        // A meeting already made for that Thursday, even at another hour, counts.
        let made = instance("s", "2026-09-24T15:30:00Z");
        let (planned, _) = plan(
            std::slice::from_ref(&weekly),
            &[made],
            Saratov,
            day(2026, 9, 18),
        );
        assert!(planned.is_empty());
    }

    fn instance(series_id: &str, occurrence: &str) -> Instance {
        Instance {
            object_id: format!("task-of-{series_id}"),
            series_id: series_id.into(),
            occurrence: Some(at(occurrence)),
            shape: Shape::default(),
        }
    }

    /// A meeting as `series` makes it for `occurrence`.
    fn meeting(series: &Series, occurrence: &str) -> Instance {
        let at = at(occurrence).parsed.with_timezone(&Utc);
        let day = at.with_timezone(&Saratov).date_naive();
        Instance {
            object_id: format!("meeting-{day}"),
            series_id: series.id.clone(),
            occurrence: Some(AnytypeDate::parse(occurrence).unwrap()),
            shape: Shape::of_meeting(series, at, day),
        }
    }

    fn claims(entries: &[(&Instance, Option<String>)]) -> HashMap<(String, NaiveDate), Claim> {
        entries
            .iter()
            .map(|(instance, recorded)| {
                let day = instance
                    .occurrence
                    .as_ref()
                    .unwrap()
                    .parsed
                    .with_timezone(&Saratov)
                    .date_naive();
                let claim = Claim {
                    fingerprint: recorded.clone(),
                    claimed_at: DateTime::UNIX_EPOCH,
                    body: None,
                };
                ((instance.series_id.clone(), day), claim)
            })
            .collect()
    }

    // Wednesday 2026-09-30, 12:00 Saratov; the daily meeting is at 15:00.
    const NOON: &str = "2026-09-30T08:00:00Z";
    const DAILY: &str = "2026-09-28T11:00:00Z";

    fn now() -> DateTime<Utc> {
        at(NOON).parsed.with_timezone(&Utc)
    }

    #[test]
    fn an_unchanged_meeting_follows_its_series_to_another_hour() {
        let before = series("daily", "FREQ=DAILY", DAILY);
        let made = meeting(&before, "2026-10-01T11:00:00Z");
        let recorded = claims(&[(&made, Some(made.shape.fingerprint()))]);
        // The series moves from 15:00 to 16:00 Saratov.
        let after = [series("daily", "FREQ=DAILY", "2026-09-28T12:00:00Z")];
        let upkeep = plan_upkeep(&after, &[made], &recorded, Saratov, now());
        let [Upkeep::Update { object_id, planned }] = upkeep.as_slice() else {
            panic!("{upkeep:?}");
        };
        assert_eq!(object_id, "meeting-2026-10-01");
        assert_eq!(planned.occurrence, "2026-10-01T12:00:00Z");
        assert_eq!(planned.day, day(2026, 10, 1));
    }

    #[test]
    fn a_meeting_changed_by_hand_is_left_alone() {
        let before = series("daily", "FREQ=DAILY", DAILY);
        let mut made = meeting(&before, "2026-10-01T11:00:00Z");
        let recorded = claims(&[(&made, Some(made.shape.fingerprint()))]);
        made.shape.address = Some("Room 4".into());
        let after = [series("daily", "FREQ=DAILY", "2026-09-28T12:00:00Z")];
        assert!(
            plan_upkeep(
                &after,
                std::slice::from_ref(&made),
                &recorded,
                Saratov,
                now()
            )
            .is_empty()
        );
        // Neither is it archived when the series is gone.
        assert!(plan_upkeep(&[], &[made], &recorded, Saratov, now()).is_empty());
    }

    #[test]
    fn a_day_the_series_no_longer_has_loses_its_meeting() {
        let before = series("daily", "FREQ=DAILY", DAILY);
        // Thursday 2026-10-01.
        let made = meeting(&before, "2026-10-01T11:00:00Z");
        let recorded = claims(&[(&made, Some(made.shape.fingerprint()))]);
        let weekdays = [series("daily", "FREQ=WEEKLY;BYDAY=MO,TU,WE", DAILY)];
        assert_eq!(
            plan_upkeep(
                &weekdays,
                std::slice::from_ref(&made),
                &recorded,
                Saratov,
                now()
            ),
            vec![Upkeep::Archive {
                object_id: "meeting-2026-10-01".into(),
                series_id: "daily".into(),
                day: day(2026, 10, 1),
                body: None,
            }]
        );
        // An archived series is not listed at all.
        assert_eq!(
            plan_upkeep(&[], &[made], &recorded, Saratov, now()).len(),
            1
        );
    }

    #[test]
    fn a_rule_that_does_not_parse_touches_nothing() {
        let before = series("daily", "FREQ=DAILY", DAILY);
        let made = meeting(&before, "2026-10-01T11:00:00Z");
        let recorded = claims(&[(&made, Some(made.shape.fingerprint()))]);
        let broken = [series("daily", "FREQ=DAILLY", DAILY)];
        assert!(plan_upkeep(&broken, &[made], &recorded, Saratov, now()).is_empty());
    }

    #[test]
    fn meetings_made_by_hand_or_already_begun_are_not_the_generators() {
        let daily = series("daily", "FREQ=DAILY", DAILY);
        let moved = [series("daily", "FREQ=DAILY", "2026-09-28T12:00:00Z")];
        // No claim: made by hand.
        let by_hand = meeting(&daily, "2026-10-01T11:00:00Z");
        assert!(plan_upkeep(&moved, &[by_hand], &HashMap::new(), Saratov, now()).is_empty());
        // Today's at 07:00 Saratov has begun by noon.
        let begun = meeting(&daily, "2026-09-30T03:00:00Z");
        let recorded = claims(&[(&begun, Some(begun.shape.fingerprint()))]);
        assert!(plan_upkeep(&moved, &[begun], &recorded, Saratov, now()).is_empty());
    }

    #[test]
    fn a_meeting_without_a_fingerprint_is_adopted_only_as_the_series_makes_it() {
        let daily = [series("daily", "FREQ=DAILY", DAILY)];
        let made = meeting(&daily[0], "2026-10-01T11:00:00Z");
        let recorded = claims(&[(&made, None)]);
        assert_eq!(
            plan_upkeep(
                &daily,
                std::slice::from_ref(&made),
                &recorded,
                Saratov,
                now()
            ),
            vec![Upkeep::Adopt {
                object_id: "meeting-2026-10-01".into(),
                series_id: "daily".into(),
                day: day(2026, 10, 1),
                fingerprint: made.shape.fingerprint(),
            }]
        );
        let mut changed = made;
        changed.shape.name = "Дейлик с Мишей".into();
        assert!(plan_upkeep(&daily, &[changed], &recorded, Saratov, now()).is_empty());
    }

    #[test]
    fn an_excluded_day_gets_no_meeting_and_loses_the_one_it_had() {
        let mut daily = series("daily", "FREQ=DAILY", DAILY);
        let made = meeting(&daily, "2026-10-01T11:00:00Z");
        daily.exdates = vec![at("2026-10-01T11:00:00Z")];
        let daily = [daily];
        let (planned, _) = plan_until(&daily, &[], Saratov, day(2026, 9, 30), day(2026, 10, 2));
        let days: Vec<NaiveDate> = planned.iter().map(|p| p.day).collect();
        assert_eq!(days, vec![day(2026, 9, 30), day(2026, 10, 2)]);

        let recorded = claims(&[(&made, Some(made.shape.fingerprint()))]);
        assert!(matches!(
            plan_upkeep(&daily, &[made], &recorded, Saratov, now()).as_slice(),
            [Upkeep::Archive { .. }]
        ));
    }

    #[test]
    fn a_deleted_meeting_joins_the_series_exdate() {
        let daily = [series("daily", "FREQ=DAILY", DAILY)];
        let gone_first = meeting(&daily[0], "2026-10-01T11:00:00Z");
        let gone_second = meeting(&daily[0], "2026-10-03T11:00:00Z");
        let kept = meeting(&daily[0], "2026-10-02T11:00:00Z");
        let mut recorded = claims(&[
            (&gone_second, None),
            (&gone_first, Some(gone_first.shape.fingerprint())),
            (&kept, Some(kept.shape.fingerprint())),
        ]);
        let upkeep = plan_upkeep(
            &daily,
            std::slice::from_ref(&kept),
            &recorded,
            Saratov,
            now(),
        );
        let [
            Upkeep::Exclude {
                series,
                occurrences,
            },
        ] = upkeep.as_slice()
        else {
            panic!("{upkeep:?}");
        };
        assert_eq!(series.id, "daily");
        assert_eq!(
            occurrences,
            &["2026-10-01T11:00:00Z", "2026-10-03T11:00:00Z"]
        );

        // A claim just made may stand for a meeting search does not show yet.
        for claim in recorded.values_mut() {
            claim.claimed_at = now() - chrono::TimeDelta::minutes(1);
        }
        assert!(
            plan_upkeep(
                &daily,
                std::slice::from_ref(&kept),
                &recorded,
                Saratov,
                now()
            )
            .is_empty()
        );
    }

    #[test]
    fn an_excluded_day_is_not_excluded_again() {
        let mut daily = series("daily", "FREQ=DAILY", DAILY);
        let gone = meeting(&daily, "2026-10-01T11:00:00Z");
        daily.exdates = vec![at("2026-10-01T11:00:00Z")];
        let recorded = claims(&[(&gone, None)]);
        assert!(plan_upkeep(&[daily], &[], &recorded, Saratov, now()).is_empty());
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

    /// A series made on the day of its first occurrence shows that day at
    /// once. On the day of an occurrence already made, the next one is still
    /// planned, with or without a horizon.
    #[test]
    fn todays_occurrence_is_planned_when_it_has_no_instance() {
        let guitar = [series("guitar", "FREQ=WEEKLY", GUITAR)];
        let days = |planned: Vec<Planned>| planned.iter().map(|p| p.day).collect::<Vec<_>>();
        let saturday = day(2026, 9, 19);

        let (planned, _) = plan(&guitar, &[], Saratov, saturday);
        assert_eq!(days(planned), [saturday, day(2026, 9, 26)]);

        let made = [instance("guitar", GUITAR)];
        let (planned, _) = plan(&guitar, &made, Saratov, saturday);
        assert_eq!(days(planned), [day(2026, 9, 26)]);

        let (planned, _) = plan_until(&guitar, &made, Saratov, saturday, day(2026, 10, 4));
        assert_eq!(days(planned), [day(2026, 9, 26), day(2026, 10, 3)]);
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
        let instances = [
            instance("guitar", GUITAR),
            instance("rent", "2026-10-12T20:00:00Z"),
        ];
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
        let instances = [
            instance("guitar", GUITAR),
            instance("guitar", "2026-09-26T15:30:00Z"),
        ];
        let (planned, _) = plan(&all, &instances, Saratov, day(2026, 9, 19));
        assert!(planned.is_empty());
    }

    #[test]
    fn an_instance_of_another_series_does_not_count() {
        let all = [series("guitar", "FREQ=WEEKLY", GUITAR)];
        let instances = [
            instance("guitar", GUITAR),
            instance("rent", "2026-09-26T08:00:00Z"),
        ];
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
        let made = [instance("guitar", GUITAR)];
        let (planned, warnings) = plan(&all, &made, Saratov, day(2026, 9, 19));
        assert_eq!(planned.len(), 1);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }
}
