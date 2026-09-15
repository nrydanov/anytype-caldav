//! Feed coordination and HTTP behaviour, driven through a scripted source.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anytype_task_exporter::{
    config::{CalendarConfig, RemindersConfig},
    feed::FeedService,
    http::{self, AppState},
    model::{AnytypeDate, Task, TaskBatch},
    render::VTodoRenderer,
    source::{SourceError, TaskSource},
};
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::{TimeZone, Utc};
use chrono_tz::Europe::Saratov;
use http_body_util::BodyExt;
use tower::ServiceExt;

// ------------------------------------------------------------- scripted source

enum Step {
    Ok(Vec<Task>),
    Fail,
}

struct ScriptedSource {
    steps: Mutex<Vec<Step>>,
    calls: AtomicUsize,
    delay: Duration,
}

impl ScriptedSource {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
        })
    }

    fn with_delay(steps: Vec<Step>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps),
            calls: AtomicUsize::new(0),
            delay,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TaskSource for ScriptedSource {
    async fn list_tasks(&self) -> Result<TaskBatch, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        // The last step repeats once the script runs out.
        let mut steps = self.steps.lock().unwrap();
        let step = if steps.len() > 1 {
            steps.remove(0)
        } else {
            match steps.first() {
                Some(Step::Ok(tasks)) => Step::Ok(tasks.clone()),
                Some(Step::Fail) => Step::Fail,
                None => Step::Ok(Vec::new()),
            }
        };
        match step {
            Step::Ok(tasks) => Ok(TaskBatch {
                tasks,
                warnings: Vec::new(),
            }),
            Step::Fail => Err(SourceError::Transport("scripted failure".into())),
        }
    }
}

// ------------------------------------------------------------------- fixtures

fn task(id: &str, name: &str) -> Task {
    Task {
        object_id: id.into(),
        name: name.into(),
        scheduled: AnytypeDate::parse("2026-08-29T00:00:00+04:00"),
        deadline: None,
        done: false,
        reminder_leads: Vec::new(),
        tags: Vec::new(),
        ical_uid: None,
        object_url: Some(format!("anytype://object?objectId={id}")),
        last_modified: Some(Utc.with_ymd_and_hms(2026, 8, 29, 12, 0, 0).unwrap()),
    }
}

const FEED_PATH: &str = "/todos.ics";

fn build(
    source: Arc<dyn TaskSource>,
    min_refresh: Duration,
    origins: Vec<String>,
) -> (Router, Arc<FeedService>) {
    build_at(source, min_refresh, origins, FEED_PATH)
}

fn build_at(
    source: Arc<dyn TaskSource>,
    min_refresh: Duration,
    origins: Vec<String>,
    feed_path: &str,
) -> (Router, Arc<FeedService>) {
    let renderer = VTodoRenderer::new(
        CalendarConfig {
            timezone: Saratov,
            name: "Anytype Tasks".into(),
            date_only_timezone: Saratov,
        },
        RemindersConfig {
            enabled: true,
            lead_time: chrono::Duration::minutes(30),
            all_day_time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        },
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
    );
    let feed = Arc::new(FeedService::new(
        source,
        renderer,
        min_refresh,
        Duration::from_secs(10),
    ));
    let state = AppState {
        feed: feed.clone(),
        allowed_origins: Arc::new(origins),
        push: None,
        caldav: None,
        writer: None,
        events: None,
    };
    (http::router(state, feed_path), feed)
}

async fn get(router: &Router, uri: &str) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_string(response: axum::http::Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------------- tests

#[tokio::test(start_paused = true)]
async fn a_successful_request_serves_the_feed_with_the_required_headers() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    let response = get(&router, "/todos.ics").await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "text/calendar; charset=utf-8"
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert!(headers.contains_key(header::ETAG));
    assert!(headers.contains_key(header::LAST_MODIFIED));
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .contains("todos.ics")
    );
    assert!(!headers.contains_key("x-exporter-stale"));

    let body = body_string(response).await;
    assert!(body.contains("BEGIN:VTODO"));
    assert!(body.contains("SUMMARY:One"));
}

#[tokio::test(start_paused = true)]
async fn a_request_inside_the_minimum_interval_does_not_touch_the_source() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source.clone(), Duration::from_secs(30), vec![]);

    get(&router, "/todos.ics").await;
    assert_eq!(source.calls(), 1);

    tokio::time::advance(Duration::from_secs(5)).await;
    get(&router, "/todos.ics").await;
    assert_eq!(source.calls(), 1, "still inside min_refresh_interval");
}

#[tokio::test(start_paused = true)]
async fn a_request_after_the_minimum_interval_refreshes_and_replaces_the_body() {
    let source = ScriptedSource::new(vec![
        Step::Ok(vec![task("a", "Before")]),
        Step::Ok(vec![task("a", "After")]),
    ]);
    let (router, _) = build(source.clone(), Duration::from_secs(30), vec![]);

    let first = body_string(get(&router, "/todos.ics").await).await;
    assert!(first.contains("SUMMARY:Before"));

    tokio::time::advance(Duration::from_secs(31)).await;
    let second = body_string(get(&router, "/todos.ics").await).await;
    assert_eq!(source.calls(), 2);
    assert!(second.contains("SUMMARY:After"), "{second}");
}

#[tokio::test(start_paused = true)]
async fn a_failure_after_a_success_serves_the_previous_body_marked_stale() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")]), Step::Fail]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    get(&router, "/todos.ics").await;
    tokio::time::advance(Duration::from_secs(31)).await;

    let response = get(&router, "/todos.ics").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-exporter-stale"], "true");
    assert!(body_string(response).await.contains("SUMMARY:One"));
}

/// An empty calendar would tell a subscribed client to delete every task, so a
/// cold-start failure must not produce one.
#[tokio::test(start_paused = true)]
async fn a_failure_before_any_success_is_unavailable_rather_than_empty() {
    let source = ScriptedSource::new(vec![Step::Fail]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    let response = get(&router, "/todos.ics").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_string(response).await;
    assert!(!body.contains("BEGIN:VCALENDAR"), "{body}");
    assert!(body.contains("anytype unavailable"), "{body}");
}

#[tokio::test(start_paused = true)]
async fn an_error_body_never_leaks_internal_detail() {
    let source = ScriptedSource::new(vec![Step::Fail]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    let body = body_string(get(&router, "/todos.ics").await).await;
    assert!(!body.contains("scripted failure"), "{body}");
}

#[tokio::test(start_paused = true)]
async fn zero_tasks_is_a_valid_empty_calendar() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![])]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    let response = get(&router, "/todos.ics").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("BEGIN:VCALENDAR"), "{body}");
    assert!(!body.contains("BEGIN:VTODO"), "{body}");
}

#[tokio::test(start_paused = true)]
async fn concurrent_requests_share_a_single_refresh() {
    let source = ScriptedSource::with_delay(
        vec![Step::Ok(vec![task("a", "One")])],
        Duration::from_millis(200),
    );
    let (router, _) = build(source.clone(), Duration::from_secs(30), vec![]);

    let responses = futures::future::join_all(
        (0..8).map(|_| async { get(&router, "/todos.ics").await.status() }),
    )
    .await;

    assert_eq!(source.calls(), 1, "one refresh serves every waiting caller");
    assert!(responses.iter().all(|status| *status == StatusCode::OK));
}

#[tokio::test(start_paused = true)]
async fn a_matching_etag_returns_not_modified() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    let etag = get(&router, "/todos.ics").await.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_string();

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/todos.ics")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(body_string(response).await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn if_modified_since_also_returns_not_modified() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    let last_modified = get(&router, "/todos.ics").await.headers()[header::LAST_MODIFIED]
        .to_str()
        .unwrap()
        .to_string();

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/todos.ics")
                .header(header::IF_MODIFIED_SINCE, &last_modified)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

/// The stale body is sent in full even against a matching validator, so the
/// fallback is visible rather than hidden behind a cache.
#[tokio::test(start_paused = true)]
async fn a_stale_feed_is_sent_in_full_despite_a_matching_validator() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")]), Step::Fail]);
    let (router, _) = build(source, Duration::from_secs(30), vec![]);

    let etag = get(&router, "/todos.ics").await.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_string();
    tokio::time::advance(Duration::from_secs(31)).await;

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/todos.ics")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-exporter-stale"], "true");
    assert!(body_string(response).await.contains("BEGIN:VTODO"));
}

/// On a public bind the unguessable path is the only thing protecting the
/// feed, so it must actually be the only route that serves it.
#[tokio::test(start_paused = true)]
async fn the_feed_is_served_only_at_the_configured_secret_path() {
    let secret = "/f/0f8c1d2e3a4b5c6d7e8f9a0b1c2d3e4f/todos.ics";
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build_at(source.clone(), Duration::from_secs(30), vec![], secret);

    let response = get(&router, secret).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("BEGIN:VTODO"));

    let guessed = get(&router, "/todos.ics").await;
    assert_eq!(
        guessed.status(),
        StatusCode::NOT_FOUND,
        "the default path must not also serve the feed"
    );
    assert_eq!(source.calls(), 1, "a miss must not trigger a refresh");

    assert_eq!(get(&router, "/healthz").await.status(), StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn healthz_is_independent_of_the_source() {
    let source = ScriptedSource::new(vec![Step::Fail]);
    let (router, _) = build(source.clone(), Duration::from_secs(30), vec![]);

    let response = get(&router, "/healthz").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(source.calls(), 0, "liveness must not call anytype");
}

// -------------------------------------------------------------------- CORS

fn origins() -> Vec<String> {
    vec!["https://calino.io".to_string()]
}

async fn get_with_origin(router: &Router, uri: &str, origin: &str) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header(header::ORIGIN, origin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn an_allowed_origin_is_echoed_and_the_validators_are_exposed() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source, Duration::from_secs(30), origins());

    let response = get_with_origin(&router, "/todos.ics", "https://calino.io").await;
    let headers = response.headers();
    assert_eq!(
        headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://calino.io"
    );
    assert_eq!(headers[header::VARY], "Origin");
    // Without this the browser client can read neither validator.
    let exposed = headers[header::ACCESS_CONTROL_EXPOSE_HEADERS]
        .to_str()
        .unwrap();
    assert!(exposed.contains("ETag"), "{exposed}");
    assert!(exposed.contains("X-Exporter-Stale"), "{exposed}");
}

#[tokio::test(start_paused = true)]
async fn a_foreign_origin_receives_no_cors_headers() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source, Duration::from_secs(30), origins());

    let response = get_with_origin(&router, "/todos.ics", "https://evil.example").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        "a page that is not configured must not be able to read the feed"
    );
}

#[tokio::test(start_paused = true)]
async fn the_preflight_admits_the_conditional_request_headers() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source.clone(), Duration::from_secs(30), origins());

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/todos.ics")
                .header(header::ORIGIN, "https://calino.io")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let headers = response.headers();
    assert_eq!(
        headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "https://calino.io"
    );
    let allowed = headers[header::ACCESS_CONTROL_ALLOW_HEADERS]
        .to_str()
        .unwrap();
    assert!(allowed.contains("If-None-Match"), "{allowed}");
    assert!(allowed.contains("If-Modified-Since"), "{allowed}");
    assert!(
        headers[header::ACCESS_CONTROL_ALLOW_METHODS]
            .to_str()
            .unwrap()
            .contains("GET")
    );
    assert_eq!(source.calls(), 0, "a preflight must not trigger a refresh");
}

#[tokio::test(start_paused = true)]
async fn the_private_network_header_appears_only_when_requested() {
    let source = ScriptedSource::new(vec![Step::Ok(vec![task("a", "One")])]);
    let (router, _) = build(source, Duration::from_secs(30), origins());

    let without = router
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/todos.ics")
                .header(header::ORIGIN, "https://calino.io")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !without
            .headers()
            .contains_key("access-control-allow-private-network")
    );

    let with = router
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/todos.ics")
                .header(header::ORIGIN, "https://calino.io")
                .header("access-control-request-private-network", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        with.headers()["access-control-allow-private-network"],
        "true"
    );
}
