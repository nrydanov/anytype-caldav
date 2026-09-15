//! HTTP surface: the feed, its preflight, and liveness.

use std::{sync::Arc, time::SystemTime};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use tracing::{debug, error, info, warn};

use crate::{
    feed::{FeedService, Outcome, Snapshot},
    push::{BrowserSubscription, Notification, PushService},
};

const STALE_HEADER: &str = "x-exporter-stale";
const ALLOW_PRIVATE_NETWORK: &str = "access-control-allow-private-network";
const REQUEST_PRIVATE_NETWORK: &str = "access-control-request-private-network";

#[derive(Clone)]
pub struct AppState {
    pub feed: Arc<FeedService>,
    pub allowed_origins: Arc<Vec<String>>,
    pub push: Option<Arc<PushService>>,
    /// Set when the CalDAV facade is enabled.
    pub caldav: Option<Arc<crate::caldav::Credentials>>,
}

pub fn router(state: AppState, feed_path: &str) -> Router {
    let mut router = Router::new()
        .route(feed_path, get(todos).options(todos_preflight))
        .route("/healthz", get(healthz));

    // Push endpoints live under the feed's own secret prefix. On a public
    // deployment that prefix is the only thing protecting the feed, and an
    // open /push/subscribe would let anyone register to receive the reminders.
    if state.push.is_some() {
        let prefix = secret_prefix(feed_path);
        router = router
            .route(&format!("{prefix}/push/key"), get(push_key))
            .route(&format!("{prefix}/push/subscribe"), post(push_subscribe))
            .route(&format!("{prefix}/push/test"), post(push_test));
    }

    if state.caldav.is_some() {
        router = router
            .route("/dav", any(crate::caldav::handle))
            .route("/dav/", any(crate::caldav::handle))
            .route("/dav/{*rest}", any(crate::caldav::handle));
    }

    let prefix = secret_prefix(feed_path);
    router
        .with_state(state)
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let prefix = prefix.clone();
            async move { log_request(&prefix, request, next).await }
        }))
}

/// One line per request: enough to line a client's behaviour up with the
/// passes around it, never the secret part of the path.
async fn log_request(prefix: &str, request: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let method = request.method().clone();
    let path = redact_prefix(request.uri().path(), prefix);
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let user_agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.chars().take(120).collect::<String>());
    let conditional = request.headers().contains_key(header::IF_NONE_MATCH);

    let response = next.run(request).await;

    let status = response.status();
    let elapsed_ms = started.elapsed().as_millis();
    if status.is_server_error() {
        warn!(%method, %path, status = status.as_u16(), elapsed_ms, ?origin, ?user_agent, conditional, "http request");
    } else if path == "/healthz" {
        debug!(%method, %path, status = status.as_u16(), elapsed_ms, "http request");
    } else {
        info!(%method, %path, status = status.as_u16(), elapsed_ms, ?origin, ?user_agent, conditional, "http request");
    }
    response
}

/// `path` with the feed's secret prefix replaced, safe to log. The prefix is
/// the only thing protecting the feed on a public bind.
pub fn redact(path: &str, feed_path: &str) -> String {
    redact_prefix(path, &secret_prefix(feed_path))
}

fn redact_prefix(path: &str, prefix: &str) -> String {
    match path.strip_prefix(prefix) {
        Some(rest) if !prefix.is_empty() => format!("/[secret]{rest}"),
        _ => path.to_string(),
    }
}

/// Everything before the final path segment: `/f/<token>/todos.ics` yields
/// `/f/<token>`.
fn secret_prefix(feed_path: &str) -> String {
    match feed_path.rfind('/') {
        Some(0) | None => String::new(),
        Some(cut) => feed_path[..cut].to_string(),
    }
}

async fn push_key(State(state): State<AppState>) -> Response {
    match &state.push {
        Some(push) => Json(serde_json::json!({ "publicKey": push.public_key() })).into_response(),
        None => (StatusCode::NOT_FOUND, "push is disabled").into_response(),
    }
}

async fn push_subscribe(
    State(state): State<AppState>,
    Json(subscription): Json<BrowserSubscription>,
) -> Response {
    match &state.push {
        Some(push) => match push.store(subscription) {
            Ok(()) => (StatusCode::NO_CONTENT, "").into_response(),
            Err(err) => {
                error!(error = %err, "cannot store push subscription");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "cannot store subscription",
                )
                    .into_response()
            }
        },
        None => (StatusCode::NOT_FOUND, "push is disabled").into_response(),
    }
}

/// Proves the whole chain end to end without waiting for a real deadline.
async fn push_test(State(state): State<AppState>) -> Response {
    let Some(push) = &state.push else {
        return (StatusCode::NOT_FOUND, "push is disabled").into_response();
    };
    let notification = Notification {
        title: "Anytype".to_string(),
        body: "Тестовое уведомление от экспортёра".to_string(),
        url: None,
        tag: Some("test".to_string()),
    };
    let (delivered, attempted) = push.notify_all(&notification).await;
    Json(serde_json::json!({ "delivered": delivered, "subscriptions": attempted })).into_response()
}

/// Liveness only: deliberately independent of Anytype, so a probe reports on
/// this process rather than on a dependency it cannot influence.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn todos_preflight(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(axum::body::Body::empty())
        .expect("static response");

    apply_cors(&mut response, &state, &headers);

    // A conditional cross-origin GET is preflighted because If-None-Match is
    // not CORS-safelisted, so these must be answered or the feed cannot be
    // read from a browser at all.
    let out = response.headers_mut();
    out.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, OPTIONS"),
    );
    out.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("If-None-Match, If-Modified-Since"),
    );
    out.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );

    // Chrome treats a public page reaching 127.0.0.1 as a private-network
    // request and requires this acknowledgement on the preflight.
    if headers.contains_key(REQUEST_PRIVATE_NETWORK) {
        out.insert(
            HeaderName::from_static(ALLOW_PRIVATE_NETWORK),
            HeaderValue::from_static("true"),
        );
    }

    response
}

async fn todos(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let outcome = state.feed.get().await;

    let mut response = match &outcome {
        Outcome::Fresh(snapshot) => {
            if not_modified(&headers, snapshot) {
                debug!("client validator matched; returning 304");
                empty(StatusCode::NOT_MODIFIED)
            } else {
                feed_response(snapshot, false)
            }
        }
        // A stale body is always sent in full, even against a matching
        // validator: the point is to make fallback visible rather than to let
        // it hide behind an intermediary cache.
        Outcome::Stale(snapshot) => feed_response(snapshot, true),
        Outcome::Unavailable { category } => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("feed unavailable: {category}\n"),
        )
            .into_response(),
    };

    apply_cors(&mut response, &state, &headers);
    response
}

fn feed_response(snapshot: &Snapshot, stale: bool) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(axum::body::Body::from(snapshot.body.to_string()))
        .expect("feed response");

    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/calendar; charset=utf-8"),
    );
    out.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline; filename=\"todos.ics\""),
    );
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(value) = HeaderValue::from_str(&snapshot.etag) {
        out.insert(header::ETAG, value);
    }
    if let Ok(value) = HeaderValue::from_str(&httpdate::fmt_http_date(SystemTime::from(
        snapshot.last_modified,
    ))) {
        out.insert(header::LAST_MODIFIED, value);
    }
    if stale {
        out.insert(
            HeaderName::from_static(STALE_HEADER),
            HeaderValue::from_static("true"),
        );
    }
    response
}

fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("static response")
}

/// Honours both validators, because clients revalidate with whichever they
/// happen to implement.
fn not_modified(headers: &HeaderMap, snapshot: &Snapshot) -> bool {
    if let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        return if_none_match
            .split(',')
            .map(str::trim)
            .any(|candidate| candidate == snapshot.etag || candidate == "*");
    }

    if let Some(since) = headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| httpdate::parse_http_date(v).ok())
    {
        // HTTP-date has one-second granularity, so compare truncated values.
        let modified = SystemTime::from(snapshot.last_modified);
        return modified
            .duration_since(since)
            .map(|delta| delta.as_secs() == 0)
            .unwrap_or(true);
    }

    false
}

/// Echoes only a configured origin. A wildcard is refused at config load: on an
/// unauthenticated loopback feed it would let any page the user visits read the
/// entire task list, which the loopback bind alone would otherwise prevent.
fn apply_cors(response: &mut Response, state: &AppState, request_headers: &HeaderMap) {
    let Some(origin) = request_headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return;
    };
    if !state
        .allowed_origins
        .iter()
        .any(|allowed| allowed == origin)
    {
        debug!(
            origin,
            "origin not in allowed_origins; omitting CORS headers"
        );
        return;
    }

    let out = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(origin) {
        out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    // Without this a script can read neither the ETag nor the stale flag:
    // only a short default set of response headers is exposed to JavaScript.
    out.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("ETag, X-Exporter-Stale"),
    );
    out.insert(header::VARY, HeaderValue::from_static("Origin"));
}

#[cfg(test)]
mod redaction_tests {
    #[test]
    fn the_secret_prefix_never_reaches_the_log() {
        let feed = "/f/0123456789abcdef/todos.ics";
        assert_eq!(super::redact(feed, feed), "/[secret]/todos.ics");
        assert_eq!(
            super::redact("/f/0123456789abcdef/push/subscribe", feed),
            "/[secret]/push/subscribe"
        );
        assert_eq!(super::redact("/healthz", feed), "/healthz");
        for line in [
            super::redact(feed, feed),
            super::redact("/f/0123456789abcdef/push/key", feed),
        ] {
            assert!(!line.contains("0123456789abcdef"), "{line}");
        }
    }

    #[test]
    fn a_root_feed_path_has_nothing_to_hide() {
        assert_eq!(super::redact("/todos.ics", "/todos.ics"), "/todos.ics");
    }
}
