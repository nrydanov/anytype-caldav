//! Events: objects of type `event` served as VEVENTs in the CalDAV `events/`
//! collection, and written back when a client edits them.
//!
//! Anytype stores a date-only value as local midnight; an all-day event's
//! `end_date` is its last day, while iCalendar's DTEND is exclusive, so the
//! renderer adds a day and write-back takes it away again. Reminders are
//! relative triggers before the start, which is what Calino reads and writes
//! for events.

use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

use anytype::{client::AnytypeClient, objects::Object, properties::SetProperty};
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use futures::StreamExt;
use icalendar::{
    Alarm, Calendar, CalendarDateTime, Component, DatePerhapsTime, EventLike, Property, Trigger,
};
use tracing::{debug, error, info, warn};

use crate::{
    config::CalendarConfig,
    feed::{Resource, calino_filename, etag_for},
    model::{AnytypeDate, CalendarValue},
    render::{sequence_for, stable_alarm},
    series::{date, text},
    source::SourceError,
    writeback::{Moment, moment, terminated, to_anytype},
};

pub const EVENT_TYPE: &str = "event";

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub object_id: String,
    pub name: String,
    pub start: Option<AnytypeDate>,
    /// For an all-day event, the last day it covers.
    pub end: Option<AnytypeDate>,
    pub location: Option<String>,
    pub tags: Vec<String>,
    /// Option names of `reminder_lead`, e.g. `15m`, kept as names so they can be
    /// written back without a lookup.
    pub reminder_names: Vec<String>,
    pub ical_uid: Option<String>,
    pub object_url: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
    /// RRULE value without the `RRULE:` prefix; set on the master of a series.
    pub rrule: Option<String>,
    /// Occurrences removed from the series, in Anytype's date shape (`exdate`,
    /// one value per line).
    pub exdates: Vec<AnytypeDate>,
    /// Set on an object that replaces one occurrence: the master's object id.
    pub series: Option<String>,
    /// The occurrence it replaces (its RECURRENCE-ID).
    pub occurrence: Option<AnytypeDate>,
    /// `deadline`, served as an entry of its own (`deadline_entry`).
    pub deadline: Option<AnytypeDate>,
}

/// What the name of a deadline's resource and its UID end with.
pub const DEADLINE_SUFFIX: &str = "-deadline";

/// The deadline of an event as a calendar entry of its own: `<name> (дедлайн)`
/// on the deadline, all day when it has no time, reminded with the event's own
/// leads, counted from the deadline. A series has one deadline for every
/// occurrence and none of them in particular, so it gets no entry.
pub fn deadline_entry(event: &Event) -> Option<Event> {
    if event.rrule.is_some() {
        return None;
    }
    Some(Event {
        object_id: event.object_id.clone(),
        name: format!("{} (дедлайн)", event.name),
        start: Some(event.deadline.clone()?),
        end: None,
        location: None,
        tags: event.tags.clone(),
        reminder_names: event.reminder_names.clone(),
        ical_uid: Some(format!("{}{DEADLINE_SUFFIX}", event.uid())),
        object_url: event.object_url.clone(),
        last_modified: event.last_modified,
        rrule: None,
        exdates: Vec::new(),
        series: None,
        occurrence: None,
        deadline: None,
    })
}

/// The deadline a client moved an entry from `deadline_entry` to: its start.
pub fn deadline_from(incoming: &IncomingEvent, config: &CalendarConfig) -> Option<String> {
    anytype_date(incoming.start.as_ref()?, config)
}

impl Event {
    pub fn uid(&self) -> String {
        self.ical_uid
            .clone()
            .unwrap_or_else(|| format!("{}@anytype-task-exporter", self.object_id))
    }

    pub fn resource_name(&self) -> String {
        match &self.ical_uid {
            Some(uid) => calino_filename(uid),
            None => self.object_id.clone(),
        }
    }

    /// Lead times parsed from the option names; unusable names are skipped.
    pub fn leads(&self) -> Vec<Duration> {
        self.reminder_names
            .iter()
            .filter_map(|name| humantime::parse_duration(name.trim()).ok())
            .filter_map(|lead| Duration::from_std(lead).ok())
            .collect()
    }
}

// ------------------------------------------------------------------ recurrence

/// Starts of a series' occurrences within `[from, to]`, in Anytype's date
/// shape so they classify like a stored start. The rule is expanded in local
/// time (`DTSTART;TZID=`), so a daylight-saving change keeps the wall clock.
/// Excluded dates and occurrences in `replaced` are left out; an event
/// without a rule yields its own start when it falls in the window.
pub fn occurrences_between(
    event: &Event,
    tz: Tz,
    replaced: &[DateTime<Utc>],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<AnytypeDate>, String> {
    let Some(start) = &event.start else {
        return Ok(Vec::new());
    };
    let Some(rule) = &event.rrule else {
        let at = start.parsed.with_timezone(&Utc);
        return Ok((from <= at && at <= to)
            .then(|| start.clone())
            .into_iter()
            .collect());
    };
    let (local, all_day) = match start.classify(tz) {
        CalendarValue::AllDay(day) => (day.and_time(NaiveTime::MIN), true),
        CalendarValue::Instant(at) => (at.with_timezone(&tz).naive_local(), false),
    };
    let text = format!(
        "DTSTART;TZID={}:{}\nRRULE:{}",
        tz.name(),
        local.format("%Y%m%dT%H%M%S"),
        rule.trim()
    );
    let set = rrule::RRuleSet::from_str(&text).map_err(|err| format!("{rule:?}: {err}"))?;
    let skipped: Vec<DateTime<Utc>> = event
        .exdates
        .iter()
        .map(|d| d.parsed.with_timezone(&Utc))
        .chain(replaced.iter().copied())
        .collect();
    let found = set
        .after(from.with_timezone(&rrule::Tz::UTC))
        .before(to.with_timezone(&rrule::Tz::UTC))
        .all(1000);
    Ok(found
        .dates
        .into_iter()
        .map(|occurrence| {
            if all_day {
                let day = occurrence.with_timezone(&tz).date_naive();
                tz.from_local_datetime(&day.and_time(NaiveTime::MIN))
                    .earliest()
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or_else(|| occurrence.with_timezone(&Utc))
            } else {
                occurrence.with_timezone(&Utc)
            }
        })
        .filter(|at| !skipped.contains(at))
        .filter_map(|at| AnytypeDate::parse(&at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)))
        .collect())
}

// ------------------------------------------------------------------ rendering

/// A value on the wire. `zone` writes an instant as local wall-clock time with
/// `TZID`, which a recurring rule needs to expand across daylight-saving
/// changes; otherwise instants are UTC.
fn wire_value(value: CalendarValue, zone: Option<Tz>) -> DatePerhapsTime {
    match (value, zone) {
        (CalendarValue::AllDay(day), _) => DatePerhapsTime::Date(day),
        (CalendarValue::Instant(at), None) => DatePerhapsTime::DateTime(CalendarDateTime::Utc(at)),
        (CalendarValue::Instant(at), Some(tz)) => {
            DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone {
                date_time: at.with_timezone(&tz).naive_local(),
                tzid: tz.name().to_string(),
            })
        }
    }
}

/// `DATE`, `TZID=…:local` or UTC text of a value, for properties the library
/// has no typed setter for (EXDATE, RECURRENCE-ID).
fn date_property(key: &str, value: CalendarValue, zone: Option<Tz>) -> Property {
    let mut property = match wire_value(value, zone) {
        DatePerhapsTime::Date(day) => {
            let mut p = Property::new(key, day.format("%Y%m%d").to_string());
            p.add_parameter("VALUE", "DATE");
            p
        }
        DatePerhapsTime::DateTime(CalendarDateTime::WithTimezone { date_time, tzid }) => {
            let mut p = Property::new(key, date_time.format("%Y%m%dT%H%M%S").to_string());
            p.add_parameter("TZID", &tzid);
            p
        }
        DatePerhapsTime::DateTime(CalendarDateTime::Utc(at)) => {
            Property::new(key, at.format("%Y%m%dT%H%M%SZ").to_string())
        }
        DatePerhapsTime::DateTime(CalendarDateTime::Floating(local)) => {
            Property::new(key, local.format("%Y%m%dT%H%M%S").to_string())
        }
    };
    property.done()
}

/// DTSTART and DTEND as a client sees them. `None` without a start: a VEVENT
/// without DTSTART is not a calendar entry.
pub fn wire_dates(
    event: &Event,
    config: &CalendarConfig,
) -> Option<(DatePerhapsTime, Option<DatePerhapsTime>)> {
    zoned_dates(event, config, None)
}

fn zoned_dates(
    event: &Event,
    config: &CalendarConfig,
    zone: Option<Tz>,
) -> Option<(DatePerhapsTime, Option<DatePerhapsTime>)> {
    let tz = config.date_only_timezone;
    let start = event.start.as_ref()?.classify(tz);
    let end = event.end.as_ref().map(|end| end.classify(tz));
    let dtend = match (start, end) {
        // Last day inclusive in Anytype, exclusive on the wire.
        (_, Some(CalendarValue::AllDay(last))) => {
            Some(DatePerhapsTime::Date(last + Duration::days(1)))
        }
        (_, Some(end @ CalendarValue::Instant(_))) => Some(wire_value(end, zone)),
        (CalendarValue::AllDay(day), None) => Some(DatePerhapsTime::Date(day + Duration::days(1))),
        (CalendarValue::Instant(_), None) => None,
    };
    Some((wire_value(start, zone), dtend))
}

/// One VEVENT. `uid` is the series' UID for a replaced occurrence.
fn vevent_for(
    event: &Event,
    uid: &str,
    config: &CalendarConfig,
    fallback: DateTime<Utc>,
    zone: Option<Tz>,
) -> Option<icalendar::Event> {
    let (start, end) = zoned_dates(event, config, zone)?;
    let modified = event.last_modified.unwrap_or(fallback);
    let mut vevent = icalendar::Event::new();
    vevent
        .uid(uid)
        .summary(&event.name)
        .timestamp(modified)
        .sequence(sequence_for(modified))
        .starts(start);
    if let Some(end) = end {
        vevent.ends(end);
    }
    if let Some(location) = &event.location {
        vevent.location(location);
    }
    if let Some(url) = &event.object_url {
        vevent.url(url);
        // Also where a client shows it: see `render_task`.
        vevent.description(&crate::render::app_link(url));
    }
    let mut vevent = vevent.done();
    for tag in &event.tags {
        vevent.add_multi_property("CATEGORIES", tag);
    }
    // Calino reads only `-PT…` durations (`parseTriggerDuration` in
    // `icalTypeMapping.ts`), so days and weeks are written as minutes.
    let alarm_owner = format!("{uid}-{}", event.object_id);
    for (index, lead) in event.leads().into_iter().enumerate() {
        let mut alarm = Alarm::display(&event.name, Trigger::before_start(lead));
        alarm.append_property(Property::new(
            "TRIGGER",
            format!("-PT{}M", lead.num_minutes()),
        ));
        vevent.alarm(stable_alarm(alarm.done(), &alarm_owner, index, modified));
    }
    Some(vevent)
}

/// A standalone event, or a series: the master with RRULE and EXDATE, then
/// one VEVENT with RECURRENCE-ID per replaced occurrence, all in one resource
/// as RFC 4791 §4.1 requires for one UID.
pub fn render_series(
    master: &Event,
    replacements: &[&Event],
    config: &CalendarConfig,
    fallback: DateTime<Utc>,
) -> Option<String> {
    let uid = master.uid();
    let recurring = master.rrule.is_some();
    let zone = recurring.then_some(config.timezone);
    let mut first = vevent_for(master, &uid, config, fallback, zone)?;
    let mut components = Vec::new();
    if let Some(rule) = &master.rrule {
        first.append_property(Property::new("RRULE", rule.trim()));
        let tz = config.date_only_timezone;
        for excluded in &master.exdates {
            first.append_multi_property(date_property("EXDATE", excluded.classify(tz), zone));
        }
        let mut replacements: Vec<&&Event> = replacements.iter().collect();
        replacements.sort_by_key(|r| r.occurrence.as_ref().map(|o| o.parsed));
        for replacement in replacements {
            let Some(occurrence) = &replacement.occurrence else {
                continue;
            };
            let Some(mut vevent) = vevent_for(replacement, &uid, config, fallback, zone) else {
                continue;
            };
            vevent.append_property(date_property(
                "RECURRENCE-ID",
                occurrence.classify(tz),
                zone,
            ));
            components.push(vevent);
        }
    }
    let mut calendar = Calendar::new();
    calendar
        .name(&config.name)
        .timezone(config.timezone.name())
        .push(first);
    for component in components {
        calendar.push(component);
    }
    Some(calendar.to_string())
}

pub fn render_event(
    event: &Event,
    config: &CalendarConfig,
    fallback: DateTime<Utc>,
) -> Option<String> {
    render_series(event, &[], config, fallback)
}

pub fn resource_for(
    master: &Event,
    replacements: &[&Event],
    config: &CalendarConfig,
    fallback: DateTime<Utc>,
) -> Option<Resource> {
    let ics = render_series(master, replacements, config, fallback)?;
    Some(Resource {
        object_id: master.object_id.clone(),
        etag: etag_for(&ics),
        ics: Arc::from(ics.as_str()),
    })
}

// ------------------------------------------------------------------ write-back

#[derive(Debug, Clone, PartialEq)]
pub struct IncomingEvent {
    pub uid: Option<String>,
    pub summary: Option<String>,
    pub start: Option<DatePerhapsTime>,
    pub end: Option<DatePerhapsTime>,
    pub location: Option<String>,
    /// Minutes before the start, from relative VALARM triggers.
    pub leads: Vec<Duration>,
    /// CATEGORIES values.
    pub categories: Vec<String>,
    /// RRULE value, on the master of a series.
    pub rrule: Option<String>,
    /// EXDATE values, one per date even when the client joined them.
    pub exdates: Vec<DatePerhapsTime>,
    /// RECURRENCE-ID, on a component replacing one occurrence.
    pub recurrence_id: Option<DatePerhapsTime>,
}

/// A PUT body: the master (or a plain event) and the components replacing
/// single occurrences.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingSeries {
    pub master: IncomingEvent,
    pub replacements: Vec<IncomingEvent>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventPatch {
    pub name: Option<String>,
    pub start: Option<Option<String>>,
    pub end: Option<Option<String>>,
    pub location: Option<Option<String>>,
    /// Option names for `reminder_lead`.
    pub reminders: Option<Vec<String>>,
    /// Option names for `tag`; an empty list clears the tags.
    pub tags: Option<Vec<String>>,
    /// `Some(None)` removes the recurrence.
    pub rrule: Option<Option<String>>,
    /// Anytype-shaped dates; an empty list clears `exdate`.
    pub exdates: Option<Vec<String>>,
    /// Only on create: the master this object replaces an occurrence of.
    pub series: Option<String>,
    /// Only on create, beside `series`.
    pub occurrence: Option<String>,
    /// `deadline`; `Some(None)` clears it.
    pub deadline: Option<Option<String>>,
}

impl EventPatch {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.start.is_none()
            && self.end.is_none()
            && self.location.is_none()
            && self.reminders.is_none()
            && self.tags.is_none()
            && self.rrule.is_none()
            && self.exdates.is_none()
            && self.series.is_none()
            && self.occurrence.is_none()
            && self.deadline.is_none()
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EventWriteError {
    #[error("body is not an iCalendar document: {0}")]
    NotCalendar(String),
    #[error("body carries no VEVENT")]
    NoEvent,
    #[error("body carries {0} VEVENTs without RECURRENCE-ID; one resource holds one event")]
    SeveralMasters(usize),
    #[error("body carries only replaced occurrences; the series itself is required")]
    NoMaster,
    #[error("RDATE is not supported")]
    Rdate,
}

fn incoming(event: &icalendar::Event) -> IncomingEvent {
    let mut leads: Vec<Duration> = event
        .components()
        .iter()
        .filter(|c| c.component_kind() == "VALARM")
        .filter_map(|alarm| alarm.property_value("TRIGGER"))
        .filter_map(parse_before)
        .collect();
    leads.sort();
    leads.dedup();
    let exdates = event
        .multi_properties()
        .get("EXDATE")
        .into_iter()
        .flatten()
        .flat_map(|property| {
            // Parsing accepts comma lists (RFC 5545 §3.8.5.1); each value
            // keeps the property's VALUE and TZID parameters.
            property.value().split(',').filter_map(move |value| {
                let mut single = Property::new("EXDATE", value.trim());
                for parameter in property.params().values() {
                    single.append_parameter(parameter.clone());
                }
                DatePerhapsTime::from_property(&single.done())
            })
        })
        .collect();
    IncomingEvent {
        uid: event.get_uid().map(str::to_string),
        summary: event
            .get_summary()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        start: event.get_start(),
        end: event.get_end(),
        location: event
            .get_location()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        leads,
        categories: categories(event),
        rrule: event
            .property_value("RRULE")
            .map(|rule| rule.trim().to_string())
            .filter(|rule| !rule.is_empty()),
        exdates,
        recurrence_id: event.get_recurrence_id(),
    }
}

pub fn parse_series(body: &str) -> Result<IncomingSeries, EventWriteError> {
    let calendar: Calendar = terminated(body)
        .parse()
        .map_err(EventWriteError::NotCalendar)?;
    let events: Vec<&icalendar::Event> = calendar.events().collect();
    if events.is_empty() {
        return Err(EventWriteError::NoEvent);
    }
    if events.iter().any(|e| e.property_value("RDATE").is_some()) {
        return Err(EventWriteError::Rdate);
    }
    let (masters, replacements): (Vec<IncomingEvent>, Vec<IncomingEvent>) = events
        .into_iter()
        .map(incoming)
        .partition(|e| e.recurrence_id.is_none());
    let mut masters = masters.into_iter();
    match (masters.next(), masters.len()) {
        (None, _) => Err(EventWriteError::NoMaster),
        (Some(master), 0) => Ok(IncomingSeries {
            // A replaced occurrence of a series without a rule means nothing.
            replacements: if master.rrule.is_some() {
                replacements
            } else {
                Vec::new()
            },
            master,
        }),
        (Some(_), more) => Err(EventWriteError::SeveralMasters(more + 1)),
    }
}

/// The master of a body; kept for callers that handle plain events only.
pub fn parse_event(body: &str) -> Result<IncomingEvent, EventWriteError> {
    parse_series(body).map(|series| series.master)
}

/// `-PT15M`, `-P1D`, `-PT1H30M`, `-P1W`, `-PT0M` → the lead before the start.
/// Absolute and after-start triggers are not leads and are ignored.
fn parse_before(trigger: &str) -> Option<Duration> {
    let rest = trigger.trim().strip_prefix('-').or_else(|| {
        // "-PT0M" is at-start; "PT0M" written without a sign means the same.
        (trigger.trim() == "PT0M" || trigger.trim() == "P0D").then_some(trigger.trim())
    })?;
    let rest = rest.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = Duration::zero();
    let mut number = String::new();
    for (part, units) in [
        (date, [('W', 604_800i64), ('D', 86_400), ('?', 0)]),
        (time, [('H', 3_600), ('M', 60), ('S', 1)]),
    ] {
        for c in part.chars() {
            if c.is_ascii_digit() {
                number.push(c);
                continue;
            }
            let seconds = units.iter().find(|(unit, _)| *unit == c)?.1;
            total += Duration::seconds(number.parse::<i64>().ok()? * seconds);
            number.clear();
        }
    }
    number.is_empty().then_some(total)
}

/// The option name for a lead: the largest whole unit, as `reminder_lead`
/// options are written (`15m`, `2h`, `1d`, `1w`).
pub fn lead_name(lead: Duration) -> String {
    let minutes = lead.num_minutes();
    if minutes == 0 {
        "0m".into()
    } else if minutes % (60 * 24 * 7) == 0 {
        format!("{}w", minutes / (60 * 24 * 7))
    } else if minutes % (60 * 24) == 0 {
        format!("{}d", minutes / (60 * 24))
    } else if minutes % 60 == 0 {
        format!("{}h", minutes / 60)
    } else {
        format!("{minutes}m")
    }
}

fn anytype_date(value: &DatePerhapsTime, config: &CalendarConfig) -> Option<String> {
    to_anytype(moment(value, config.timezone)?, config.date_only_timezone)
}

/// DTEND back to Anytype's `end_date`: an exclusive all-day end becomes the
/// last day; an end that adds nothing to the start is no end at all.
fn anytype_end(
    start: Option<&DatePerhapsTime>,
    end: Option<&DatePerhapsTime>,
    config: &CalendarConfig,
) -> Option<String> {
    let end = moment(end?, config.timezone)?;
    let start = start.and_then(|s| moment(s, config.timezone));
    match end {
        Moment::Day(day) => {
            let last = day - Duration::days(1);
            match start {
                Some(Moment::Day(first)) if last <= first => None,
                _ => to_anytype(Moment::Day(last), config.date_only_timezone),
            }
        }
        Moment::At(_) if start == Some(end) => None,
        Moment::At(_) => to_anytype(end, config.date_only_timezone),
    }
}

pub fn event_patch_for_update(
    current: &Event,
    incoming: &IncomingEvent,
    config: &CalendarConfig,
) -> EventPatch {
    let mut patch = EventPatch::default();
    if let Some(name) = &incoming.summary
        && name != &current.name
    {
        patch.name = Some(name.clone());
    }
    let tz = config.timezone;
    let (now_start, now_end) = match wire_dates(current, config) {
        Some((s, e)) => (Some(s), e),
        None => (None, None),
    };
    let same = |a: Option<&DatePerhapsTime>, b: Option<&DatePerhapsTime>| {
        a.and_then(|v| moment(v, tz)) == b.and_then(|v| moment(v, tz))
    };
    if !same(now_start.as_ref(), incoming.start.as_ref()) {
        patch.start = Some(
            incoming
                .start
                .as_ref()
                .and_then(|s| anytype_date(s, config)),
        );
    }
    if !same(now_end.as_ref(), incoming.end.as_ref()) || patch.start.is_some() {
        let end = anytype_end(incoming.start.as_ref(), incoming.end.as_ref(), config);
        let current_end = current.end.as_ref().map(|e| e.raw.clone());
        if end != current_end {
            patch.end = Some(end);
        }
    }
    if incoming.location != current.location {
        patch.location = Some(incoming.location.clone());
    }
    let mut names: Vec<String> = incoming.leads.iter().map(|l| lead_name(*l)).collect();
    names.sort();
    let mut current_names: Vec<String> = current.leads().iter().map(|l| lead_name(*l)).collect();
    current_names.sort();
    if names != current_names {
        patch.reminders = Some(names);
    }
    patch.tags = tags_patch(&current.tags, &incoming.categories);
    patch
}

pub fn event_patch_for_create(incoming: &IncomingEvent, config: &CalendarConfig) -> EventPatch {
    EventPatch {
        tags: (!incoming.categories.is_empty()).then(|| incoming.categories.clone()),
        rrule: None,
        exdates: None,
        series: None,
        occurrence: None,
        deadline: None,
        name: Some(
            incoming
                .summary
                .clone()
                .unwrap_or_else(|| "(unnamed)".into()),
        ),
        start: Some(
            incoming
                .start
                .as_ref()
                .and_then(|s| anytype_date(s, config)),
        ),
        end: Some(anytype_end(
            incoming.start.as_ref(),
            incoming.end.as_ref(),
            config,
        )),
        location: incoming.location.clone().map(Some),
        reminders: (!incoming.leads.is_empty())
            .then(|| incoming.leads.iter().map(|l| lead_name(*l)).collect()),
    }
}

/// What a PUT of a series changes in Anytype.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeriesPlan {
    pub master: EventPatch,
    /// Existing replacement objects and their patches.
    pub update: Vec<(String, EventPatch)>,
    /// New replacement objects; each patch carries `series` and `occurrence`.
    pub create: Vec<EventPatch>,
    /// Replacement objects the body no longer has.
    pub archive: Vec<String>,
}

impl SeriesPlan {
    pub fn is_empty(&self) -> bool {
        self.master.is_empty()
            && self.update.is_empty()
            && self.create.is_empty()
            && self.archive.is_empty()
    }
}

/// An instant for a wire date, as stored in Anytype: the key replacements and
/// exclusions are matched by, whatever form (TZID, UTC, floating) carried it.
fn instant_of(value: &DatePerhapsTime, config: &CalendarConfig) -> Option<(String, DateTime<Utc>)> {
    let stored = anytype_date(value, config)?;
    let at = AnytypeDate::parse(&stored)?.parsed.with_timezone(&Utc);
    Some((stored, at))
}

/// RRULE parts compare as a set: clients reorder them.
fn same_rule(a: Option<&str>, b: Option<&str>) -> bool {
    let parts = |rule: Option<&str>| {
        let mut parts: Vec<String> = rule
            .unwrap_or_default()
            .split(';')
            .map(|p| p.trim().to_ascii_uppercase())
            .filter(|p| !p.is_empty())
            .collect();
        parts.sort();
        parts
    };
    parts(a) == parts(b)
}

fn exdates_patch(
    current: &[AnytypeDate],
    incoming: &[DatePerhapsTime],
    config: &CalendarConfig,
) -> Option<Vec<String>> {
    let mut wanted: Vec<(String, DateTime<Utc>)> = incoming
        .iter()
        .filter_map(|value| instant_of(value, config))
        .collect();
    wanted.sort_by_key(|(_, at)| *at);
    wanted.dedup_by_key(|(_, at)| *at);
    let mut have: Vec<DateTime<Utc>> = current
        .iter()
        .map(|d| d.parsed.with_timezone(&Utc))
        .collect();
    have.sort();
    have.dedup();
    let wanted_at: Vec<DateTime<Utc>> = wanted.iter().map(|(_, at)| *at).collect();
    (wanted_at != have).then(|| wanted.into_iter().map(|(stored, _)| stored).collect())
}

pub fn plan_series_update(
    current: &Event,
    replacements: &[Event],
    incoming: &IncomingSeries,
    config: &CalendarConfig,
) -> SeriesPlan {
    let mut master = event_patch_for_update(current, &incoming.master, config);
    if !same_rule(current.rrule.as_deref(), incoming.master.rrule.as_deref()) {
        master.rrule = Some(incoming.master.rrule.clone());
    }
    master.exdates = exdates_patch(&current.exdates, &incoming.master.exdates, config);

    let mut plan = SeriesPlan {
        master,
        ..SeriesPlan::default()
    };
    let mut matched: Vec<&str> = Vec::new();
    for wanted in &incoming.replacements {
        let Some((stored, at)) = wanted
            .recurrence_id
            .as_ref()
            .and_then(|r| instant_of(r, config))
        else {
            continue;
        };
        let existing = replacements.iter().find(|r| {
            r.occurrence.as_ref().map(|o| o.parsed.with_timezone(&Utc)) == Some(at)
                && !matched.contains(&r.object_id.as_str())
        });
        match existing {
            Some(existing) => {
                matched.push(&existing.object_id);
                let patch = event_patch_for_update(existing, wanted, config);
                if !patch.is_empty() {
                    plan.update.push((existing.object_id.clone(), patch));
                }
            }
            None => {
                let mut patch = event_patch_for_create(wanted, config);
                patch.series = Some(current.object_id.clone());
                patch.occurrence = Some(stored);
                plan.create.push(patch);
            }
        }
    }
    plan.archive = replacements
        .iter()
        .filter(|r| !matched.contains(&r.object_id.as_str()))
        .map(|r| r.object_id.clone())
        .collect();
    plan
}

/// A new series: the master's patch and the replacements created after it.
pub fn plan_series_create(
    incoming: &IncomingSeries,
    config: &CalendarConfig,
) -> (EventPatch, Vec<EventPatch>) {
    let mut master = event_patch_for_create(&incoming.master, config);
    master.rrule = incoming.master.rrule.clone().map(Some);
    let exdates = exdates_patch(&[], &incoming.master.exdates, config);
    master.exdates = exdates;
    let replacements = incoming
        .replacements
        .iter()
        .filter_map(|wanted| {
            let (stored, _) = wanted
                .recurrence_id
                .as_ref()
                .and_then(|r| instant_of(r, config))?;
            let mut patch = event_patch_for_create(wanted, config);
            patch.occurrence = Some(stored);
            Some(patch)
        })
        .collect();
    (master, replacements)
}

// ------------------------------------------------------------------ Anytype

/// Where events are read from and written to; Anytype in production, memory
/// in tests.
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    async fn list(&self) -> Result<Vec<Event>, SourceError>;
    async fn get(&self, object_id: &str) -> Result<Option<Event>, SourceError>;
    async fn create(&self, uid: &str, patch: &EventPatch) -> Result<String, SourceError>;
    /// An object replacing one occurrence: no `ical_uid`, since it is served
    /// inside its series' resource under the series' UID.
    async fn create_replacement(&self, patch: &EventPatch) -> Result<String, SourceError>;
    async fn update(&self, object_id: &str, patch: &EventPatch) -> Result<(), SourceError>;
    async fn archive(&self, object_id: &str) -> Result<(), SourceError>;
}

pub struct AnytypeEvents {
    client: AnytypeClient,
    space_id: String,
}

fn to_event(object: &Object) -> Event {
    Event {
        object_id: object.id.clone(),
        name: object
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "(unnamed)".into()),
        start: date(object, "start_date"),
        end: date(object, "end_date"),
        location: text(object, "address"),
        tags: object
            .get_property_multi_select("tag")
            .map(|tags| tags.iter().map(|t| t.name.trim().to_string()).collect())
            .unwrap_or_default(),
        reminder_names: object
            .get_property_multi_select("reminder_lead")
            .map(|tags| tags.iter().map(|t| t.name.trim().to_string()).collect())
            .unwrap_or_default(),
        ical_uid: text(object, "ical_uid"),
        object_url: Some(object.get_link()),
        last_modified: object
            .get_property_date("last_modified_date")
            .map(|d| d.with_timezone(&Utc)),
        rrule: text(object, "rrule")
            .map(|rule| rule.trim().trim_start_matches("RRULE:").to_string()),
        exdates: text(object, "exdate")
            .map(|values| values.lines().filter_map(AnytypeDate::parse).collect())
            .unwrap_or_default(),
        series: object
            .get_property_array("series")
            .and_then(|links| links.into_iter().next()),
        occurrence: date(object, "occurrence"),
        deadline: date(object, "deadline"),
    }
}

fn transport(err: impl std::fmt::Display) -> SourceError {
    SourceError::Transport(err.to_string())
}

impl AnytypeEvents {
    pub fn new(client: AnytypeClient, space_id: String) -> Self {
        Self { client, space_id }
    }
    async fn properties_for(
        &self,
        patch: &EventPatch,
    ) -> Result<Vec<serde_json::Value>, SourceError> {
        let mut out = Vec::new();
        for (key, value) in [
            ("start_date", &patch.start),
            ("end_date", &patch.end),
            ("deadline", &patch.deadline),
        ] {
            if let Some(value) = value {
                out.push(serde_json::json!({ "key": key, "date": value }));
            }
        }
        if let Some(location) = &patch.location {
            out.push(serde_json::json!({ "key": "address", "text": location.clone().unwrap_or_default() }));
        }
        if let Some(names) = &patch.reminders {
            let ids = option_ids(&self.client, &self.space_id, "reminder_lead", names).await?;
            out.push(serde_json::json!({ "key": "reminder_lead", "multi_select": ids }));
        }
        if let Some(tags) = &patch.tags {
            let ids = option_ids(&self.client, &self.space_id, "tag", tags).await?;
            out.push(serde_json::json!({ "key": "tag", "multi_select": ids }));
        }
        if let Some(rule) = &patch.rrule {
            out.push(
                serde_json::json!({ "key": "rrule", "text": rule.clone().unwrap_or_default() }),
            );
        }
        if let Some(exdates) = &patch.exdates {
            out.push(serde_json::json!({ "key": "exdate", "text": exdates.join("\n") }));
        }
        if let Some(series) = &patch.series {
            out.push(serde_json::json!({ "key": "series", "objects": [series] }));
        }
        if let Some(occurrence) = &patch.occurrence {
            out.push(serde_json::json!({ "key": "occurrence", "date": occurrence }));
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl EventStore for AnytypeEvents {
    async fn list(&self) -> Result<Vec<Event>, SourceError> {
        let started = Instant::now();
        let paged = self
            .client
            .search_in(&self.space_id)
            .types([EVENT_TYPE])
            .execute()
            .await
            .map_err(transport)?;
        let mut stream = paged.into_stream();
        let mut events = Vec::new();
        while let Some(object) = stream.next().await {
            let object = object.map_err(transport)?;
            if !object.archived {
                events.push(to_event(&object));
            }
        }
        debug!(
            events = events.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "anytype event listing finished"
        );
        Ok(events)
    }

    async fn get(&self, object_id: &str) -> Result<Option<Event>, SourceError> {
        match self.client.object(&self.space_id, object_id).get().await {
            Ok(object) if !object.archived => Ok(Some(to_event(&object))),
            Ok(_) => Ok(None),
            Err(anytype::error::AnytypeError::NotFound { .. }) => Ok(None),
            Err(err) => {
                error!(object_id, error = %err, "anytype get event failed");
                Err(transport(err))
            }
        }
    }

    async fn update(&self, object_id: &str, patch: &EventPatch) -> Result<(), SourceError> {
        let mut request = self.client.update_object(&self.space_id, object_id);
        if let Some(name) = &patch.name {
            request = request.name(name.clone());
        }
        for property in self.properties_for(patch).await? {
            request = request.add_property(property);
        }
        info!(object_id, ?patch, "anytype update event");
        request.update().await.map_err(|err| {
            error!(object_id, ?patch, error = %err, "anytype update event failed");
            transport(err)
        })?;
        Ok(())
    }

    async fn create(&self, uid: &str, patch: &EventPatch) -> Result<String, SourceError> {
        let mut request = self
            .client
            .new_object(&self.space_id, EVENT_TYPE)
            .name(patch.name.clone().unwrap_or_else(|| "(unnamed)".into()))
            .set_text("ical_uid", uid);
        for property in self.properties_for(patch).await? {
            request = request.add_property(property);
        }
        info!(uid, ?patch, "anytype create event");
        let object = request.create().await.map_err(|err| {
            error!(uid, ?patch, error = %err, "anytype create event failed");
            transport(err)
        })?;
        Ok(object.id)
    }

    async fn create_replacement(&self, patch: &EventPatch) -> Result<String, SourceError> {
        let mut request = self
            .client
            .new_object(&self.space_id, EVENT_TYPE)
            .name(patch.name.clone().unwrap_or_else(|| "(unnamed)".into()));
        for property in self.properties_for(patch).await? {
            request = request.add_property(property);
        }
        info!(?patch, "anytype create replaced occurrence");
        let object = request.create().await.map_err(|err| {
            error!(?patch, error = %err, "anytype create replaced occurrence failed");
            transport(err)
        })?;
        Ok(object.id)
    }

    async fn archive(&self, object_id: &str) -> Result<(), SourceError> {
        info!(object_id, "anytype archive event");
        self.client
            .object(&self.space_id, object_id)
            .delete()
            .await
            .map_err(transport)?;
        Ok(())
    }
}

// ------------------------------------------------------------------ service

pub struct EventSnapshot {
    pub objects: BTreeMap<String, Resource>,
    pub ctag: String,
}

/// Events for the CalDAV collection, cached for `ttl` and dropped after writes.
pub struct EventService {
    pub source: Arc<dyn EventStore>,
    pub config: CalendarConfig,
    fallback: DateTime<Utc>,
    ttl: StdDuration,
    cache: Mutex<Option<(Instant, Arc<EventSnapshot>)>>,
}

impl EventService {
    pub fn new(
        source: Arc<dyn EventStore>,
        config: CalendarConfig,
        fallback: DateTime<Utc>,
        ttl: StdDuration,
    ) -> Self {
        Self {
            source,
            config,
            fallback,
            ttl,
            cache: Mutex::new(None),
        }
    }

    pub async fn snapshot(&self) -> Result<Arc<EventSnapshot>, SourceError> {
        if let Ok(cache) = self.cache.lock()
            && let Some((at, snapshot)) = cache.as_ref()
            && at.elapsed() < self.ttl
        {
            return Ok(snapshot.clone());
        }
        let events = self.source.list().await?;
        let mut objects = BTreeMap::new();
        let mut undated = 0;
        for event in &events {
            if replaces_an_occurrence(event, &events) {
                continue;
            }
            match self.resource(event, &replacements_of(&event.object_id, &events)) {
                Some(resource) => {
                    objects.insert(event.resource_name(), resource);
                }
                None => undated += 1,
            }
            if let Some(entry) = deadline_entry(event)
                && let Some(resource) = self.resource(&entry, &[])
            {
                objects.insert(entry.resource_name(), resource);
            }
        }
        if undated > 0 {
            debug!(undated, "events without a start are not served");
        }
        let ctag = etag_for(
            &objects
                .values()
                .map(|r| r.etag.as_str())
                .collect::<Vec<_>>()
                .join(","),
        );
        let snapshot = Arc::new(EventSnapshot { objects, ctag });
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some((Instant::now(), snapshot.clone()));
        }
        Ok(snapshot)
    }

    pub fn resource(&self, master: &Event, replacements: &[&Event]) -> Option<Resource> {
        resource_for(master, replacements, &self.config, self.fallback)
    }

    /// A fresh read of one resource: the object and, for a series, the
    /// objects replacing its occurrences. Write preconditions use this, never
    /// the snapshot.
    pub async fn read_series(
        &self,
        object_id: &str,
    ) -> Result<Option<(Event, Vec<Event>)>, SourceError> {
        let Some(master) = self.source.get(object_id).await? else {
            return Ok(None);
        };
        if master.rrule.is_none() {
            return Ok(Some((master, Vec::new())));
        }
        let replacements = self
            .source
            .list()
            .await?
            .into_iter()
            .filter(|event| event.series.as_deref() == Some(object_id))
            .collect();
        Ok(Some((master, replacements)))
    }

    pub fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    pub fn warn_unreachable(&self, err: &SourceError) {
        warn!(error = %err, "events unavailable");
    }
}

/// Option ids of the select property `key` for `names`, creating the options
/// that do not exist yet.
pub(crate) async fn option_ids(
    client: &AnytypeClient,
    space_id: &str,
    key: &str,
    names: &[String],
) -> Result<Vec<String>, SourceError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let properties = client
        .properties(space_id)
        .list()
        .await
        .map_err(transport)?
        .collect_all()
        .await
        .map_err(transport)?;
    let property = properties
        .iter()
        .find(|p| p.key == key)
        .ok_or_else(|| SourceError::Schema(format!("space has no {key} property")))?;
    let options = client
        .tags(space_id, &property.id)
        .list()
        .await
        .map_err(transport)?
        .collect_all()
        .await
        .map_err(transport)?;
    let mut ids = Vec::new();
    for name in names {
        match options.iter().find(|t| t.name.trim() == name) {
            Some(option) => ids.push(option.id.clone()),
            None => {
                info!(%name, key, "creating select option");
                let option = client
                    .new_tag(space_id, &property.id)
                    .name(name.as_str())
                    .color(anytype::objects::Color::Grey)
                    .create()
                    .await
                    .map_err(transport)?;
                ids.push(option.id);
            }
        }
    }
    Ok(ids)
}

/// Every CATEGORIES value of a component: several lines, each a comma list.
/// Trimmed, deduplicated, in order. The parser has already unescaped `\,`,
/// so a tag whose name contains a comma comes back as two tags.
pub(crate) fn categories(component: &impl Component) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for property in component
        .multi_properties()
        .get("CATEGORIES")
        .into_iter()
        .flatten()
    {
        for value in property.value().split(',') {
            let value = value.trim().to_string();
            if !value.is_empty() && !out.contains(&value) {
                out.push(value);
            }
        }
    }
    out
}

/// Tags changed when the sets differ; order is not kept by Anytype.
pub(crate) fn tags_patch(current: &[String], incoming: &[String]) -> Option<Vec<String>> {
    let mut a: Vec<&String> = current.iter().collect();
    let mut b: Vec<&String> = incoming.iter().collect();
    a.sort();
    b.sort();
    (a != b).then(|| incoming.to_vec())
}

/// Objects replacing occurrences of `master_id`.
pub fn replacements_of<'a>(master_id: &str, events: &'a [Event]) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.series.as_deref() == Some(master_id) && event.occurrence.is_some())
        .collect()
}

/// Served inside its series' resource rather than on its own. An object whose
/// series is gone or no longer recurs is served as a plain event, so it never
/// disappears from the calendar.
pub fn replaces_an_occurrence(event: &Event, events: &[Event]) -> bool {
    event.occurrence.is_some()
        && event.series.as_deref().is_some_and(|series| {
            events
                .iter()
                .any(|other| other.object_id == series && other.rrule.is_some())
        })
}

/// An instant for an event's start, for reminders.
pub fn start_instant(
    event: &Event,
    config: &CalendarConfig,
    all_day_time: NaiveTime,
) -> Option<DateTime<Utc>> {
    match event.start.as_ref()?.classify(config.date_only_timezone) {
        CalendarValue::Instant(at) => Some(at),
        CalendarValue::AllDay(day) => config
            .timezone
            .from_local_datetime(&day.and_time(all_day_time))
            .earliest()
            .map(|t| t.with_timezone(&Utc)),
    }
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

    fn event(start: Option<&str>, end: Option<&str>) -> Event {
        Event {
            object_id: "ev1".into(),
            name: "Лекция".into(),
            start: start.and_then(AnytypeDate::parse),
            end: end.and_then(AnytypeDate::parse),
            location: Some("ул. Вольская 10а".into()),
            tags: vec!["Аспирантура".into()],
            reminder_names: vec!["15m".into(), "1d".into()],
            ical_uid: None,
            object_url: None,
            last_modified: Some(Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap()),
            rrule: None,
            exdates: Vec::new(),
            series: None,
            occurrence: None,

            deadline: None,
        }
    }

    fn fallback() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn round_trip(ev: &Event, edit: impl Fn(String) -> String) -> EventPatch {
        let ics = render_event(ev, &config(), fallback()).unwrap();
        event_patch_for_update(ev, &parse_event(&edit(ics)).unwrap(), &config())
    }

    #[test]
    fn a_timed_event_renders_start_location_categories_and_relative_alarms() {
        let ics = render_event(
            &event(Some("2026-09-20T09:50:00Z"), None),
            &config(),
            fallback(),
        )
        .unwrap();
        assert!(ics.contains("BEGIN:VEVENT"), "{ics}");
        assert!(ics.contains("DTSTART:20260920T095000Z"), "{ics}");
        assert!(!ics.contains("DTEND"), "{ics}");
        assert!(ics.contains("LOCATION:ул. Вольская 10а"), "{ics}");
        assert!(ics.contains("CATEGORIES:Аспирантура"), "{ics}");
        assert!(ics.contains("TRIGGER:-PT15M"), "{ics}");
        assert!(ics.contains("TRIGGER:-PT1440M"), "{ics}");
        assert_eq!(
            ics,
            render_event(
                &event(Some("2026-09-20T09:50:00Z"), None),
                &config(),
                fallback()
            )
            .unwrap()
        );
    }

    #[test]
    fn an_all_day_event_gets_an_exclusive_end() {
        let ics = render_event(
            &event(Some("2026-09-19T20:00:00Z"), Some("2026-09-20T20:00:00Z")),
            &config(),
            fallback(),
        )
        .unwrap();
        assert!(ics.contains("DTSTART;VALUE=DATE:20260920"), "{ics}");
        assert!(ics.contains("DTEND;VALUE=DATE:20260922"), "{ics}");
    }

    #[test]
    fn an_undated_event_is_not_rendered() {
        assert!(render_event(&event(None, None), &config(), fallback()).is_none());
    }

    #[test]
    fn an_unchanged_event_changes_nothing() {
        for ev in [
            event(Some("2026-09-20T09:50:00Z"), None),
            event(Some("2026-09-20T09:50:00Z"), Some("2026-09-20T11:20:00Z")),
            event(Some("2026-09-19T20:00:00Z"), None),
            event(Some("2026-09-19T20:00:00Z"), Some("2026-09-20T20:00:00Z")),
        ] {
            assert!(round_trip(&ev, |s| s).is_empty(), "{ev:?}");
        }
    }

    #[test]
    fn moving_and_extending_an_event_writes_start_and_end() {
        let ev = event(Some("2026-09-20T09:50:00Z"), None);
        let patch = round_trip(&ev, |s| {
            s.replace(
                "DTSTART:20260920T095000Z",
                "DTSTART:20260921T100000Z\r\nDTEND:20260921T113000Z",
            )
        });
        assert_eq!(patch.start, Some(Some("2026-09-21T10:00:00Z".into())));
        assert_eq!(patch.end, Some(Some("2026-09-21T11:30:00Z".into())));
    }

    #[test]
    fn a_one_day_all_day_event_keeps_no_end() {
        let ev = event(Some("2026-09-19T20:00:00Z"), None);
        let patch = round_trip(&ev, |s| {
            s.replace("DTSTART;VALUE=DATE:20260920", "DTSTART;VALUE=DATE:20260925")
                .replace("DTEND;VALUE=DATE:20260921", "DTEND;VALUE=DATE:20260926")
        });
        assert_eq!(patch.start, Some(Some("2026-09-24T20:00:00Z".into())));
        assert_eq!(patch.end, None);
    }

    #[test]
    fn a_body_without_a_final_line_break_is_read() {
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART:20261001T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR";
        assert_eq!(parse_event(body).unwrap().uid.as_deref(), Some("u"));
    }

    #[test]
    fn a_weekly_rule_keeps_the_wall_clock_across_a_daylight_saving_change() {
        let berlin = chrono_tz::Europe::Berlin;
        let mut ev = event(Some("2026-10-19T08:00:00Z"), None); // 10:00 CEST
        ev.rrule = Some("FREQ=WEEKLY".into());
        ev.exdates = vec![AnytypeDate::parse("2026-11-02T09:00:00Z").unwrap()];
        let starts = occurrences_between(
            &ev,
            berlin,
            &[],
            Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0).unwrap(),
        )
        .unwrap();
        let raw: Vec<&str> = starts.iter().map(|s| s.raw.as_str()).collect();
        // 26 October is after the switch to CET: still 10:00 local, 09:00 UTC.
        assert_eq!(
            raw,
            [
                "2026-10-19T08:00:00Z",
                "2026-10-26T09:00:00Z",
                "2026-11-09T09:00:00Z"
            ]
        );
    }

    #[test]
    fn exdates_are_read_in_every_form_calino_writes() {
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART:20261001T100000Z\r\nRRULE:FREQ=DAILY\r\nEXDATE;TZID=Europe/Saratov:20261002T140000\r\nEXDATE:20261003T100000Z,20261004T100000Z\r\nEXDATE:20261005T140000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let series = parse_series(body).unwrap();
        let ev = Event {
            rrule: Some("FREQ=DAILY".into()),
            ..event(Some("2026-10-01T10:00:00Z"), None)
        };
        let plan = plan_series_update(&ev, &[], &series, &config());
        assert_eq!(
            plan.master.exdates,
            Some(vec![
                "2026-10-02T10:00:00Z".to_string(),
                "2026-10-03T10:00:00Z".to_string(),
                "2026-10-04T10:00:00Z".to_string(),
                // Floating, read as local time in the calendar's zone.
                "2026-10-05T10:00:00Z".to_string(),
            ])
        );
        assert!(plan.master.rrule.is_none(), "{plan:?}");
    }

    #[test]
    fn a_reordered_rule_is_the_same_rule() {
        assert!(same_rule(
            Some("FREQ=WEEKLY;BYDAY=MO;UNTIL=20261228T195959Z"),
            Some("UNTIL=20261228T195959Z;FREQ=WEEKLY;BYDAY=MO")
        ));
        assert!(!same_rule(Some("FREQ=WEEKLY"), None));
    }

    #[test]
    fn triggers_become_option_names() {
        assert_eq!(parse_before("-PT15M"), Some(Duration::minutes(15)));
        assert_eq!(parse_before("-P1D"), Some(Duration::days(1)));
        assert_eq!(parse_before("-PT1H30M"), Some(Duration::minutes(90)));
        assert_eq!(parse_before("-P1W"), Some(Duration::weeks(1)));
        assert_eq!(parse_before("PT15M"), None);
        assert_eq!(lead_name(Duration::minutes(90)), "90m");
        assert_eq!(lead_name(Duration::hours(2)), "2h");
        assert_eq!(lead_name(Duration::days(2)), "2d");
        assert_eq!(lead_name(Duration::weeks(1)), "1w");
    }

    #[test]
    fn changing_alarms_in_the_client_changes_reminders() {
        let ev = event(Some("2026-09-20T09:50:00Z"), None);
        let ics = render_event(&ev, &config(), fallback()).unwrap();
        // Drop the 1-day alarm, keep 15 minutes.
        let start = ics.rfind("BEGIN:VALARM").unwrap();
        let first = ics.find("BEGIN:VALARM").unwrap();
        let (keep, drop) = if ics[first..].contains("-P1D") && ics[first..start].contains("-P1D") {
            (start, first)
        } else {
            (first, start)
        };
        let end = ics[drop..].find("END:VALARM\r\n").unwrap() + drop + "END:VALARM\r\n".len();
        let _ = keep;
        let edited = format!("{}{}", &ics[..drop], &ics[end..]);
        let patch = event_patch_for_update(&ev, &parse_event(&edited).unwrap(), &config());
        assert!(
            matches!(patch.reminders.as_deref(), Some([one]) if one == "15m" || one == "1d"),
            "{patch:?}"
        );
    }

    #[test]
    fn a_new_event_from_the_client() {
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u-1\r\nSUMMARY:Встреча\r\nDTSTART:20261001T100000Z\r\nDTEND:20261001T110000Z\r\nLOCATION:Кафедра\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT30M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let incoming = parse_event(body).unwrap();
        let patch = event_patch_for_create(&incoming, &config());
        assert_eq!(patch.name.as_deref(), Some("Встреча"));
        assert_eq!(patch.start, Some(Some("2026-10-01T10:00:00Z".into())));
        assert_eq!(patch.end, Some(Some("2026-10-01T11:00:00Z".into())));
        assert_eq!(patch.location, Some(Some("Кафедра".into())));
        assert_eq!(patch.reminders, Some(vec!["30m".into()]));
    }
}
