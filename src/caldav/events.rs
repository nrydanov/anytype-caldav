//! The event collection, `events/`, with recurring series and deadlines.

use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use tracing::{debug, info, warn};

use super::names::named;
use super::paths::resource_name_in;
use super::xml::{
    Report, collection_props, member_props, multistatus, prop_text, report_kind, response, status,
};
use super::{EVENTS, header_text, precondition_failed, source_failure};
use crate::{
    events::{self as ev, EventService, EventWriteError},
    feed::calino_filename,
    http::AppState,
    source::SourceError,
};

/// Every request under `events/`. Mirrors the task collection: the snapshot
/// serves reads, a fresh read of the object decides write preconditions.
#[allow(clippy::too_many_arguments)]
pub(super) async fn events_route(
    state: &AppState,
    service: &EventService,
    names: &str,
    method: &Method,
    path: &str,
    depth: &str,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    let writable = state.writer.is_some();
    let name = resource_name_in(path, EVENTS).map(str::to_string);
    match (method.as_str(), path, name) {
        ("PROPFIND", EVENTS, _) => match service.snapshot().await {
            Ok(snapshot) => {
                let mut responses = vec![response(
                    EVENTS,
                    &collection_props(
                        &["VEVENT"],
                        &named(state, names, EVENTS, &state.calendar_names.events),
                        &snapshot.ctag,
                        writable,
                    ),
                )];
                if depth == "1" {
                    responses.extend(snapshot.objects.iter().map(|(name, resource)| {
                        response(&event_href(name), &member_props(&resource.etag))
                    }));
                }
                multistatus(responses)
            }
            Err(err) => source_failure(&err),
        },
        ("REPORT", EVENTS, _) => match report_kind(body) {
            Report::SyncCollection => {
                info!("caldav sync-collection on events requested but not supported");
                status(StatusCode::FORBIDDEN)
            }
            Report::TasksOnly => {
                debug!("caldav calendar-query for tasks in events: none");
                multistatus(Vec::new())
            }
            Report::Tasks | Report::EventsOnly => match service.snapshot().await {
                Ok(snapshot) => {
                    debug!(
                        resources = snapshot.objects.len(),
                        "caldav calendar-query for events"
                    );
                    multistatus(
                        snapshot
                            .objects
                            .iter()
                            .map(|(name, resource)| {
                                response(
                                    &event_href(name),
                                    &[
                                        prop_text("d:getetag", &resource.etag),
                                        prop_text("d:getcontenttype", "text/calendar"),
                                        prop_text("c:calendar-data", &resource.ics),
                                    ],
                                )
                            })
                            .collect(),
                    )
                }
                Err(err) => source_failure(&err),
            },
        },
        ("PROPFIND" | "GET" | "HEAD", _, Some(name)) => {
            let snapshot = match service.snapshot().await {
                Ok(snapshot) => snapshot,
                Err(err) => return source_failure(&err),
            };
            let Some(resource) = snapshot.objects.get(&name) else {
                debug!(resource = %name, "caldav event not found");
                return status(StatusCode::NOT_FOUND);
            };
            if method.as_str() == "PROPFIND" {
                return multistatus(vec![response(
                    &event_href(&name),
                    &member_props(&resource.etag),
                )]);
            }
            let body = if method == Method::HEAD {
                Body::empty()
            } else {
                Body::from(resource.ics.to_string())
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
                .header(header::ETAG, &resource.etag)
                .body(body)
                .expect("valid response")
        }
        ("PUT", _, Some(name)) if writable && name.ends_with(ev::DEADLINE_SUFFIX) => {
            put_deadline(service, &name, headers, body).await
        }
        ("DELETE", _, Some(name)) if writable && name.ends_with(ev::DEADLINE_SUFFIX) => {
            delete_deadline(service, &name, headers).await
        }
        ("PUT", _, Some(name)) if writable => put_event(service, &name, headers, body).await,
        ("DELETE", _, Some(name)) if writable => delete_event(service, &name, headers).await,
        ("PUT" | "DELETE" | "PROPPATCH" | "MKCOL" | "MKCALENDAR" | "MOVE" | "COPY", _, _) => {
            info!(%method, path, writable, "caldav event write refused");
            status(StatusCode::FORBIDDEN)
        }
        _ => {
            debug!(%method, path, "caldav events path not found");
            status(StatusCode::NOT_FOUND)
        }
    }
}

pub(super) fn event_href(name: &str) -> String {
    format!("{EVENTS}{name}.ics")
}

async fn locate_event(service: &EventService, name: &str) -> Result<Option<String>, Box<Response>> {
    match service.snapshot().await {
        Ok(snapshot) => Ok(snapshot.objects.get(name).map(|r| r.object_id.clone())),
        Err(err) => Err(Box::new(source_failure(&err))),
    }
}

async fn put_event(
    service: &EventService,
    name: &str,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    let if_match = header_text(headers, header::IF_MATCH).filter(|v| v != "*");
    let if_none_match = header_text(headers, header::IF_NONE_MATCH);
    debug!(resource = name, body, "caldav event put body");
    let incoming = match ev::parse_series(body) {
        Ok(incoming) => incoming,
        Err(err) => {
            warn!(resource = name, error = %err, body_bytes = body.len(), "caldav event put: refused body");
            let code = match err {
                EventWriteError::NotCalendar(_) => StatusCode::BAD_REQUEST,
                _ => StatusCode::FORBIDDEN,
            };
            return Response::builder()
                .status(code)
                .body(Body::from(err.to_string()))
                .expect("static response");
        }
    };
    debug!(
        resource = name,
        ?incoming,
        ?if_match,
        ?if_none_match,
        "caldav event put parsed"
    );

    let object_id = match locate_event(service, name).await {
        Ok(Some(id)) => id,
        Ok(None) => return create_event(service, name, if_match, &incoming).await,
        Err(response) => return *response,
    };
    let (current, replacements) = match service.read_series(&object_id).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            info!(resource = name, %object_id, "caldav event put: event vanished since the snapshot");
            service.invalidate();
            return if if_match.is_some() {
                precondition_failed("resource no longer exists")
            } else {
                create_event(service, name, None, &incoming).await
            };
        }
        Err(err) => return source_failure(&err),
    };
    let refs: Vec<&ev::Event> = replacements.iter().collect();
    let current_etag = service.resource(&current, &refs).map(|r| r.etag);
    if if_none_match.as_deref() == Some("*") {
        warn!(resource = name, %object_id, "caldav event put: If-None-Match * on an existing resource");
        return precondition_failed("resource exists");
    }
    if let Some(expected) = &if_match
        && Some(expected) != current_etag.as_ref()
    {
        warn!(resource = name, %object_id, client_etag = %expected, server_etag = ?current_etag, "caldav event put: stale etag");
        return precondition_failed("etag mismatch");
    }
    let plan = ev::plan_series_update(&current, &replacements, &incoming, &service.config);
    if plan.is_empty() {
        info!(resource = name, %object_id, "caldav event put: nothing changed");
    } else {
        info!(resource = name, %object_id, ?plan, "caldav event put: updating event");
        let result = apply_plan(service, &object_id, &plan).await;
        service.invalidate();
        if let Err(err) = result {
            return source_failure(&err);
        }
    }
    event_written(service, &object_id, StatusCode::NO_CONTENT).await
}

/// Master first, then replacements. A failure part-way leaves the resource
/// with a new ETag, so the client's retry re-reads and re-plans against what
/// was written instead of repeating it.
async fn apply_plan(
    service: &EventService,
    master_id: &str,
    plan: &ev::SeriesPlan,
) -> Result<(), SourceError> {
    if !plan.master.is_empty() {
        service.source.update(master_id, &plan.master).await?;
    }
    for (object_id, patch) in &plan.update {
        info!(%master_id, %object_id, ?patch, "caldav event put: updating replaced occurrence");
        service.source.update(object_id, patch).await?;
    }
    for patch in &plan.create {
        info!(%master_id, ?patch, "caldav event put: creating replaced occurrence");
        service.source.create_replacement(patch).await?;
    }
    for object_id in &plan.archive {
        info!(%master_id, %object_id, "caldav event put: archiving replaced occurrence");
        service.source.archive(object_id).await?;
    }
    Ok(())
}

async fn create_event(
    service: &EventService,
    name: &str,
    if_match: Option<String>,
    incoming: &ev::IncomingSeries,
) -> Response {
    if if_match.is_some() {
        info!(
            resource = name,
            "caldav event put: If-Match on a missing resource"
        );
        return precondition_failed("resource does not exist");
    }
    let Some(uid) = incoming.master.uid.clone() else {
        warn!(resource = name, "caldav event put: new event without UID");
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("VEVENT has no UID"))
            .expect("static response");
    };
    if calino_filename(&uid) != name {
        warn!(resource = name, %uid, expected = %calino_filename(&uid), "caldav event put: resource name does not match UID");
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("resource name must be derived from the UID"))
            .expect("static response");
    }
    if incoming.master.start.is_none() {
        warn!(
            resource = name,
            "caldav event put: new event without DTSTART"
        );
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("VEVENT has no DTSTART"))
            .expect("static response");
    }
    let (patch, replacements) = ev::plan_series_create(incoming, &service.config);
    info!(resource = name, %uid, ?patch, replacements = replacements.len(), "caldav event put: creating event");
    let object_id = match service.source.create(&uid, &patch).await {
        Ok(id) => id,
        Err(err) => return source_failure(&err),
    };
    for mut replacement in replacements {
        replacement.series = Some(object_id.clone());
        info!(resource = name, %object_id, ?replacement, "caldav event put: creating replaced occurrence");
        if let Err(err) = service.source.create_replacement(&replacement).await {
            service.invalidate();
            return source_failure(&err);
        }
    }
    service.invalidate();
    event_written(service, &object_id, StatusCode::CREATED).await
}

async fn event_written(service: &EventService, object_id: &str, code: StatusCode) -> Response {
    let mut builder = Response::builder().status(code);
    match service.read_series(object_id).await {
        Ok(Some((event, replacements))) => {
            let refs: Vec<&ev::Event> = replacements.iter().collect();
            match service.resource(&event, &refs) {
                Some(resource) => {
                    info!(%object_id, etag = %resource.etag, status = code.as_u16(), "caldav event write done");
                    builder = builder.header(header::ETAG, resource.etag);
                }
                None => warn!(%object_id, "caldav event write done but the event has no start"),
            }
        }
        other => {
            warn!(%object_id, result = ?other.map(|e| e.is_some()), "caldav event write done but reading it back failed");
        }
    }
    builder.body(Body::empty()).expect("static response")
}

/// The event behind a deadline's entry, read afresh, once the client's
/// `If-Match` agrees with that entry as it is now.
async fn deadline_owner(
    service: &EventService,
    name: &str,
    headers: &HeaderMap,
) -> Result<ev::Event, Box<Response>> {
    let if_match = header_text(headers, header::IF_MATCH).filter(|v| v != "*");
    let Some(object_id) = locate_event(service, name).await? else {
        // A deadline is made on its event, never on its own.
        info!(resource = name, "caldav deadline: no such entry");
        return Err(Box::new(status(StatusCode::NOT_FOUND)));
    };
    let current = match service.source.get(&object_id).await {
        Ok(Some(event)) => event,
        Ok(None) => {
            service.invalidate();
            return Err(Box::new(status(StatusCode::NOT_FOUND)));
        }
        Err(err) => return Err(Box::new(source_failure(&err))),
    };
    let current_etag = ev::deadline_entry(&current, service.config.language)
        .and_then(|entry| service.resource(&entry, &[]))
        .map(|resource| resource.etag);
    if let Some(expected) = &if_match
        && Some(expected) != current_etag.as_ref()
    {
        warn!(resource = name, %object_id, client_etag = %expected, server_etag = ?current_etag, "caldav deadline: stale etag");
        return Err(Box::new(precondition_failed("etag mismatch")));
    }
    Ok(current)
}

/// Moving `<name> (дедлайн)` moves the event's deadline to the entry's start.
/// Nothing else of the entry is written: its name and the rest belong to the
/// event.
async fn put_deadline(
    service: &EventService,
    name: &str,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    let current = match deadline_owner(service, name, headers).await {
        Ok(event) => event,
        Err(response) => return *response,
    };
    let deadline = match ev::parse_event(body) {
        Ok(incoming) => ev::deadline_from(&incoming, &service.config),
        Err(err) => {
            warn!(resource = name, error = %err, "caldav deadline put: unreadable body");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(err.to_string()))
                .expect("static response");
        }
    };
    let Some(deadline) = deadline else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("a deadline needs DTSTART"))
            .expect("static response");
    };
    info!(resource = name, object_id = %current.object_id, event = %current.name, %deadline, "caldav deadline put: moving the deadline");
    let patch = ev::EventPatch {
        deadline: Some(Some(deadline)),
        ..Default::default()
    };
    if let Err(err) = service.source.update(&current.object_id, &patch).await {
        return source_failure(&err);
    }
    service.invalidate();
    let mut builder = Response::builder().status(StatusCode::NO_CONTENT);
    if let Ok(Some(event)) = service.source.get(&current.object_id).await
        && let Some(resource) = ev::deadline_entry(&event, service.config.language)
            .and_then(|e| service.resource(&e, &[]))
    {
        builder = builder.header(header::ETAG, resource.etag);
    }
    builder.body(Body::empty()).expect("static response")
}

/// Deleting `<name> (дедлайн)` clears the event's deadline; the event stays.
async fn delete_deadline(service: &EventService, name: &str, headers: &HeaderMap) -> Response {
    let current = match deadline_owner(service, name, headers).await {
        Ok(event) => event,
        Err(response) => return *response,
    };
    info!(resource = name, object_id = %current.object_id, event = %current.name, "caldav deadline delete: clearing the deadline");
    let patch = ev::EventPatch {
        deadline: Some(None),
        ..Default::default()
    };
    if let Err(err) = service.source.update(&current.object_id, &patch).await {
        return source_failure(&err);
    }
    service.invalidate();
    status(StatusCode::NO_CONTENT)
}

async fn delete_event(service: &EventService, name: &str, headers: &HeaderMap) -> Response {
    let if_match = header_text(headers, header::IF_MATCH).filter(|v| v != "*");
    let object_id = match locate_event(service, name).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            info!(resource = name, "caldav event delete: already gone");
            return status(StatusCode::NOT_FOUND);
        }
        Err(response) => return *response,
    };
    let (current, replacements) = match service.read_series(&object_id).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            service.invalidate();
            return status(StatusCode::NOT_FOUND);
        }
        Err(err) => return source_failure(&err),
    };
    let refs: Vec<&ev::Event> = replacements.iter().collect();
    let current_etag = service.resource(&current, &refs).map(|r| r.etag);
    if let Some(expected) = &if_match
        && Some(expected) != current_etag.as_ref()
    {
        warn!(resource = name, %object_id, client_etag = %expected, server_etag = ?current_etag, "caldav event delete: stale etag");
        return precondition_failed("etag mismatch");
    }
    info!(resource = name, %object_id, event = %current.name, replacements = replacements.len(), "caldav event delete: archiving event");
    for replacement in &replacements {
        if let Err(err) = service.source.archive(&replacement.object_id).await {
            service.invalidate();
            return source_failure(&err);
        }
    }
    if let Err(err) = service.source.archive(&object_id).await {
        service.invalidate();
        return source_failure(&err);
    }
    service.invalidate();
    status(StatusCode::NO_CONTENT)
}
