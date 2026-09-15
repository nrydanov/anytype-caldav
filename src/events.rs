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
    sync::{Arc, Mutex},
    time::{Duration as StdDuration, Instant},
};

use anytype::{client::AnytypeClient, objects::Object, properties::SetProperty};
use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
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

// ------------------------------------------------------------------ rendering

fn wire_date(value: CalendarValue) -> DatePerhapsTime {
    match value {
        CalendarValue::AllDay(day) => DatePerhapsTime::Date(day),
        CalendarValue::Instant(at) => DatePerhapsTime::DateTime(CalendarDateTime::Utc(at)),
    }
}

/// DTSTART and DTEND as a client sees them. `None` without a start: a VEVENT
/// without DTSTART is not a calendar entry.
pub fn wire_dates(
    event: &Event,
    config: &CalendarConfig,
) -> Option<(DatePerhapsTime, Option<DatePerhapsTime>)> {
    let tz = config.date_only_timezone;
    let start = event.start.as_ref()?.classify(tz);
    let end = event.end.as_ref().map(|end| end.classify(tz));
    let dtend = match (start, end) {
        // Last day inclusive in Anytype, exclusive on the wire.
        (_, Some(CalendarValue::AllDay(last))) => {
            Some(DatePerhapsTime::Date(last + Duration::days(1)))
        }
        (_, Some(end @ CalendarValue::Instant(_))) => Some(wire_date(end)),
        (CalendarValue::AllDay(day), None) => Some(DatePerhapsTime::Date(day + Duration::days(1))),
        (CalendarValue::Instant(_), None) => None,
    };
    Some((wire_date(start), dtend))
}

pub fn render_event(
    event: &Event,
    config: &CalendarConfig,
    fallback: DateTime<Utc>,
) -> Option<String> {
    let (start, end) = wire_dates(event, config)?;
    let modified = event.last_modified.unwrap_or(fallback);
    let mut vevent = icalendar::Event::new();
    vevent
        .uid(&event.uid())
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
    }
    let mut vevent = vevent.done();
    for tag in &event.tags {
        vevent.add_multi_property("CATEGORIES", tag);
    }
    // Calino reads only `-PT…` durations (`parseTriggerDuration` in
    // `icalTypeMapping.ts`), so days and weeks are written as minutes.
    let uid = event.uid();
    for (index, lead) in event.leads().into_iter().enumerate() {
        let mut alarm = Alarm::display(&event.name, Trigger::before_start(lead));
        alarm.append_property(Property::new(
            "TRIGGER",
            format!("-PT{}M", lead.num_minutes()),
        ));
        vevent.alarm(stable_alarm(alarm.done(), &uid, index, modified));
    }
    let mut calendar = Calendar::new();
    calendar
        .name(&config.name)
        .timezone(config.timezone.name())
        .push(vevent);
    Some(calendar.to_string())
}

pub fn resource_for(
    event: &Event,
    config: &CalendarConfig,
    fallback: DateTime<Utc>,
) -> Option<Resource> {
    let ics = render_event(event, config, fallback)?;
    Some(Resource {
        object_id: event.object_id.clone(),
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
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventPatch {
    pub name: Option<String>,
    pub start: Option<Option<String>>,
    pub end: Option<Option<String>>,
    pub location: Option<Option<String>>,
    /// Option names for `reminder_lead`.
    pub reminders: Option<Vec<String>>,
}

impl EventPatch {
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.start.is_none()
            && self.end.is_none()
            && self.location.is_none()
            && self.reminders.is_none()
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EventWriteError {
    #[error("body is not an iCalendar document: {0}")]
    NotCalendar(String),
    #[error("body carries no VEVENT")]
    NoEvent,
    #[error("body carries {0} VEVENTs; recurrence overrides are not supported")]
    Overrides(usize),
    #[error("recurring events are not supported; Anytype has no recurrence for events")]
    Recurring,
}

pub fn parse_event(body: &str) -> Result<IncomingEvent, EventWriteError> {
    let calendar: Calendar = terminated(body)
        .parse()
        .map_err(EventWriteError::NotCalendar)?;
    let events: Vec<&icalendar::Event> = calendar.events().collect();
    let event = match events.as_slice() {
        [] => return Err(EventWriteError::NoEvent),
        [one] => *one,
        many => return Err(EventWriteError::Overrides(many.len())),
    };
    if event.property_value("RRULE").is_some() || event.property_value("RDATE").is_some() {
        return Err(EventWriteError::Recurring);
    }
    let mut leads: Vec<Duration> = event
        .components()
        .iter()
        .filter(|c| c.component_kind() == "VALARM")
        .filter_map(|alarm| alarm.property_value("TRIGGER"))
        .filter_map(parse_before)
        .collect();
    leads.sort();
    leads.dedup();
    Ok(IncomingEvent {
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
    })
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
    patch
}

pub fn event_patch_for_create(incoming: &IncomingEvent, config: &CalendarConfig) -> EventPatch {
    EventPatch {
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

// ------------------------------------------------------------------ Anytype

/// Where events are read from and written to; Anytype in production, memory
/// in tests.
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    async fn list(&self) -> Result<Vec<Event>, SourceError>;
    async fn get(&self, object_id: &str) -> Result<Option<Event>, SourceError>;
    async fn create(&self, uid: &str, patch: &EventPatch) -> Result<String, SourceError>;
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
    }
}

fn transport(err: impl std::fmt::Display) -> SourceError {
    SourceError::Transport(err.to_string())
}

impl AnytypeEvents {
    pub fn new(client: AnytypeClient, space_id: String) -> Self {
        Self { client, space_id }
    }
    /// Option ids of `reminder_lead` for `names`, creating missing options.
    async fn reminder_ids(&self, names: &[String]) -> Result<Vec<String>, SourceError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let properties = self
            .client
            .properties(&self.space_id)
            .list()
            .await
            .map_err(transport)?
            .collect_all()
            .await
            .map_err(transport)?;
        let property = properties
            .iter()
            .find(|p| p.key == "reminder_lead")
            .ok_or_else(|| SourceError::Schema("space has no reminder_lead property".into()))?;
        let tags = self
            .client
            .tags(&self.space_id, &property.id)
            .list()
            .await
            .map_err(transport)?
            .collect_all()
            .await
            .map_err(transport)?;
        let mut ids = Vec::new();
        for name in names {
            match tags.iter().find(|t| t.name.trim() == name) {
                Some(tag) => ids.push(tag.id.clone()),
                None => {
                    info!(%name, "creating reminder_lead option");
                    let tag = self
                        .client
                        .new_tag(&self.space_id, &property.id)
                        .name(name.as_str())
                        .color(anytype::objects::Color::Grey)
                        .create()
                        .await
                        .map_err(transport)?;
                    ids.push(tag.id);
                }
            }
        }
        Ok(ids)
    }

    async fn properties_for(
        &self,
        patch: &EventPatch,
    ) -> Result<Vec<serde_json::Value>, SourceError> {
        let mut out = Vec::new();
        for (key, value) in [("start_date", &patch.start), ("end_date", &patch.end)] {
            if let Some(value) = value {
                out.push(serde_json::json!({ "key": key, "date": value }));
            }
        }
        if let Some(location) = &patch.location {
            out.push(serde_json::json!({ "key": "address", "text": location.clone().unwrap_or_default() }));
        }
        if let Some(names) = &patch.reminders {
            let ids = self.reminder_ids(names).await?;
            out.push(serde_json::json!({ "key": "reminder_lead", "multi_select": ids }));
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
            match self.resource(event) {
                Some(resource) => {
                    objects.insert(event.resource_name(), resource);
                }
                None => undated += 1,
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

    pub fn resource(&self, event: &Event) -> Option<Resource> {
        resource_for(event, &self.config, self.fallback)
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
