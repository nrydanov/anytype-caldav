//! The task collections: `tasks/`, `tasks-<key>/` and the tasks in them.

use std::{borrow::Cow, collections::BTreeMap};

use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use tracing::{debug, info, warn};

use super::auth::Reader;
use super::events::{event_href, events_route};
use super::names::named;
use super::paths::{collection_path, href_for, object_id, tasks_key};
use super::xml::{
    Report, collection_props, member_props, multistatus, prop_text, report_kind, response, status,
};
use super::{
    flat_contents, header_text, precondition_failed, source_failure, unavailable, with_snapshot,
};
use crate::{
    feed::{Outcome, Resource, Snapshot, calino_filename, collection_ctag},
    http::AppState,
    writeback::{self, WriteError},
};

/// Every request under `tasks/` and under a `tasks-<key>/` of its own. The two
/// differ only in which resources they list: a member GET still looks the name
/// up in the flat index, because the same task is served under the same name in
/// every collection it belongs to.
#[allow(clippy::too_many_arguments)]
pub(super) async fn tasks_route(
    state: &AppState,
    reader: &Reader,
    names: &str,
    method: &Method,
    path: &str,
    depth: &str,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    let key = tasks_key(path).expect("routed by the key").to_string();
    let collection = collection_path(&key);
    let writable = state.writer.is_some();
    let name = object_id(path).map(|(_, name)| name.to_string());
    // With `events_in_tasks`, the flat collection also serves the events. An
    // event, or a new resource whose body is one, is handled by the events
    // code as if it had been addressed under `events/`.
    let events = match (&state.events, key.is_empty() && state.events_in_tasks) {
        (Some(service), true) => match service.snapshot().await {
            Ok(snapshot) => Some((service, snapshot)),
            Err(err) => return source_failure(&err),
        },
        _ => None,
    };
    if let (Some((service, snapshot)), Some(name)) = (&events, &name) {
        let is_event = snapshot.objects.contains_key(name)
            || (method == Method::PUT && body.contains("BEGIN:VEVENT"));
        if is_event {
            let path = event_href(name);
            return events_route(state, service, names, method, &path, depth, headers, body).await;
        }
    }
    let events = events.map(|(_, snapshot)| snapshot);
    match (method.as_str(), path == collection, name) {
        ("PROPFIND", true, _) => {
            with_snapshot(state, |snapshot| {
                let Some(view) = collection_view(snapshot, &key, reader) else {
                    return status(StatusCode::NOT_FOUND);
                };
                let (components, ctag) = flat_contents(&view.ctag, events.as_deref());
                let mut responses = vec![response(
                    &collection,
                    &collection_props(
                        components,
                        &named(
                            state,
                            names,
                            &collection,
                            view.display_name.unwrap_or(&state.calendar_names.tasks),
                        ),
                        &ctag,
                        writable,
                    ),
                )];
                // Depth 1 also lists members with their ETags, which is how a
                // generic client finds out what changed without a REPORT.
                if depth == "1" {
                    let merged = events.iter().flat_map(|events| events.objects.iter());
                    responses.extend(view.objects.iter().chain(merged).map(|(id, resource)| {
                        response(&href_for(&key, id), &member_props(&resource.etag))
                    }));
                }
                multistatus(responses)
            })
            .await
        }
        ("REPORT", true, _) => {
            let kind = report_kind(body);
            with_snapshot(state, |snapshot| match kind {
                Report::SyncCollection => {
                    // Never advertised, so Calino does not send it; a 403
                    // makes any client fall back to a full listing.
                    info!("caldav sync-collection requested but not supported");
                    status(StatusCode::FORBIDDEN)
                }
                Report::EventsOnly if events.is_none() => {
                    debug!("caldav calendar-query for events: none");
                    multistatus(Vec::new())
                }
                kind => {
                    let Some(view) = collection_view(snapshot, &key, reader) else {
                        return status(StatusCode::NOT_FOUND);
                    };
                    let tasks = view.objects.iter().filter(|_| kind != Report::EventsOnly);
                    let merged = events
                        .iter()
                        .flat_map(|events| events.objects.iter())
                        .filter(|_| kind != Report::TasksOnly);
                    debug!(
                        resources = view.objects.len(),
                        events = events.as_ref().map(|events| events.objects.len()),
                        key,
                        ?kind,
                        "caldav calendar-query for tasks"
                    );
                    multistatus(
                        tasks
                            .chain(merged)
                            .map(|(id, resource)| {
                                response(
                                    &href_for(&key, id),
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
            })
            .await
        }
        ("PROPFIND" | "GET" | "HEAD", _, Some(name)) => {
            let is_get = method.as_str() != "PROPFIND";
            let head = method == Method::HEAD;
            with_snapshot(state, move |snapshot| {
                let view = collection_view(snapshot, &key, reader);
                let resource = view.as_ref().and_then(|view| view.objects.get(&name));
                let Some(resource) = resource else {
                    debug!(object_id = %name, key, "caldav resource not found");
                    return status(StatusCode::NOT_FOUND);
                };
                if !is_get {
                    return multistatus(vec![response(
                        &href_for(&key, &name),
                        &member_props(&resource.etag),
                    )]);
                }
                let body = if head {
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
            })
            .await
        }
        ("PUT", _, Some(name)) if writable => put(state, reader, &key, &name, headers, body).await,
        ("DELETE", _, Some(name)) if writable => delete(state, reader, &key, &name, headers).await,
        ("PUT" | "DELETE" | "PROPPATCH" | "MKCOL" | "MKCALENDAR" | "MOVE" | "COPY", _, _) => {
            info!(%method, path, writable, "caldav write refused");
            status(StatusCode::FORBIDDEN)
        }
        _ => {
            debug!(%method, path, "caldav path not found");
            status(StatusCode::NOT_FOUND)
        }
    }
}

/// What one task collection serves.
pub(super) struct View<'a> {
    /// A person's name, or `None` for the flat collection, whose name is configured.
    display_name: Option<&'a str>,
    pub(super) ctag: Cow<'a, str>,
    objects: Cow<'a, BTreeMap<String, Resource>>,
}

/// The flat collection is the whole snapshot; a keyed one is its own subset,
/// with a ctag that moves only when its own members do. `None` for a key the
/// snapshot does not hold: a subscription saved before an assignee left the
/// space, or before grouping was configured at all.
///
/// A task shared with the person reading is shown in their own calendar and
/// in no other, wherever `collections` serves it: they see it without looking
/// in someone else's calendar, and a client still meets each UID once.
pub(super) fn collection_view<'a>(
    snapshot: &'a Snapshot,
    key: &str,
    reader: &Reader,
) -> Option<View<'a>> {
    if key.is_empty() {
        return Some(View {
            display_name: None,
            ctag: Cow::Borrowed(&snapshot.etag),
            objects: Cow::Borrowed(&snapshot.objects),
        });
    }
    let collection = snapshot.collections.get(key)?;
    let mut objects = Cow::Borrowed(&collection.objects);
    if let Reader::Person(me) = reader {
        let own = collection.member_id.as_ref() == Some(me);
        for (name, assignees) in snapshot.shared.iter() {
            if !assignees.contains(me) {
                continue;
            }
            let listed = objects.contains_key(name);
            if own && !listed {
                if let Some(resource) = snapshot.objects.get(name) {
                    objects.to_mut().insert(name.clone(), resource.clone());
                }
            } else if !own && listed {
                objects.to_mut().remove(name);
            }
        }
    }
    let ctag = match &objects {
        Cow::Borrowed(_) => Cow::Borrowed(collection.ctag.as_str()),
        Cow::Owned(objects) => Cow::Owned(collection_ctag(objects)),
    };
    Some(View {
        display_name: Some(&collection.display_name),
        ctag,
        objects,
    })
}

/// What the current snapshot knows about a resource name under a collection.
struct Located {
    /// The object behind the name, wherever in the space it is served.
    object_id: Option<String>,
    /// Whether the addressed collection is the one that serves it.
    here: bool,
    /// Whose calendar the collection is; `None` for the flat one and for the
    /// unassigned.
    member_id: Option<String>,
    /// Whose calendar serves the object now; `None` among the unassigned.
    served_by: Option<String>,
}

/// Finds the object behind a resource name in the current snapshot, or behind
/// the UID when the name is new. A key the snapshot does not hold is a 404, as
/// it is for reads.
async fn locate(
    state: &AppState,
    reader: &Reader,
    key: &str,
    name: &str,
    uid: Option<&str>,
) -> Result<Located, Box<Response>> {
    match state.feed.get().await {
        Outcome::Fresh(snapshot) | Outcome::Stale(snapshot) => {
            let Some(view) = collection_view(&snapshot, key, reader) else {
                debug!(key, "caldav task collection not found");
                return Err(Box::new(status(StatusCode::NOT_FOUND)));
            };
            let name = match snapshot.objects.contains_key(name) {
                true => Some(name),
                false => uid.and_then(|uid| snapshot.names_by_uid.get(uid).map(String::as_str)),
            };
            Ok(Located {
                object_id: name
                    .and_then(|name| snapshot.objects.get(name))
                    .map(|r| r.object_id.clone()),
                here: name.is_some_and(|name| view.objects.contains_key(name)),
                member_id: snapshot
                    .collections
                    .get(key)
                    .and_then(|collection| collection.member_id.clone()),
                served_by: name.and_then(|name| match reader {
                    Reader::Person(me)
                        if snapshot
                            .shared
                            .get(name)
                            .is_some_and(|assignees| assignees.contains(me)) =>
                    {
                        Some(me.clone())
                    }
                    _ => snapshot
                        .collections
                        .values()
                        .find(|collection| collection.objects.contains_key(name))
                        .and_then(|collection| collection.member_id.clone()),
                }),
            })
        }
        Outcome::Unavailable { category } => Err(Box::new(unavailable(category))),
    }
}

pub(super) async fn put(
    state: &AppState,
    reader: &Reader,
    key: &str,
    name: &str,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    // The whole body: what a client meant is only recoverable from it.
    debug!(resource = name, body, "caldav put body");
    let writer = state.writer.as_ref().expect("routed only with a writer");
    let if_match = header_text(headers, header::IF_MATCH).filter(|v| v != "*");
    let if_none_match = header_text(headers, header::IF_NONE_MATCH);

    let incoming = match writeback::parse(body) {
        Ok(incoming) => incoming,
        Err(err @ WriteError::NotCalendar(_)) => {
            warn!(resource = name, error = %err, body_bytes = body.len(), "caldav put: unreadable body");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(err.to_string()))
                .expect("static response");
        }
        Err(err) => {
            warn!(resource = name, error = %err, "caldav put: unsupported body");
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Body::from(err.to_string()))
                .expect("static response");
        }
    };
    debug!(
        resource = name,
        ?incoming,
        ?if_match,
        ?if_none_match,
        "caldav put parsed"
    );

    let located = match locate(state, reader, key, name, incoming.uid.as_deref()).await {
        Ok(located) => located,
        Err(response) => return *response,
    };

    let Some(object_id) = located.object_id else {
        return create(state, name, if_match, &incoming, located.member_id).await;
    };
    // Calino moves a task to another calendar with a PUT there and a DELETE of
    // the old resource. The PUT hands the task over to this calendar's person;
    // the DELETE then finds it gone.
    let moving = !located.here;

    // A fresh read decides the precondition and the patch: the snapshot may be
    // older than an edit made in Anytype a moment ago.
    let current = match writer.get_task(&object_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            info!(resource = name, %object_id, "caldav put: task vanished since the snapshot");
            state.feed.invalidate();
            return if if_match.is_some() {
                precondition_failed("resource no longer exists")
            } else {
                create(state, name, None, &incoming, located.member_id).await
            };
        }
        Err(err) => return source_failure(&err),
    };
    let current_etag = state.feed.resource_for(&current).etag;
    if !moving && if_none_match.as_deref() == Some("*") {
        warn!(resource = name, %object_id, "caldav put: If-None-Match * on an existing resource");
        return precondition_failed("resource exists");
    }
    if let Some(expected) = &if_match
        && expected != &current_etag
    {
        warn!(resource = name, %object_id, client_etag = %expected, server_etag = %current_etag, "caldav put: stale etag");
        return precondition_failed("etag mismatch");
    }

    let renderer = state.feed.renderer();
    let mut patch = writeback::for_update(
        &current,
        &renderer.wire(&current),
        &incoming,
        renderer.config(),
    );
    if moving {
        info!(resource = name, key, %object_id, from = ?located.served_by, to = ?located.member_id, "caldav put: moving the task to this calendar");
        patch.assignees = Some(handed_over(
            &current.assignees,
            located.served_by.as_deref(),
            located.member_id.as_deref(),
        ));
    }
    if patch.is_empty() {
        info!(resource = name, %object_id, "caldav put: nothing changed");
    } else {
        info!(resource = name, %object_id, ?patch, "caldav put: updating task");
        if let Err(err) = writer.update_task(&object_id, &patch).await {
            return source_failure(&err);
        }
        state.feed.invalidate();
    }
    let code = if moving {
        StatusCode::CREATED
    } else {
        StatusCode::NO_CONTENT
    };
    written(state, &object_id, code).await
}

/// The assignees once a task moves from one person's calendar to another's:
/// the first leaves the task and the second comes first, which is whose
/// calendar serves it. `None` is the unassigned, which adds nobody.
fn handed_over(current: &[String], from: Option<&str>, to: Option<&str>) -> Vec<String> {
    let mut assignees: Vec<String> = to.map(String::from).into_iter().collect();
    assignees.extend(
        current
            .iter()
            .filter(|id| Some(id.as_str()) != from && Some(id.as_str()) != to)
            .cloned(),
    );
    assignees
}

pub(super) async fn create(
    state: &AppState,
    name: &str,
    if_match: Option<String>,
    incoming: &writeback::Incoming,
    member_id: Option<String>,
) -> Response {
    let writer = state.writer.as_ref().expect("routed only with a writer");
    if if_match.is_some() {
        info!(
            resource = name,
            "caldav put: If-Match on a missing resource"
        );
        return precondition_failed("resource does not exist");
    }
    let Some(uid) = incoming.uid.clone() else {
        warn!(resource = name, "caldav put: new task without UID");
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("VTODO has no UID"))
            .expect("static response");
    };
    // The task will be served under the name derived from its UID; a client
    // that PUT it elsewhere would never find it again.
    if calino_filename(&uid) != name {
        warn!(resource = name, %uid, expected = %calino_filename(&uid), "caldav put: resource name does not match UID");
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Body::from("resource name must be derived from the UID"))
            .expect("static response");
    }
    let mut patch = writeback::for_create(incoming, state.feed.renderer().config());
    // A task created in a person's calendar is theirs.
    patch.assignees = member_id.map(|id| vec![id]);
    info!(resource = name, %uid, ?patch, "caldav put: creating task");
    let object_id = match writer.create_task(&uid, &patch).await {
        Ok(id) => id,
        Err(err) => return source_failure(&err),
    };
    state.feed.invalidate();
    written(state, &object_id, StatusCode::CREATED).await
}

/// Answers a successful write with the new ETag, read back from Anytype, so
/// the client does not need a PROPFIND to learn it.
pub(super) async fn written(state: &AppState, object_id: &str, code: StatusCode) -> Response {
    let writer = state.writer.as_ref().expect("routed only with a writer");
    let mut builder = Response::builder().status(code);
    match writer.get_task(object_id).await {
        Ok(Some(task)) => {
            let etag = state.feed.resource_for(&task).etag;
            info!(%object_id, %etag, status = code.as_u16(), "caldav write done");
            builder = builder.header(header::ETAG, etag);
        }
        other => {
            // The write happened; only the ETag is unknown. Calino recovers it
            // with a PROPFIND.
            warn!(%object_id, result = ?other.map(|t| t.is_some()), "caldav write done but reading it back failed");
        }
    }
    builder.body(Body::empty()).expect("static response")
}

/// In the flat calendar and among the unassigned a DELETE archives the task.
/// In a person's calendar it takes that person off the task: what someone means
/// by deleting from their own calendar is "not mine", and the task moves to the
/// next assignee's calendar or to the unassigned.
pub(super) async fn delete(
    state: &AppState,
    reader: &Reader,
    key: &str,
    name: &str,
    headers: &HeaderMap,
) -> Response {
    let writer = state.writer.as_ref().expect("routed only with a writer");
    let if_match = header_text(headers, header::IF_MATCH).filter(|v| v != "*");
    let located = match locate(state, reader, key, name, None).await {
        Ok(located) => located,
        Err(response) => return *response,
    };
    let object_id = match located.object_id {
        Some(id) if located.here => id,
        // Moved to another calendar, as the second half of a move does.
        Some(id) => {
            info!(resource = name, key, object_id = %id, "caldav delete: the task is in another calendar now");
            return status(StatusCode::NO_CONTENT);
        }
        None => {
            info!(resource = name, key, "caldav delete: already gone");
            return status(StatusCode::NOT_FOUND);
        }
    };
    let current = match writer.get_task(&object_id).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            state.feed.invalidate();
            return status(StatusCode::NOT_FOUND);
        }
        Err(err) => return source_failure(&err),
    };
    let current_etag = state.feed.resource_for(&current).etag;
    if let Some(expected) = &if_match
        && expected != &current_etag
    {
        warn!(resource = name, %object_id, client_etag = %expected, server_etag = %current_etag, "caldav delete: stale etag");
        return precondition_failed("etag mismatch");
    }
    if let Some(member_id) = &located.member_id {
        let remaining: Vec<String> = current
            .assignees
            .iter()
            .filter(|id| *id != member_id)
            .cloned()
            .collect();
        info!(resource = name, %object_id, task = %current.name, %member_id, left = remaining.len(), "caldav delete: removing the assignee");
        let patch = writeback::Patch {
            assignees: Some(remaining),
            ..Default::default()
        };
        if let Err(err) = writer.update_task(&object_id, &patch).await {
            return source_failure(&err);
        }
    } else {
        info!(resource = name, %object_id, task = %current.name, "caldav delete: archiving task");
        if let Err(err) = writer.archive_task(&object_id).await {
            return source_failure(&err);
        }
    }
    state.feed.invalidate();
    status(StatusCode::NO_CONTENT)
}
