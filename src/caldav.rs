//! A read-only CalDAV view of the feed, shaped to what Calino asks for.
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

use std::{borrow::Cow, collections::BTreeMap};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::Response,
};
use base64::Engine;
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};

use crate::{
    events::{self as ev, EventService, EventWriteError},
    feed::{Outcome, Resource, Snapshot, calino_filename, collection_ctag, etag_for},
    http::AppState,
    source::SourceError,
    state::StateStore,
    writeback::{self, WriteError},
};

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
const SETTINGS_NAMESPACE: &str = "http://calino.app/ns/";
const SETTINGS_DISPLAY_NAME: &str = "Calino Settings";
/// A settings document is a few kilobytes; this only stops a runaway client.
const MAX_DOCUMENT_BYTES: usize = 512 * 1024;
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

impl Default for CalendarNames {
    fn default() -> Self {
        Self {
            tasks: "Anytype".to_string(),
            events: "События".to_string(),
        }
    }
}

/// Who may use the facade. The password is only ever held as a digest.
pub struct Credentials {
    username: String,
    password_digest: [u8; 32],
    /// Set when every person of the space has an account of their own.
    accounts: Option<crate::accounts::Accounts>,
}

/// Who a request was made by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reader {
    /// The one configured username and password: nobody in particular.
    Shared,
    /// A person of the space, by the id their calendar is kept under.
    Person(String),
}

impl Reader {
    /// Where this reader's client documents are kept. A person's are their
    /// own: Calino syncs its settings as one document per account, so people
    /// sharing a collection would overwrite each other's.
    fn settings_storage(&self) -> String {
        match self {
            Reader::Shared => SETTINGS.to_string(),
            Reader::Person(id) => format!("{SETTINGS}{}/", crate::accounts::username(id)),
        }
    }

    /// Where this reader's names for the calendars are kept. Two instances,
    /// or two people, see the same collections under names of their own.
    fn names_storage(&self) -> String {
        match self {
            Reader::Shared => NAMES.to_string(),
            Reader::Person(id) => format!("{NAMES}{}/", crate::accounts::username(id)),
        }
    }
}

impl Credentials {
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            username: username.to_string(),
            password_digest: Sha256::digest(password.as_bytes()).into(),
            accounts: None,
        }
    }

    /// Also accepts the account of every person who may sign in, derived from
    /// this secret (`accounts`).
    pub fn with_accounts(mut self, secret: &str) -> Self {
        self.accounts = Some(crate::accounts::Accounts::new(secret));
        self
    }

    /// Who signed in, if anyone. A person is looked up among the calendars of
    /// the current snapshot, so an account exists exactly as long as the
    /// directory says it does.
    pub async fn reader(
        &self,
        feed: &crate::feed::FeedService,
        headers: &HeaderMap,
    ) -> Option<Reader> {
        if self.accepts(headers) {
            return Some(Reader::Shared);
        }
        let accounts = self.accounts.as_ref()?;
        let (username, password) = basic(headers)?;
        let (Outcome::Fresh(snapshot) | Outcome::Stale(snapshot)) = feed.get().await else {
            return None;
        };
        let people = snapshot
            .collections
            .values()
            .filter(|collection| collection.account)
            .filter_map(|collection| collection.member_id.as_deref());
        accounts
            .person(&username, &password, people)
            .map(|id| Reader::Person(id.to_string()))
    }

    /// Checks an `Authorization: Basic …` header. The password comparison
    /// runs over fixed-length digests so its duration does not depend on how
    /// much of the password matched.
    pub fn accepts(&self, headers: &HeaderMap) -> bool {
        let Some((user, password)) = basic(headers) else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(password.as_bytes()).into();
        let same_password = digest
            .iter()
            .zip(self.password_digest.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
        same_password & (user == self.username)
    }
}

/// The username and password of an `Authorization: Basic …` header.
fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_string(), password.to_string()))
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
        ("PROPFIND", HOME) => {
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
                            &named(&state, &names, EVENTS, &state.calendar_names.events),
                            &snapshot.ctag,
                            writable,
                        ),
                    )
                });
            let settings = state.documents.as_ref().map(|store| {
                response(
                    SETTINGS,
                    &settings_collection_props(store, &settings_storage),
                )
            });
            with_snapshot(&state, |snapshot| {
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
                            &named(&state, &names, TASKS, &state.calendar_names.tasks),
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
                            &state,
                            &names,
                            &collection_path(key),
                            &collection.display_name,
                        ),
                        &collection_view(snapshot, key, &reader)
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

/// Every request under `tasks/` and under a `tasks-<key>/` of its own. The two
/// differ only in which resources they list: a member GET still looks the name
/// up in the flat index, because the same task is served under the same name in
/// every collection it belongs to.
#[allow(clippy::too_many_arguments)]
async fn tasks_route(
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
struct View<'a> {
    /// A person's name, or `None` for the flat collection, whose name is configured.
    display_name: Option<&'a str>,
    ctag: Cow<'a, str>,
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
fn collection_view<'a>(snapshot: &'a Snapshot, key: &str, reader: &Reader) -> Option<View<'a>> {
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

async fn put(
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

async fn create(
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
async fn written(state: &AppState, object_id: &str, code: StatusCode) -> Response {
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
async fn delete(
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

// ---------------------------------------------------------------- settings

fn settings_collection_props(store: &StateStore, storage: &str) -> Vec<String> {
    let ctag = store
        .documents(storage)
        .map(|documents| {
            etag_for(
                &documents
                    .iter()
                    .map(|(name, body)| format!("{name}:{body}"))
                    .collect::<Vec<_>>()
                    .join(","),
            )
        })
        .unwrap_or_else(|_| "\"unknown\"".to_string());
    let mut props = collection_props(&["VEVENT"], SETTINGS_DISPLAY_NAME, &ctag, true);
    // The marker Calino looks for; without it the client makes a calendar of
    // its own, which this server does not allow.
    props.push(format!(
        "<C:X-CALINO-SETTINGS-CALENDAR xmlns:C=\"{SETTINGS_NAMESPACE}\">1</C:X-CALINO-SETTINGS-CALENDAR>"
    ));
    props
}

fn document_name(path: &str) -> Option<&str> {
    resource_name_in(path, SETTINGS)
}

/// `storage` is where this reader's documents are kept (`Reader::settings_storage`);
/// the path a client sees is the same for everyone.
fn settings_route(
    store: &StateStore,
    storage: &str,
    method: &Method,
    path: &str,
    depth: &str,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    let name = document_name(path).map(str::to_string);
    let documents = |store: &StateStore| store.documents(storage).unwrap_or_default();
    match (method.as_str(), path, name) {
        ("PROPFIND", SETTINGS, _) => {
            let mut responses = vec![response(
                SETTINGS,
                &settings_collection_props(store, storage),
            )];
            if depth == "1" {
                responses.extend(documents(store).into_iter().map(|(name, body)| {
                    response(
                        &format!("{SETTINGS}{name}.ics"),
                        &[prop_text("d:getetag", &etag_for(&body))],
                    )
                }));
            }
            multistatus(responses)
        }
        // Calino filters by UID; every document of this collection is its own,
        // so the filter needs no reading.
        ("REPORT", SETTINGS, _) => multistatus(
            documents(store)
                .into_iter()
                .map(|(name, body)| {
                    response(
                        &format!("{SETTINGS}{name}.ics"),
                        &[
                            prop_text("d:getetag", &etag_for(&body)),
                            prop_text("d:getcontenttype", "text/calendar"),
                            prop_text("c:calendar-data", &body),
                        ],
                    )
                })
                .collect(),
        ),
        // The collection is already marked; a client setting properties on it
        // is told the write went nowhere rather than that it failed.
        ("PROPPATCH", SETTINGS, _) => multistatus(vec![response(SETTINGS, &[])]),
        ("GET" | "HEAD" | "PROPFIND", _, Some(name)) => {
            let Some(body) = store.document(storage, &name).ok().flatten() else {
                debug!(resource = %name, "caldav settings document not found");
                return status(StatusCode::NOT_FOUND);
            };
            let etag = etag_for(&body);
            if method.as_str() == "PROPFIND" {
                return multistatus(vec![response(
                    &format!("{SETTINGS}{name}.ics"),
                    &[prop_text("d:getetag", &etag)],
                )]);
            }
            let payload = if method == Method::HEAD {
                Body::empty()
            } else {
                Body::from(body)
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
                .header(header::ETAG, etag)
                .body(payload)
                .expect("valid response")
        }
        ("PUT", _, Some(name)) => {
            if body.len() > MAX_DOCUMENT_BYTES {
                warn!(resource = %name, bytes = body.len(), "caldav settings document too large");
                return status(StatusCode::PAYLOAD_TOO_LARGE);
            }
            let current = store.document(storage, &name).ok().flatten();
            let if_match = header_text(headers, header::IF_MATCH).filter(|v| v != "*");
            let if_none_match = header_text(headers, header::IF_NONE_MATCH);
            if if_none_match.as_deref() == Some("*") && current.is_some() {
                return precondition_failed("resource exists");
            }
            if let Some(expected) = &if_match {
                let now = current.as_deref().map(etag_for);
                if now.as_ref() != Some(expected) {
                    warn!(resource = %name, client_etag = %expected, server_etag = ?now, "caldav settings put: stale etag");
                    return precondition_failed("etag mismatch");
                }
            }
            if let Err(err) = store.put_document(storage, &name, body) {
                error!(resource = %name, error = %err, "caldav settings put failed");
                return status(StatusCode::INTERNAL_SERVER_ERROR);
            }
            info!(resource = %name, bytes = body.len(), created = current.is_none(), "caldav settings document written");
            let code = if current.is_some() {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            };
            Response::builder()
                .status(code)
                .header(header::ETAG, etag_for(body))
                .body(Body::empty())
                .expect("valid response")
        }
        ("DELETE", _, Some(name)) => match store.delete_document(storage, &name) {
            Ok(true) => {
                info!(resource = %name, "caldav settings document deleted");
                status(StatusCode::NO_CONTENT)
            }
            Ok(false) => status(StatusCode::NOT_FOUND),
            Err(err) => {
                error!(resource = %name, error = %err, "caldav settings delete failed");
                status(StatusCode::INTERNAL_SERVER_ERROR)
            }
        },
        _ => {
            debug!(%method, path, "caldav settings path not found");
            status(StatusCode::NOT_FOUND)
        }
    }
}

// ------------------------------------------------------------------ events

/// Every request under `events/`. Mirrors the task collection: the snapshot
/// serves reads, a fresh read of the object decides write preconditions.
#[allow(clippy::too_many_arguments)]
async fn events_route(
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

fn event_href(name: &str) -> String {
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
    let current_etag = ev::deadline_entry(&current)
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
        && let Some(resource) = ev::deadline_entry(&event).and_then(|e| service.resource(&e, &[]))
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

#[derive(Debug, PartialEq)]
enum Report {
    Tasks,
    TasksOnly,
    EventsOnly,
    SyncCollection,
}

/// Classifies a REPORT body by its text, as Calino's mock does. Prefixes are
/// free in XML, so only local names and attribute values are looked at.
fn report_kind(body: &str) -> Report {
    if body.contains("sync-collection") {
        return Report::SyncCollection;
    }
    let asks_events = body.contains("\"VEVENT\"") || body.contains("'VEVENT'");
    let asks_tasks = body.contains("\"VTODO\"") || body.contains("'VTODO'");
    if asks_events && !asks_tasks {
        Report::EventsOnly
    } else if asks_tasks && !asks_events {
        Report::TasksOnly
    } else {
        Report::Tasks
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

fn collection_props(components: &[&str], name: &str, ctag: &str, writable: bool) -> Vec<String> {
    let components: String = components
        .iter()
        .map(|component| format!("<c:comp name=\"{component}\"/>"))
        .collect();
    vec![
        "<d:resourcetype><d:collection/><c:calendar/></d:resourcetype>".to_string(),
        prop_text("d:displayname", name),
        format!(
            "<c:supported-calendar-component-set>{components}</c:supported-calendar-component-set>"
        ),
        // Changes whenever any member changes, so Calino can skip an unchanged
        // collection without listing it.
        prop_text("cs:getctag", ctag),
        if writable {
            "<d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege><d:privilege><d:write-content/></d:privilege><d:privilege><d:bind/></d:privilege><d:privilege><d:unbind/></d:privilege></d:current-user-privilege-set>".to_string()
        } else {
            "<d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set>".to_string()
        },
    ]
}

/// The task collection a path addresses: the key that follows `tasks-`, or the
/// empty string for the flat one. A key is a digest or the reserved
/// `unassigned`, so anything else is not a task path at all.
fn tasks_key(path: &str) -> Option<&str> {
    if path.starts_with(TASKS) {
        return Some("");
    }
    let (key, _) = path.strip_prefix(TASKS_PREFIX)?.split_once('/')?;
    key.chars()
        .all(|c| c.is_ascii_alphanumeric())
        .then_some(key)
}

/// The name a reader gave a calendar, or the one the server gives it.
fn named(state: &AppState, names: &str, path: &str, default: &str) -> String {
    state
        .documents
        .as_ref()
        .and_then(|store| store.document(names, path).ok().flatten())
        .unwrap_or_else(|| default.to_string())
}

/// A task or event collection, which a reader may rename.
fn is_calendar(path: &str) -> bool {
    path == EVENTS || tasks_key(path).is_some_and(|key| collection_path(key) == path)
}

/// Keeps the name a reader gave a calendar. Calino renames with a PROPPATCH
/// of `displayname` alone and reads the name back on every sync, so a name the
/// server does not keep is gone by the next one. An empty name, or a
/// `displayname` under `remove`, returns the calendar to its own name.
fn rename(store: &StateStore, names: &str, path: &str, body: &str) -> Response {
    let Some(name) = display_name(body) else {
        info!(path, "caldav proppatch without a displayname refused");
        return status(StatusCode::FORBIDDEN);
    };
    let name = name.trim();
    let stored = if name.is_empty() {
        store.delete_document(names, path).map(|_| ())
    } else {
        store.put_document(names, path, name)
    };
    if let Err(err) = stored {
        warn!(path, %err, "caldav rename not stored");
        return status(StatusCode::INTERNAL_SERVER_ERROR);
    }
    info!(path, name, "caldav calendar renamed");
    multistatus(vec![response(path, &["<d:displayname/>".to_string()])])
}

/// The text of the first `displayname` element in a request body, whatever
/// its namespace prefix; empty for `<displayname/>`.
fn display_name(body: &str) -> Option<String> {
    let mut rest = body;
    loop {
        let at = rest.find("displayname")?;
        let before = rest[..at].chars().next_back();
        let after = &rest[at + "displayname".len()..];
        rest = after;
        if !matches!(before, Some('<' | ':')) || !after.starts_with(['>', ' ', '/']) {
            continue;
        }
        let open_end = after.find('>')?;
        if after[..open_end].ends_with('/') {
            return Some(String::new());
        }
        let text = &after[open_end + 1..];
        let text = &text[..text.find('<')?];
        return Some(unescape(text));
    }
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn collection_path(key: &str) -> String {
    if key.is_empty() {
        TASKS.to_string()
    } else {
        format!("{TASKS_PREFIX}{key}/")
    }
}

/// `/dav/calendars/tasks/<name>.ics` → `("", <name>)`. A name is an object id
/// or a name Calino derived from a UID, so only `[A-Za-z0-9._~-]` is accepted;
/// `..` and anything path-like is refused rather than looked up.
fn object_id(path: &str) -> Option<(&str, &str)> {
    let key = tasks_key(path)?;
    let name = resource_name_in(path, &collection_path(key))?;
    Some((key, name))
}

fn resource_name_in<'a>(path: &'a str, collection: &str) -> Option<&'a str> {
    let name = path.strip_prefix(collection)?.strip_suffix(".ics")?;
    let plain = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'));
    (!name.is_empty() && plain && !name.contains("..")).then_some(name)
}

fn href_for(key: &str, object_id: &str) -> String {
    format!("{}{object_id}.ics", collection_path(key))
}

/// Collections are addressed with a trailing slash; accept them without one.
fn normalize(path: &str) -> String {
    match path {
        "/dav"
        | "/dav/principal"
        | "/dav/calendars"
        | "/dav/calendars/events"
        | "/dav/calendars/calino-settings" => format!("{path}/"),
        // There is one task collection per assignee, so they cannot be listed.
        other
            if other
                .strip_prefix("/dav/calendars/tasks")
                .is_some_and(|rest| !rest.contains('/')) =>
        {
            format!("{other}/")
        }
        other => other.to_string(),
    }
}

fn response(href: &str, props: &[String]) -> String {
    format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop>{}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        escape(href),
        props.concat()
    )
}

/// What a PROPFIND tells of a task or an event. Thunderbird fetches only the
/// members listed as `text/calendar` (`CalDavRequestHandlers.sys.mjs`).
fn member_props(etag: &str) -> [String; 2] {
    [
        prop_text("d:getetag", etag),
        prop_text("d:getcontenttype", "text/calendar"),
    ]
}

fn prop_text(name: &str, value: &str) -> String {
    format!("<{name}>{}</{name}>", escape(value))
}

fn prop_href(name: &str, href: &str) -> String {
    format!("<{name}><d:href>{}</d:href></{name}>", escape(href))
}

fn multistatus(responses: Vec<String>) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" xmlns:cs=\"http://calendarserver.org/ns/\" xmlns:a=\"http://apple.com/ns/ical/\">{}</d:multistatus>",
        responses.concat()
    );
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(body))
        .expect("valid response")
}

fn options() -> Response {
    let mut response = status(StatusCode::OK);
    let out = response.headers_mut();
    out.insert(
        header::ALLOW,
        HeaderValue::from_static("OPTIONS, GET, HEAD, PROPFIND, REPORT, PUT, DELETE"),
    );
    out.insert("DAV", HeaderValue::from_static("1, 3, calendar-access"));
    response
}

fn status(code: StatusCode) -> Response {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .expect("static response")
}

/// XML text escaping. `calendar-data` must be escaped, not raw: a SUMMARY
/// with `&` or `<` would otherwise break the document for the whole listing.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(user: &str, password: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {token}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn only_the_exact_credentials_are_accepted() {
        let credentials = Credentials::new("me", "s3cret:with:colons");
        assert!(credentials.accepts(&basic("me", "s3cret:with:colons")));
        assert!(!credentials.accepts(&basic("me", "s3cret")));
        assert!(!credentials.accepts(&basic("you", "s3cret:with:colons")));
        assert!(!credentials.accepts(&HeaderMap::new()));
    }

    /// Calino's own bodies, from `CalDAVClient.ts` and tsdav's calendar-query.
    #[test]
    fn reports_are_classified_by_their_text() {
        let todo = r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VTODO"/></c:comp-filter></c:filter></c:calendar-query>"#;
        let event = todo.replace("VTODO", "VEVENT");
        let sync = r#"<D:sync-collection xmlns:D="DAV:"><D:sync-token/><D:sync-level>1</D:sync-level></D:sync-collection>"#;
        assert_eq!(report_kind(todo), Report::TasksOnly);
        assert_eq!(report_kind("<c:calendar-query/>"), Report::Tasks);
        assert_eq!(report_kind(&event), Report::EventsOnly);
        assert_eq!(report_kind(sync), Report::SyncCollection);
    }

    #[test]
    fn only_plain_object_ids_are_resources() {
        assert_eq!(
            object_id("/dav/calendars/tasks/bafyreiabc123.ics"),
            Some(("", "bafyreiabc123"))
        );
        assert_eq!(object_id("/dav/calendars/tasks/../x.ics"), None);
        assert_eq!(
            object_id("/dav/calendars/tasks/0b9a4f2c-5d1e-4c3a-9f7e-2a6b8c1d0e3f.ics"),
            Some(("", "0b9a4f2c-5d1e-4c3a-9f7e-2a6b8c1d0e3f"))
        );
        assert_eq!(object_id("/dav/calendars/tasks/.ics"), None);
        assert_eq!(object_id("/dav/calendars/tasks/abc"), None);
    }

    #[test]
    fn a_grouped_collection_carries_its_key() {
        assert_eq!(tasks_key("/dav/calendars/tasks/"), Some(""));
        assert_eq!(
            tasks_key("/dav/calendars/tasks-a1b2c3d4e5f6/"),
            Some("a1b2c3d4e5f6")
        );
        assert_eq!(
            tasks_key("/dav/calendars/tasks-unassigned/"),
            Some("unassigned")
        );
        assert_eq!(tasks_key("/dav/calendars/tasks-a1b2"), None);
        assert_eq!(tasks_key("/dav/calendars/events/"), None);
        // A key with a path behind it names no collection and no resource, so
        // both the collection arms and the member arms miss it.
        assert_eq!(object_id("/dav/calendars/tasks-a1b2/../x.ics"), None);
        assert_eq!(
            object_id("/dav/calendars/tasks-unassigned/bafyreiabc123.ics"),
            Some(("unassigned", "bafyreiabc123"))
        );
    }

    #[test]
    fn calendar_data_is_escaped() {
        let prop = prop_text("c:calendar-data", "SUMMARY:Tom & Jerry <3");
        assert_eq!(
            prop,
            "<c:calendar-data>SUMMARY:Tom &amp; Jerry &lt;3</c:calendar-data>"
        );
    }
}
