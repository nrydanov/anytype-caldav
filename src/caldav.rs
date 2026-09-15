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
//!
//! Read-only for now: the collection advertises only the `read` privilege, so
//! Calino shows the calendar as read-only and never attempts a write.

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::Response,
};
use base64::Engine;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::{
    feed::{Outcome, Snapshot},
    http::AppState,
};

pub const BASE: &str = "/dav/";
const PRINCIPAL: &str = "/dav/principal/";
const HOME: &str = "/dav/calendars/";
const TASKS: &str = "/dav/calendars/tasks/";
const REALM: &str = "anytype";

/// Who may use the facade. The password is only ever held as a digest.
pub struct Credentials {
    username: String,
    password_digest: [u8; 32],
}

impl Credentials {
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            username: username.to_string(),
            password_digest: Sha256::digest(password.as_bytes()).into(),
        }
    }

    /// Checks an `Authorization: Basic …` header. The password comparison
    /// runs over fixed-length digests so its duration does not depend on how
    /// much of the password matched.
    fn accepts(&self, headers: &HeaderMap) -> bool {
        let Some(encoded) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
        else {
            return false;
        };
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
            return false;
        };
        let Ok(text) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((user, password)) = text.split_once(':') else {
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
    if !credentials.accepts(&headers) {
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
    }

    let path = normalize(path);
    debug!(%method, path = %path, depth, body_bytes = body.len(), "caldav request");

    match (method.as_str(), path.as_str()) {
        ("PROPFIND", "/dav/" | "/") => multistatus(vec![response(
            BASE,
            &[
                prop_href("d:current-user-principal", PRINCIPAL),
                prop_text("d:displayname", "Anytype"),
            ],
        )]),
        ("PROPFIND", PRINCIPAL) => multistatus(vec![response(
            PRINCIPAL,
            &[
                prop_href("c:calendar-home-set", HOME),
                prop_href("d:current-user-principal", PRINCIPAL),
            ],
        )]),
        ("PROPFIND", HOME) => {
            with_snapshot(&state, |snapshot| {
                multistatus(vec![response(TASKS, &collection_props(snapshot))])
            })
            .await
        }
        ("PROPFIND", TASKS) => {
            with_snapshot(&state, |snapshot| {
                let mut responses = vec![response(TASKS, &collection_props(snapshot))];
                // Depth 1 also lists members with their ETags, which is how a
                // generic client finds out what changed without a REPORT.
                if depth == "1" {
                    responses.extend(snapshot.objects.iter().map(|(id, resource)| {
                        response(&href_for(id), &[prop_text("d:getetag", &resource.etag)])
                    }));
                }
                multistatus(responses)
            })
            .await
        }
        ("REPORT", TASKS) => {
            let kind = report_kind(&body);
            with_snapshot(&state, |snapshot| match kind {
                Report::SyncCollection => {
                    // Never advertised, so Calino does not send it; a 403
                    // makes any client fall back to a full listing.
                    info!("caldav sync-collection requested but not supported");
                    status(StatusCode::FORBIDDEN)
                }
                Report::EventsOnly => {
                    debug!("caldav calendar-query for events: none");
                    multistatus(Vec::new())
                }
                Report::Tasks => {
                    debug!(
                        resources = snapshot.objects.len(),
                        "caldav calendar-query for tasks"
                    );
                    multistatus(
                        snapshot
                            .objects
                            .iter()
                            .map(|(id, resource)| {
                                response(
                                    &href_for(id),
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
        ("PROPFIND" | "GET" | "HEAD", _) if object_id(&path).is_some() => {
            let id = object_id(&path).expect("checked").to_string();
            let is_get = method.as_str() != "PROPFIND";
            let head = method == Method::HEAD;
            with_snapshot(&state, move |snapshot| {
                let Some(resource) = snapshot.objects.get(&id) else {
                    debug!(object_id = %id, "caldav resource not found");
                    return status(StatusCode::NOT_FOUND);
                };
                if !is_get {
                    return multistatus(vec![response(
                        &href_for(&id),
                        &[prop_text("d:getetag", &resource.etag)],
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
        ("PUT" | "DELETE" | "PROPPATCH" | "MKCOL" | "MKCALENDAR" | "MOVE" | "COPY", _) => {
            info!(%method, path = %path, "caldav write refused: the facade is read-only");
            status(StatusCode::FORBIDDEN)
        }
        _ => {
            debug!(%method, path = %path, "caldav path not found");
            status(StatusCode::NOT_FOUND)
        }
    }
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
    } else {
        Report::Tasks
    }
}

fn collection_props(snapshot: &Snapshot) -> Vec<String> {
    vec![
        "<d:resourcetype><d:collection/><c:calendar/></d:resourcetype>".to_string(),
        prop_text("d:displayname", "Anytype"),
        "<c:supported-calendar-component-set><c:comp name=\"VTODO\"/></c:supported-calendar-component-set>".to_string(),
        // Changes whenever any task changes, so Calino can skip an unchanged
        // collection without listing it.
        prop_text("cs:getctag", &snapshot.etag),
        "<d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set>".to_string(),
    ]
}

/// `/dav/calendars/tasks/<id>.ics` → `<id>`. Object ids are base32 CIDs, so
/// anything outside `[a-z0-9]` is refused rather than looked up.
fn object_id(path: &str) -> Option<&str> {
    let name = path.strip_prefix(TASKS)?.strip_suffix(".ics")?;
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric())).then_some(name)
}

fn href_for(object_id: &str) -> String {
    format!("{TASKS}{object_id}.ics")
}

/// Collections are addressed with a trailing slash; accept them without one.
fn normalize(path: &str) -> String {
    match path {
        "/dav" | "/dav/principal" | "/dav/calendars" | "/dav/calendars/tasks" => format!("{path}/"),
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
        HeaderValue::from_static("OPTIONS, GET, HEAD, PROPFIND, REPORT"),
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
        assert_eq!(report_kind(todo), Report::Tasks);
        assert_eq!(report_kind(&event), Report::EventsOnly);
        assert_eq!(report_kind(sync), Report::SyncCollection);
    }

    #[test]
    fn only_plain_object_ids_are_resources() {
        assert_eq!(
            object_id("/dav/calendars/tasks/bafyreiabc123.ics"),
            Some("bafyreiabc123")
        );
        assert_eq!(object_id("/dav/calendars/tasks/../x.ics"), None);
        assert_eq!(object_id("/dav/calendars/tasks/.ics"), None);
        assert_eq!(object_id("/dav/calendars/tasks/abc"), None);
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
