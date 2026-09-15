//! The CalDAV facade driven through the real router with Calino's own request
//! bodies (Calino 0.33.4: `discovery.ts`, `CalDAVClient.ts`, tsdav's
//! calendar-query).

use std::{sync::Arc, time::Duration};

use anytype_task_exporter::{
    caldav::Credentials,
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
use base64::Engine;
use chrono::{TimeZone, Utc};
use chrono_tz::Europe::Saratov;
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Fixed(Vec<Task>);

#[async_trait]
impl TaskSource for Fixed {
    async fn list_tasks(&self) -> Result<TaskBatch, SourceError> {
        Ok(TaskBatch {
            tasks: self.0.clone(),
            warnings: Vec::new(),
        })
    }
}

fn task(id: &str, name: &str) -> Task {
    Task {
        object_id: id.into(),
        name: name.into(),
        scheduled: AnytypeDate::parse("2026-09-20T09:00:00Z"),
        deadline: None,
        done: false,
        reminder_leads: Vec::new(),
        tags: vec!["Финансы".into()],
        object_url: None,
        last_modified: Some(Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap()),
    }
}

fn router() -> Router {
    let renderer = VTodoRenderer::new(
        CalendarConfig {
            timezone: Saratov,
            name: "Anytype Tasks".into(),
            date_only_timezone: Saratov,
        },
        RemindersConfig {
            enabled: false,
            lead_time: chrono::Duration::minutes(30),
            all_day_time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        },
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
    );
    let feed = Arc::new(FeedService::new(
        Arc::new(Fixed(vec![
            task("bafyreiaaa", "Pay rent & bills <now>"),
            task("bafyreibbb", "Guitar"),
        ])),
        renderer,
        Duration::from_secs(30),
        Duration::from_secs(5),
    ));
    http::router(
        AppState {
            feed,
            allowed_origins: Arc::new(Vec::new()),
            push: None,
            caldav: Some(Arc::new(Credentials::new("me", "pw"))),
        },
        "/f/secret/todos.ics",
    )
}

fn auth() -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("me:pw")
    )
}

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    depth: Option<&str>,
    body: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, auth())
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8");
    if let Some(depth) = depth {
        request = request.header("Depth", depth);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

const PROBE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:displayname/></d:prop></d:propfind>"#;

const PRINCIPAL_QUERY: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal xmlns:d="DAV:"/></d:prop></d:propfind>"#;

const TODO_QUERY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VTODO"/></c:comp-filter></c:filter></c:calendar-query>"#;

#[tokio::test]
async fn without_credentials_the_facade_asks_for_them() {
    let response = router()
        .oneshot(
            Request::builder()
                .method("PROPFIND")
                .uri("/dav/")
                .body(Body::from(PROBE))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Basic ")
    );
}

/// Calino's well-known probe is a GET without credentials. Challenging it
/// made Firefox open its native sign-in dialog over the app.
#[tokio::test]
async fn a_credential_less_get_is_refused_without_a_browser_challenge() {
    for method in ["GET", "HEAD"] {
        let response = router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/dav/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method}");
        assert!(
            response.headers().get(header::WWW_AUTHENTICATE).is_none(),
            "{method}"
        );
    }
}

#[tokio::test]
async fn discovery_leads_from_the_base_to_a_read_only_task_collection() {
    let router = router();

    let (status, _, body) = send(&router, "PROPFIND", "/dav/", Some("0"), PROBE).await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(
        body.contains("<d:current-user-principal><d:href>/dav/principal/</d:href>"),
        "{body}"
    );

    let (status, _, body) = send(
        &router,
        "PROPFIND",
        "/dav/principal/",
        Some("0"),
        PRINCIPAL_QUERY,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(
        body.contains("<c:calendar-home-set><d:href>/dav/calendars/</d:href>"),
        "{body}"
    );

    let (status, _, body) = send(&router, "PROPFIND", "/dav/calendars/", Some("1"), PROBE).await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(
        body.contains("<d:href>/dav/calendars/tasks/</d:href>"),
        "{body}"
    );
    assert!(body.contains("<c:calendar/>"), "{body}");
    assert!(body.contains("<c:comp name=\"VTODO\"/>"), "{body}");
    assert!(body.contains("<cs:getctag>"), "{body}");
    assert!(
        body.contains("<d:privilege><d:read/></d:privilege>"),
        "{body}"
    );
    assert!(!body.contains("<d:write"), "{body}");
}

#[tokio::test]
async fn a_task_query_lists_every_task_with_escaped_data_and_matching_etags() {
    let router = router();
    let (status, _, body) = send(
        &router,
        "REPORT",
        "/dav/calendars/tasks/",
        Some("1"),
        TODO_QUERY,
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(body.matches("<d:response>").count(), 2, "{body}");
    assert!(
        body.contains("SUMMARY:Pay rent &amp; bills &lt;now&gt;"),
        "{body}"
    );
    assert!(body.contains("CATEGORIES:Финансы"), "{body}");

    // The ETag a REPORT hands out is the one GET and PROPFIND agree on.
    let etag = body
        .split("<d:href>/dav/calendars/tasks/bafyreiaaa.ics</d:href>")
        .nth(1)
        .and_then(|rest| rest.split("<d:getetag>").nth(1))
        .and_then(|rest| rest.split("</d:getetag>").next())
        .unwrap()
        .replace("&quot;", "\"");
    let (status, headers, ics) = send(
        &router,
        "GET",
        "/dav/calendars/tasks/bafyreiaaa.ics",
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::ETAG).unwrap().to_str().unwrap(), etag);
    assert!(ics.starts_with("BEGIN:VCALENDAR"), "{ics}");
    assert_eq!(ics.matches("BEGIN:VTODO").count(), 1, "{ics}");

    let (status, _, body) = send(
        &router,
        "PROPFIND",
        "/dav/calendars/tasks/bafyreiaaa.ics",
        Some("0"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(body.contains(&etag.replace('"', "&quot;")), "{body}");
}

#[tokio::test]
async fn an_event_query_returns_nothing() {
    let (status, _, body) = send(
        &router(),
        "REPORT",
        "/dav/calendars/tasks/",
        Some("1"),
        &TODO_QUERY.replace("VTODO", "VEVENT"),
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert_eq!(body.matches("<d:response>").count(), 0, "{body}");
}

#[tokio::test]
async fn unknown_resources_are_missing_and_writes_are_refused() {
    let router = router();
    let (status, _, _) = send(
        &router,
        "GET",
        "/dav/calendars/tasks/bafyreizzz.ics",
        None,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &router,
        "PUT",
        "/dav/calendars/tasks/bafyreiaaa.ics",
        None,
        "BEGIN:VCALENDAR",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
