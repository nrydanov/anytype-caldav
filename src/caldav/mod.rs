//! The CalDAV facade over the space, shaped to what Calino asks for.
//!
//! The request set and the response shapes follow Calino 0.33.4 and the mock
//! server its end-to-end suite runs against (`e2e/fixtures/vite-caldav-mock.ts`):
//! requests are classified by path and, for REPORT, by the text of the body;
//! namespace prefixes are the lowercase `d:`, `c:`, `cs:`, `a:` that Calino's
//! own parsing expects in places.
//!
//! Layout, all under `/dav/`:
//!
//! - `/dav/`                        → `current-user-principal`
//! - `/dav/principal/`              → `calendar-home-set`
//! - `/dav/calendars/`              → the one collection
//! - `/dav/calendars/tasks/`        → collection properties, REPORT
//! - `/dav/calendars/tasks/<id>.ics` → one task
//! - `/dav/calendars/tasks-<key>/`  → the same, for one assignee's tasks
//! - `/dav/calendars/events/`       → events, when `caldav.events` is on
//! - `/dav/calendars/events/<id>.ics` → one event
//! - `/dav/calendars/calino-settings/` → the client's own documents, stored as they arrive
//!
//! Writes are accepted only when the service was given a `TaskWriter`; the
//! collection then advertises `write` and Calino allows editing. PUT and DELETE
//! check `If-Match`/`If-None-Match` against a fresh read of the task, never
//! against the cached snapshot, which may be up to `min_refresh_interval` old.

mod auth;
mod events;
mod home;
mod names;
mod paths;
mod settings;
mod tasks;
mod xml;

pub use auth::{Credentials, Reader};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::Response,
};
use tracing::{debug, info, warn};

use crate::{
    events as ev,
    feed::{Outcome, Snapshot, etag_for},
    http::AppState,
    source::SourceError,
};
use events::events_route;
use names::rename;
use paths::{is_calendar, normalize, tasks_key};
use settings::settings_route;
use tasks::tasks_route;
use xml::{multistatus, options, prop_href, prop_text, response, status};

pub const BASE: &str = "/dav/";

const PRINCIPAL: &str = "/dav/principal/";

const HOME: &str = "/dav/calendars/";

const TASKS: &str = "/dav/calendars/tasks/";

/// One calendar per assignee lives beside the flat one, under a key the
/// snapshot computed (`feed::collection_key`).
const TASKS_PREFIX: &str = "/dav/calendars/tasks-";

const EVENTS: &str = "/dav/calendars/events/";

/// Calino keeps its own settings in a calendar of its own, marked with a dead
/// property (`settingsSync.ts`, `CalDAVClient.discoverSettingsCalendar`). The
/// documents are stored as they arrive: they describe the calendar app, not
/// the task space, so nothing of them belongs in Anytype.
const SETTINGS: &str = "/dav/calendars/calino-settings/";

const REALM: &str = "anytype";

/// Where the names a reader gave the calendars are kept, one document per
/// collection path.
const NAMES: &str = "names/";

/// What the collections are called unless a reader renamed them.
#[derive(Debug, Clone)]
pub struct CalendarNames {
    pub tasks: String,
    pub events: String,
}

impl CalendarNames {
    pub fn for_language(language: crate::locale::Language) -> Self {
        Self {
            tasks: "Anytype".to_string(),
            events: language.events().to_string(),
        }
    }
}

impl Default for CalendarNames {
    fn default() -> Self {
        Self::for_language(crate::locale::Language::default())
    }
}

/// Every method on every `/dav/` path lands here.
pub async fn handle(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    let path = uri.path();
    let depth = headers
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();

    // OPTIONS carries no data; answering it unauthenticated lets a client
    // discover capabilities (and a browser preflight succeed).
    if method == Method::OPTIONS {
        return options();
    }

    let Some(credentials) = &state.caldav else {
        return status(StatusCode::NOT_FOUND);
    };
    let reader = credentials.reader(&state.feed, &headers).await;
    let Some(reader) = reader else {
        let presented = headers.contains_key(header::AUTHORIZATION);
        if presented {
            warn!(%method, %path, "caldav request with wrong credentials");
        } else {
            debug!(%method, %path, "caldav request without credentials");
        }
        // No challenge on GET/HEAD. Calino probes `/.well-known/caldav` with a
        // credential-less GET that nginx redirects here, and only reads the
        // final URL (`discovery.ts`, `probeWellKnownDirect`). A challenge on
        // that response makes the browser open its own sign-in dialog over
        // the app. Every DAV method still gets the challenge, which is how a
        // generic client learns to send Basic.
        let mut builder = Response::builder().status(StatusCode::UNAUTHORIZED);
        if method != Method::GET && method != Method::HEAD {
            builder = builder.header(
                header::WWW_AUTHENTICATE,
                format!("Basic realm=\"{REALM}\", charset=\"UTF-8\""),
            );
        }
        return builder.body(Body::empty()).expect("static response");
    };
    let settings_storage = reader.settings_storage();
    let names = reader.names_storage();

    let path = normalize(path);
    debug!(%method, path = %path, depth, body_bytes = body.len(), "caldav request");

    match (method.as_str(), path.as_str()) {
        // Thunderbird stops discovery at an answer without `resourcetype`.
        ("PROPFIND", "/dav/" | "/") => multistatus(vec![response(
            BASE,
            &[
                "<d:resourcetype><d:collection/></d:resourcetype>".to_string(),
                prop_href("d:current-user-principal", PRINCIPAL),
                prop_text("d:displayname", "Anytype"),
            ],
        )]),
        ("PROPFIND", PRINCIPAL) => multistatus(vec![response(
            PRINCIPAL,
            &[
                "<d:resourcetype><d:principal/></d:resourcetype>".to_string(),
                prop_href("c:calendar-home-set", HOME),
                prop_href("d:current-user-principal", PRINCIPAL),
            ],
        )]),
        ("PROPFIND", HOME) => home::home_set(&state, &reader, &names, &settings_storage).await,
        (_, _) if path.starts_with(SETTINGS) && state.documents.is_some() => {
            let store = state.documents.clone().expect("checked");
            settings_route(
                &store,
                &settings_storage,
                &method,
                &path,
                &depth,
                &headers,
                &body,
            )
        }
        ("PROPPATCH", _) if state.documents.is_some() && is_calendar(&path) => {
            let store = state.documents.clone().expect("checked");
            rename(&store, &names, &path, &body)
        }
        (_, _) if path.starts_with(EVENTS) && state.events.is_some() => {
            let service = state.events.clone().expect("checked");
            events_route(
                &state, &service, &names, &method, &path, &depth, &headers, &body,
            )
            .await
        }
        (_, _) if tasks_key(&path).is_some() => {
            tasks_route(
                &state, &reader, &names, &method, &path, &depth, &headers, &body,
            )
            .await
        }
        // A query at the base, principal or home set: not a calendar collection.
        // RFC 4791 wants 403 here; Calino's diagnostics read 404 as a
        // broken server and 403 as "point me at a calendar".
        ("REPORT", BASE | "/" | PRINCIPAL | HOME) => {
            debug!(path = %path, "caldav report outside a calendar collection");
            status(StatusCode::FORBIDDEN)
        }
        ("PUT" | "DELETE" | "PROPPATCH" | "MKCOL" | "MKCALENDAR" | "MOVE" | "COPY", _) => {
            info!(%method, path = %path, writable = state.writer.is_some(), "caldav write refused");
            status(StatusCode::FORBIDDEN)
        }
        _ => {
            debug!(%method, path = %path, "caldav path not found");
            status(StatusCode::NOT_FOUND)
        }
    }
}

fn unavailable(category: &str) -> Response {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::RETRY_AFTER, "30")
        .body(Body::from(category.to_string()))
        .expect("static response")
}

/// A failed Anytype call. Transport problems are 503 so Calino keeps the change
/// queued and retries; a schema problem will not heal by retrying.
fn source_failure(err: &SourceError) -> Response {
    match err {
        SourceError::Transport(_) | SourceError::TooManyObjects(_) => {
            unavailable("anytype unavailable")
        }
        SourceError::Schema(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
}

fn precondition_failed(reason: &str) -> Response {
    Response::builder()
        .status(StatusCode::PRECONDITION_FAILED)
        .body(Body::from(reason.to_string()))
        .expect("static response")
}

/// Runs `render` against the current snapshot, or reports why there is none.
/// A stale snapshot is served: a calendar client keeps working while Anytype
/// is briefly unreachable, exactly as the feed does.
async fn with_snapshot<F>(state: &AppState, render: F) -> Response
where
    F: FnOnce(&Snapshot) -> Response,
{
    match state.feed.get().await {
        Outcome::Fresh(snapshot) => render(&snapshot),
        Outcome::Stale(snapshot) => {
            warn!("caldav serving a stale snapshot");
            render(&snapshot)
        }
        Outcome::Unavailable { category } => {
            // 503, not 4xx: Calino retries a 5xx and drops a change on 4xx.
            warn!(category, "caldav has no snapshot to serve");
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(header::RETRY_AFTER, "30")
                .body(Body::from(category))
                .expect("static response")
        }
    }
}

/// What the flat collection declares and its ctag: tasks alone, or tasks and
/// the events merged into it, whose ctag then moves with either.
fn flat_contents(
    tasks_ctag: &str,
    events: Option<&ev::EventSnapshot>,
) -> (&'static [&'static str], String) {
    match events {
        Some(events) => (
            &["VTODO", "VEVENT"],
            etag_for(&format!("{tasks_ctag},{}", events.ctag)),
        ),
        None => (&["VTODO"], tasks_ctag.to_string()),
    }
}
