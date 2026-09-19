//! The home set: which calendars a reader is offered.

use axum::response::Response;

use super::auth::Reader;
use super::names::named;
use super::paths::collection_path;
use super::settings::settings_collection_props;
use super::tasks::collection_view;
use super::xml::{collection_props, multistatus, prop_text, response};
use super::{EVENTS, SETTINGS, TASKS, flat_contents, source_failure, with_snapshot};
use crate::http::AppState;

/// The calendars a reader sees: the flat task collection or one per person,
/// the events and the client's settings.
pub(super) async fn home_set(
    state: &AppState,
    reader: &Reader,
    names: &str,
    settings_storage: &str,
) -> Response {
    let writable = state.writer.is_some();
    let event_snapshot = match &state.events {
        Some(service) => match service.snapshot().await {
            Ok(snapshot) => Some(snapshot),
            Err(err) => return source_failure(&err),
        },
        None => None,
    };
    let events = event_snapshot
        .as_ref()
        .filter(|_| !state.events_in_tasks)
        .map(|snapshot| {
            response(
                EVENTS,
                &collection_props(
                    &["VEVENT"],
                    &named(state, names, EVENTS, &state.calendar_names.events),
                    &snapshot.ctag,
                    writable,
                ),
            )
        });
    let settings = state.documents.as_ref().map(|store| {
        response(
            SETTINGS,
            &settings_collection_props(store, settings_storage),
        )
    });
    with_snapshot(state, |snapshot| {
        // Calino keeps one entry per UID across all its calendars, so
        // a home set lists a task once: the flat collection or the
        // calendars per person, never both.
        let mut responses = Vec::new();
        if snapshot.collections.is_empty() {
            let merged = event_snapshot.as_deref().filter(|_| state.events_in_tasks);
            let (components, ctag) = flat_contents(&snapshot.etag, merged);
            responses.push(response(
                TASKS,
                &collection_props(
                    components,
                    &named(state, names, TASKS, &state.calendar_names.tasks),
                    &ctag,
                    writable,
                ),
            ));
        }
        // A person's own calendar comes first, because Calino makes
        // the first collection the default one for a new task. The
        // others arrive switched off: a fork of Calino reads the flag
        // once, when it first sees the url, and the stock client
        // ignores it. Nobody in particular gets them all switched on.
        let own = match &reader {
            Reader::Person(id) => Some(id.as_str()),
            Reader::Shared => None,
        };
        let is_own = |collection: &crate::feed::Collection| {
            own.is_some() && collection.member_id.as_deref() == own
        };
        let mut collections: Vec<_> = snapshot.collections.iter().collect();
        collections.sort_by_key(|(_, collection)| !is_own(collection));
        responses.extend(collections.into_iter().map(|(key, collection)| {
            let mut props = collection_props(
                &["VTODO"],
                &named(
                    state,
                    names,
                    &collection_path(key),
                    &collection.display_name,
                ),
                &collection_view(snapshot, key, reader)
                    .map(|view| view.ctag.into_owned())
                    .unwrap_or_default(),
                writable,
            );
            if own.is_some() && !is_own(collection) {
                // An explicit value: the client parses an empty
                // element into an object, which is truthy.
                props.push(prop_text("cs:calendar-hidden", "1"));
            }
            response(&collection_path(key), &props)
        }));
        responses.extend(events);
        responses.extend(settings);
        multistatus(responses)
    })
    .await
}
