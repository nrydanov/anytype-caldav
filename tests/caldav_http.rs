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
        ical_uid: None,
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
            writer: None,
            events: None,
            documents: None,
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

/// Thunderbird reads `resourcetype` of every discovery answer before anything
/// else and gives up on the server when it is missing
/// (`CalDavProvider.sys.mjs`, `detectCollection`).
#[tokio::test]
async fn the_base_and_the_principal_state_their_resource_type() {
    let router = router();

    let (_, _, body) = send(&router, "PROPFIND", "/dav/", Some("0"), PROBE).await;
    assert!(
        body.contains("<d:resourcetype><d:collection/></d:resourcetype>"),
        "{body}"
    );

    let (_, _, body) = send(&router, "PROPFIND", "/dav/principal/", Some("0"), PROBE).await;
    assert!(
        body.contains("<d:resourcetype><d:principal/></d:resourcetype>"),
        "{body}"
    );
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
async fn a_query_outside_the_calendar_is_forbidden_rather_than_missing() {
    for path in ["/dav/", "/dav/principal/", "/dav/calendars/"] {
        let (status, _, _) = send(&router(), "REPORT", path, Some("1"), TODO_QUERY).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
}

/// Calino's settings sync: it looks for a calendar carrying its own dead
/// property, then keeps one document there (`CalDAVClient.discoverSettingsCalendar`).
mod capture {
    use super::*;

    async fn post(
        router: &Router,
        auth_header: Option<String>,
        body: &str,
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/capture")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(value) = auth_header {
            request = request.header(header::AUTHORIZATION, value);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_dictated_line_becomes_a_task_once() {
        let (router, store) = writes::writable();
        let before = store.tasks.lock().unwrap().len();

        let (status, _) = post(&router, None, r#"{"text":"Купить корм"}"#).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let body = r#"{"text":"Купить корм","due":"2026-09-17T18:00:00+04:00","id":"reminder-1","tags":["Быт"]}"#;
        let (status, first) = post(&router, Some(auth()), body).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert!(first.contains("\"created\":true"), "{first}");
        let task = store.tasks.lock().unwrap().last().cloned().unwrap();
        assert_eq!(task.name, "Купить корм");
        assert_eq!(task.scheduled.unwrap().raw, "2026-09-17T14:00:00Z");
        assert_eq!(task.tags, vec!["Быт".to_string()]);
        assert!(!task.done);

        // The same reminder sent again writes nothing.
        let (status, second) = post(&router, Some(auth()), body).await;
        assert_eq!(status, StatusCode::OK);
        assert!(second.contains("\"created\":false"), "{second}");
        assert_eq!(store.tasks.lock().unwrap().len(), before + 1);
    }

    #[tokio::test]
    async fn an_empty_text_or_an_unreadable_date_is_refused() {
        let (router, store) = writes::writable();
        let before = store.tasks.lock().unwrap().len();
        for body in [
            r#"{"text":"  "}"#,
            r#"{"text":"x","due":"завтра"}"#,
            "not json",
        ] {
            let (status, _) = post(&router, Some(auth()), body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        }
        assert_eq!(store.tasks.lock().unwrap().len(), before);
    }
}

mod settings {
    use anytype_task_exporter::state::StateStore;

    use super::*;

    const PATH: &str = "/dav/calendars/calino-settings/calino-settings.ics";

    fn router_with_documents() -> (Router, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::open(&directory.path().join("state.sqlite3")).unwrap());
        let renderer = VTodoRenderer::new(
            CalendarConfig {
                timezone: Saratov,
                name: "t".into(),
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
            Arc::new(Fixed(Vec::new())),
            renderer,
            Duration::from_secs(30),
            Duration::from_secs(5),
        ));
        let router = http::router(
            AppState {
                feed,
                allowed_origins: Arc::new(Vec::new()),
                push: None,
                caldav: Some(Arc::new(Credentials::new("me", "pw"))),
                writer: None,
                events: None,
                documents: Some(store),
            },
            "/f/secret/todos.ics",
        );
        (router, directory)
    }

    async fn put_document(
        router: &Router,
        condition: Option<(&str, &str)>,
        body: &str,
    ) -> (StatusCode, Option<String>) {
        let mut request = Request::builder()
            .method("PUT")
            .uri(PATH)
            .header(header::AUTHORIZATION, auth())
            .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8");
        if let Some((name, value)) = condition {
            request = request.header(name, value);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let etag = response
            .headers()
            .get(header::ETAG)
            .map(|v| v.to_str().unwrap().to_string());
        (response.status(), etag)
    }

    #[tokio::test]
    async fn the_home_set_offers_a_calendar_marked_for_settings() {
        let (router, _directory) = router_with_documents();
        let (status, _, body) = send(&router, "PROPFIND", "/dav/calendars/", Some("1"), "").await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert!(
            body.contains("<d:href>/dav/calendars/calino-settings/</d:href>"),
            "{body}"
        );
        assert!(
            body.contains("<d:displayname>Calino Settings</d:displayname>"),
            "{body}"
        );
        assert!(
            body.contains(r#"<C:X-CALINO-SETTINGS-CALENDAR xmlns:C="http://calino.app/ns/">1</C:X-CALINO-SETTINGS-CALENDAR>"#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_document_is_stored_and_served_back_unchanged() {
        let (router, _directory) = router_with_documents();
        let document = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:calino-settings\r\nDTSTAMP:20260916T120000Z\r\nATTACH;ENCODING=BASE64;FMTTYPE=application/json:eyJhIjoxfQ==\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let (status, etag) = put_document(&router, Some(("If-None-Match", "*")), document).await;
        assert_eq!(status, StatusCode::CREATED);
        let etag = etag.unwrap();

        let (status, headers, body) = send(&router, "GET", PATH, None, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, document, "stored byte for byte");
        assert_eq!(headers.get(header::ETAG).unwrap().to_str().unwrap(), etag);

        // The query Calino sends, a UID filter, lists the document with its data.
        let query = r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:prop-filter name="UID"><c:text-match collation="i;octet" negate="no">calino-settings</c:text-match></c:prop-filter></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#;
        let (status, _, listing) = send(
            &router,
            "REPORT",
            "/dav/calendars/calino-settings/",
            Some("1"),
            query,
        )
        .await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert!(listing.contains("ATTACH"), "{listing}");
        // The ETag is XML-escaped inside the listing.
        assert!(listing.contains(etag.trim_matches('"')), "{listing}");

        // A second write needs the current ETag.
        let (status, _) = put_document(&router, Some(("If-Match", "\"stale\"")), document).await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        let changed = document.replace("eyJhIjoxfQ==", "eyJhIjoyfQ==");
        let (status, new_etag) = put_document(&router, Some(("If-Match", &etag)), &changed).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_ne!(new_etag.unwrap(), etag);
        let (_, _, body) = send(&router, "GET", PATH, None, "").await;
        assert_eq!(body, changed);
    }

    #[tokio::test]
    async fn without_a_store_the_calendar_is_not_offered() {
        let (status, _, body) = send(&router(), "PROPFIND", "/dav/calendars/", Some("1"), "").await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert!(!body.contains("calino-settings"), "{body}");
        let (status, _, _) = send(&router(), "GET", PATH, None, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn push_outside_the_secret_prefix_needs_the_caldav_password() {
    use anytype_task_exporter::{push::PushService, state::StateStore};

    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(StateStore::open(&directory.path().join("state.sqlite3")).unwrap());
    let push = PushService::load(
        std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/vapid-test-only.pem"
        )),
        state,
        Some("https://calendar.example/".into()),
    )
    .unwrap();
    let feed = Arc::new(FeedService::new(
        Arc::new(Fixed(Vec::new())),
        VTodoRenderer::new(
            CalendarConfig {
                timezone: Saratov,
                name: "t".into(),
                date_only_timezone: Saratov,
            },
            RemindersConfig {
                enabled: false,
                lead_time: chrono::Duration::minutes(30),
                all_day_time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            },
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        ),
        Duration::from_secs(30),
        Duration::from_secs(5),
    ));
    let router = http::router(
        AppState {
            feed,
            allowed_origins: Arc::new(Vec::new()),
            push: Some(push.clone()),
            caldav: Some(Arc::new(Credentials::new("me", "pw"))),
            writer: None,
            events: None,
            documents: None,
        },
        "/f/secret/todos.ics",
    );
    let subscription =
        r#"{"endpoint":"https://web.push.apple.com/abc","keys":{"p256dh":"k","auth":"a"}}"#;
    let request = |auth: Option<String>| {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/push/subscribe")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(auth) = auth {
            builder = builder.header(header::AUTHORIZATION, auth);
        }
        builder.body(Body::from(subscription)).unwrap()
    };

    let garbage = Request::builder()
        .method("POST")
        .uri("/push/subscribe")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(garbage).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let refused = router.clone().oneshot(request(None)).await.unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert!(!refused.headers().contains_key(header::WWW_AUTHENTICATE));
    assert_eq!(push.subscription_count(), 0);

    let accepted = router.clone().oneshot(request(Some(auth()))).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
    assert_eq!(push.subscription_count(), 1);

    let key = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/push/key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(key.status(), StatusCode::OK);
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

// ------------------------------------------------------------------ writes

mod writes {
    use std::sync::Mutex;

    use anytype_task_exporter::{source::TaskWriter, writeback::Patch};

    use super::*;

    /// Anytype in memory: listing and writing share one store, the way the
    /// real source reads what it just wrote.
    #[derive(Default)]
    pub(super) struct Store {
        pub(super) tasks: Mutex<Vec<Task>>,
        patches: Mutex<Vec<(String, Patch)>>,
        next: Mutex<u32>,
    }

    #[async_trait]
    impl TaskSource for Store {
        async fn list_tasks(&self) -> Result<TaskBatch, SourceError> {
            Ok(TaskBatch {
                tasks: self.tasks.lock().unwrap().clone(),
                warnings: Vec::new(),
            })
        }
    }

    fn apply(task: &mut Task, patch: &Patch) {
        if let Some(name) = &patch.name {
            task.name = name.clone();
        }
        if let Some(done) = patch.done {
            task.done = done;
        }
        if let Some(value) = &patch.scheduled {
            task.scheduled = value.as_deref().and_then(AnytypeDate::parse);
        }
        if let Some(value) = &patch.deadline {
            task.deadline = value.as_deref().and_then(AnytypeDate::parse);
        }
        if let Some(tags) = &patch.tags {
            task.tags = tags.clone();
        }
        // Anytype bumps last-modified on every write, which changes DTSTAMP.
        task.last_modified = Some(Utc::now());
    }

    #[async_trait]
    impl TaskWriter for Store {
        async fn get_task(&self, object_id: &str) -> Result<Option<Task>, SourceError> {
            Ok(self
                .tasks
                .lock()
                .unwrap()
                .iter()
                .find(|t| t.object_id == object_id)
                .cloned())
        }
        async fn update_task(&self, object_id: &str, patch: &Patch) -> Result<(), SourceError> {
            self.patches
                .lock()
                .unwrap()
                .push((object_id.into(), patch.clone()));
            let mut tasks = self.tasks.lock().unwrap();
            let task = tasks.iter_mut().find(|t| t.object_id == object_id).unwrap();
            apply(task, patch);
            Ok(())
        }
        async fn create_task(&self, uid: &str, patch: &Patch) -> Result<String, SourceError> {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let id = format!("bafyreinew{next}");
            let mut created = task(&id, "");
            created.scheduled = None;
            created.tags = Vec::new();
            created.ical_uid = Some(uid.into());
            apply(&mut created, patch);
            self.tasks.lock().unwrap().push(created);
            Ok(id)
        }
        async fn archive_task(&self, object_id: &str) -> Result<(), SourceError> {
            self.tasks
                .lock()
                .unwrap()
                .retain(|t| t.object_id != object_id);
            Ok(())
        }
    }

    pub(super) fn writable() -> (Router, Arc<Store>) {
        let store = Arc::new(Store::default());
        store
            .tasks
            .lock()
            .unwrap()
            .push(task("bafyreiaaa", "Pay rent"));
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
            store.clone(),
            renderer,
            Duration::from_secs(30),
            Duration::from_secs(5),
        ));
        let router = http::router(
            AppState {
                feed,
                allowed_origins: Arc::new(Vec::new()),
                push: None,
                caldav: Some(Arc::new(Credentials::new("me", "pw"))),
                writer: Some(store.clone()),
                events: None,
                documents: None,
            },
            "/f/secret/todos.ics",
        );
        (router, store)
    }

    async fn put(
        router: &Router,
        path: &str,
        condition: Option<(&str, &str)>,
        body: &str,
    ) -> (StatusCode, Option<String>) {
        let mut request = Request::builder()
            .method("PUT")
            .uri(path)
            .header(header::AUTHORIZATION, auth())
            .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8");
        if let Some((name, value)) = condition {
            request = request.header(name, value);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let etag = response
            .headers()
            .get(header::ETAG)
            .map(|v| v.to_str().unwrap().to_string());
        (response.status(), etag)
    }

    async fn current(router: &Router, name: &str) -> (String, String) {
        let (status, headers, ics) = send(
            router,
            "GET",
            &format!("/dav/calendars/tasks/{name}.ics"),
            None,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}");
        (
            headers
                .get(header::ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string(),
            ics,
        )
    }

    #[tokio::test]
    async fn the_collection_advertises_write_when_a_writer_is_present() {
        let (router, _) = writable();
        let (_, _, body) = send(&router, "PROPFIND", "/dav/calendars/", Some("1"), "").await;
        assert!(
            body.contains("<d:privilege><d:write/></d:privilege>"),
            "{body}"
        );
    }

    /// The whole Calino tick: GET, edit the bytes, PUT with If-Match.
    #[tokio::test]
    async fn ticking_in_the_client_marks_the_task_done_and_returns_the_new_etag() {
        let (router, store) = writable();
        let (etag, ics) = current(&router, "bafyreiaaa").await;
        let ticked = ics
            .replace("STATUS:NEEDS-ACTION", "STATUS:COMPLETED")
            .replace("PERCENT-COMPLETE:0", "PERCENT-COMPLETE:100");

        let (status, new_etag) = put(
            &router,
            "/dav/calendars/tasks/bafyreiaaa.ics",
            Some(("If-Match", &etag)),
            &ticked,
        )
        .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        let patches = store.patches.lock().unwrap().clone();
        assert_eq!(
            patches,
            vec![(
                "bafyreiaaa".to_string(),
                Patch {
                    done: Some(true),
                    ..Patch::default()
                }
            )]
        );
        let (fresh_etag, fresh) = current(&router, "bafyreiaaa").await;
        assert_eq!(new_etag.as_deref(), Some(fresh_etag.as_str()));
        assert_ne!(fresh_etag, etag);
        assert!(fresh.contains("STATUS:COMPLETED"), "{fresh}");
    }

    #[tokio::test]
    async fn tags_set_in_the_client_are_written_and_kept() {
        let (router, store) = writable();
        let (etag, ics) = current(&router, "bafyreiaaa").await;
        // Calino writes one CATEGORIES property with a comma list.
        let tagged = ics.replace("CATEGORIES:Финансы", "CATEGORIES:Финансы,Дом");
        let (status, _) = put(
            &router,
            "/dav/calendars/tasks/bafyreiaaa.ics",
            Some(("If-Match", &etag)),
            &tagged,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let task = store.tasks.lock().unwrap()[0].clone();
        assert_eq!(task.tags, vec!["Финансы".to_string(), "Дом".to_string()]);

        // Sent back unchanged, in another order: nothing to write.
        let (etag, ics) = current(&router, "bafyreiaaa").await;
        let before = store.patches.lock().unwrap().len();
        let (status, _) = put(
            &router,
            "/dav/calendars/tasks/bafyreiaaa.ics",
            Some(("If-Match", &etag)),
            &ics,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(store.patches.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn a_stale_etag_is_refused_and_nothing_is_written() {
        let (router, store) = writable();
        let (_, ics) = current(&router, "bafyreiaaa").await;
        let (status, _) = put(
            &router,
            "/dav/calendars/tasks/bafyreiaaa.ics",
            Some(("If-Match", "\"stale\"")),
            &ics.replace("SUMMARY:Pay rent", "SUMMARY:Changed"),
        )
        .await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert!(store.patches.lock().unwrap().is_empty());
    }

    /// Calino creates with If-None-Match: * at `<uid>.ics` and afterwards
    /// addresses the task only by that name.
    #[tokio::test]
    async fn a_task_created_in_the_client_stays_under_its_own_name() {
        let (router, store) = writable();
        let uid = "0b9a4f2c-5d1e-4c3a-9f7e-2a6b8c1d0e3f";
        let body = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:{uid}\r\nSUMMARY:Buy milk\r\nDUE;VALUE=DATE:20260920\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\nEND:VCALENDAR\r\n"
        );
        let path = format!("/dav/calendars/tasks/{uid}.ics");

        let (status, etag) = put(&router, &path, Some(("If-None-Match", "*")), &body).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(etag.is_some());

        let created = store.tasks.lock().unwrap().last().cloned().unwrap();
        assert_eq!(created.name, "Buy milk");
        assert_eq!(created.ical_uid.as_deref(), Some(uid));
        assert_eq!(created.scheduled.unwrap().raw, "2026-09-19T20:00:00Z");

        let (_, ics) = current(&router, uid).await;
        assert!(ics.contains(&format!("UID:{uid}")), "{ics}");

        // A second create at the same name is a precondition failure, not a duplicate.
        let (status, _) = put(&router, &path, Some(("If-None-Match", "*")), &body).await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(store.tasks.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_create_at_a_name_that_does_not_match_the_uid_is_refused() {
        let (router, store) = writable();
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:abc\r\nSUMMARY:x\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let (status, _) = put(
            &router,
            "/dav/calendars/tasks/other.ics",
            Some(("If-None-Match", "*")),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(store.tasks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_archives_and_the_resource_is_gone() {
        let (router, store) = writable();
        let (etag, _) = current(&router, "bafyreiaaa").await;
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/dav/calendars/tasks/bafyreiaaa.ics")
                    .header(header::AUTHORIZATION, auth())
                    .header("If-Match", etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(store.tasks.lock().unwrap().is_empty());
        let (status, _, _) = send(
            &router,
            "GET",
            "/dav/calendars/tasks/bafyreiaaa.ics",
            None,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_event_is_refused_with_a_reason() {
        let (router, _) = writable();
        let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:e\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let (status, _) = put(
            &router,
            "/dav/calendars/tasks/e.ics",
            Some(("If-None-Match", "*")),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    mod events {
        use anytype_task_exporter::events::{Event, EventPatch, EventService, EventStore};

        use super::*;

        #[derive(Default)]
        struct Events {
            events: Mutex<Vec<Event>>,
            next: Mutex<u32>,
        }

        fn apply(event: &mut Event, patch: &EventPatch) {
            if let Some(name) = &patch.name {
                event.name = name.clone();
            }
            if let Some(start) = &patch.start {
                event.start = start.as_deref().and_then(AnytypeDate::parse);
            }
            if let Some(end) = &patch.end {
                event.end = end.as_deref().and_then(AnytypeDate::parse);
            }
            if let Some(location) = &patch.location {
                event.location = location.clone();
            }
            if let Some(reminders) = &patch.reminders {
                event.reminder_names = reminders.clone();
            }
            if let Some(tags) = &patch.tags {
                event.tags = tags.clone();
            }
            if let Some(rule) = &patch.rrule {
                event.rrule = rule.clone();
            }
            if let Some(exdates) = &patch.exdates {
                event.exdates = exdates
                    .iter()
                    .filter_map(|d| AnytypeDate::parse(d))
                    .collect();
            }
            if let Some(series) = &patch.series {
                event.series = Some(series.clone());
            }
            if let Some(occurrence) = &patch.occurrence {
                event.occurrence = AnytypeDate::parse(occurrence);
            }
            event.last_modified = Some(event.last_modified.unwrap() + chrono::Duration::seconds(1));
        }

        #[async_trait]
        impl EventStore for Events {
            async fn list(&self) -> Result<Vec<Event>, SourceError> {
                Ok(self.events.lock().unwrap().clone())
            }
            async fn get(&self, object_id: &str) -> Result<Option<Event>, SourceError> {
                Ok(self
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|e| e.object_id == object_id)
                    .cloned())
            }
            async fn create(&self, uid: &str, patch: &EventPatch) -> Result<String, SourceError> {
                let mut next = self.next.lock().unwrap();
                *next += 1;
                let mut event = lecture(&format!("new{next}"));
                event.ical_uid = Some(uid.into());
                event.reminder_names.clear();
                event.location = None;
                apply(&mut event, patch);
                let id = event.object_id.clone();
                self.events.lock().unwrap().push(event);
                Ok(id)
            }
            async fn create_replacement(&self, patch: &EventPatch) -> Result<String, SourceError> {
                let mut next = self.next.lock().unwrap();
                *next += 1;
                let mut event = lecture(&format!("repl{next}"));
                event.reminder_names.clear();
                event.location = None;
                event.tags.clear();
                apply(&mut event, patch);
                let id = event.object_id.clone();
                self.events.lock().unwrap().push(event);
                Ok(id)
            }
            async fn update(&self, object_id: &str, patch: &EventPatch) -> Result<(), SourceError> {
                let mut events = self.events.lock().unwrap();
                let event = events
                    .iter_mut()
                    .find(|e| e.object_id == object_id)
                    .unwrap();
                apply(event, patch);
                Ok(())
            }
            async fn archive(&self, object_id: &str) -> Result<(), SourceError> {
                self.events
                    .lock()
                    .unwrap()
                    .retain(|e| e.object_id != object_id);
                Ok(())
            }
        }

        fn lecture(id: &str) -> Event {
            Event {
                object_id: id.into(),
                name: "Лекция".into(),
                start: AnytypeDate::parse("2026-09-20T09:50:00Z"),
                end: AnytypeDate::parse("2026-09-20T11:20:00Z"),
                location: Some("Кафедра".into()),
                tags: vec!["Аспирантура".into()],
                reminder_names: vec!["15m".into()],
                ical_uid: None,
                object_url: None,
                last_modified: Some(Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap()),
                rrule: None,
                exdates: Vec::new(),
                series: None,
                occurrence: None,
            }
        }

        fn with_events() -> (Router, Arc<Events>) {
            let (_, tasks) = writable();
            let events = Arc::new(Events::default());
            events.events.lock().unwrap().push(lecture("bafyreieee"));
            let mut undated = lecture("bafyreiundated");
            undated.start = None;
            events.events.lock().unwrap().push(undated);
            let config = CalendarConfig {
                timezone: Saratov,
                name: "Anytype Tasks".into(),
                date_only_timezone: Saratov,
            };
            let renderer = VTodoRenderer::new(
                config.clone(),
                RemindersConfig {
                    enabled: false,
                    lead_time: chrono::Duration::minutes(30),
                    all_day_time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                },
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            );
            let feed = Arc::new(FeedService::new(
                tasks.clone(),
                renderer,
                Duration::from_secs(30),
                Duration::from_secs(5),
            ));
            let service = EventService::new(
                events.clone(),
                config,
                Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                // No cache, so every request sees the store as it is.
                Duration::ZERO,
            );
            let router = http::router(
                AppState {
                    feed,
                    allowed_origins: Arc::new(Vec::new()),
                    push: None,
                    caldav: Some(Arc::new(Credentials::new("me", "pw"))),
                    writer: Some(tasks),
                    events: Some(Arc::new(service)),
                    documents: None,
                },
                "/f/secret/todos.ics",
            );
            (router, events)
        }

        const EVENT_QUERY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="20260901T000000Z" end="20261101T000000Z"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#;

        #[tokio::test]
        async fn the_home_set_lists_tasks_and_events_by_component() {
            let (router, _) = with_events();
            let (status, _, body) =
                send(&router, "PROPFIND", "/dav/calendars/", Some("1"), "").await;
            assert_eq!(status, StatusCode::MULTI_STATUS);
            assert!(
                body.contains("<d:href>/dav/calendars/tasks/</d:href>"),
                "{body}"
            );
            assert!(
                body.contains("<d:href>/dav/calendars/events/</d:href>"),
                "{body}"
            );
            assert!(body.contains(r#"<c:comp name="VEVENT"/>"#), "{body}");
        }

        #[tokio::test]
        async fn an_event_query_serves_dated_events_and_a_task_query_none() {
            let (router, _) = with_events();
            let (status, _, body) = send(
                &router,
                "REPORT",
                "/dav/calendars/events/",
                Some("1"),
                EVENT_QUERY,
            )
            .await;
            assert_eq!(status, StatusCode::MULTI_STATUS);
            assert_eq!(body.matches("<d:response>").count(), 1, "{body}");
            assert!(
                body.contains("/dav/calendars/events/bafyreieee.ics"),
                "{body}"
            );
            assert!(body.contains("TRIGGER:-PT15M"), "{body}");
            let (_, _, body) = send(
                &router,
                "REPORT",
                "/dav/calendars/events/",
                Some("1"),
                TODO_QUERY,
            )
            .await;
            assert_eq!(body.matches("<d:response>").count(), 0, "{body}");
        }

        async fn get_event(router: &Router, name: &str) -> (String, String) {
            let (status, headers, ics) = send(
                router,
                "GET",
                &format!("/dav/calendars/events/{name}.ics"),
                None,
                "",
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{name}");
            (
                headers
                    .get(header::ETAG)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string(),
                ics,
            )
        }

        #[tokio::test]
        async fn moving_an_event_in_the_client_writes_its_dates() {
            let (router, events) = with_events();
            let (etag, ics) = get_event(&router, "bafyreieee").await;
            let edited = ics
                .replace("DTSTART:20260920T095000Z", "DTSTART:20260921T100000Z")
                .replace("DTEND:20260920T112000Z", "DTEND:20260921T113000Z")
                .replace("TRIGGER:-PT15M", "TRIGGER:-PT60M");
            let (status, new_etag) = put(
                &router,
                "/dav/calendars/events/bafyreieee.ics",
                Some(("If-Match", &etag)),
                &edited,
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert!(new_etag.is_some_and(|e| e != etag));
            let stored = events.events.lock().unwrap()[0].clone();
            assert_eq!(stored.start.unwrap().raw, "2026-09-21T10:00:00Z");
            assert_eq!(stored.end.unwrap().raw, "2026-09-21T11:30:00Z");
            assert_eq!(stored.reminder_names, vec!["1h".to_string()]);
        }

        #[tokio::test]
        async fn a_stale_event_etag_is_refused() {
            let (router, _) = with_events();
            let (_, ics) = get_event(&router, "bafyreieee").await;
            let (status, _) = put(
                &router,
                "/dav/calendars/events/bafyreieee.ics",
                Some(("If-Match", "\"old\"")),
                &ics,
            )
            .await;
            assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        }

        #[tokio::test]
        async fn an_event_created_in_the_client_lands_in_anytype_under_its_name() {
            let (router, events) = with_events();
            let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc 1\r\nSUMMARY:Встреча\r\nCATEGORIES:Аспирантура,Кафедра\r\nDTSTART;VALUE=DATE:20261001\r\nDTEND;VALUE=DATE:20261003\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
            let (status, etag) = put(
                &router,
                "/dav/calendars/events/abc~201.ics",
                Some(("If-None-Match", "*")),
                body,
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
            let (served, _) = get_event(&router, "abc~201").await;
            assert_eq!(Some(served), etag);
            let stored = events.events.lock().unwrap().last().cloned().unwrap();
            assert_eq!(
                stored.tags,
                vec!["Аспирантура".to_string(), "Кафедра".to_string()]
            );
            // All day, 1–2 October: last day inclusive in Anytype.
            assert_eq!(stored.start.unwrap().raw, "2026-09-30T20:00:00Z");
            assert_eq!(stored.end.unwrap().raw, "2026-10-01T20:00:00Z");
        }

        /// Calino's bodies (0.32–0.33): TZID wall-clock times without
        /// VTIMEZONE, one EXDATE per date, the whole group PUT to the master's
        /// href with If-Match.
        const SERIES: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Calino//EN\r\nBEGIN:VEVENT\r\nUID:9f1c0b7e-1d2a-4c55-8a36-5b8e2f4d7c10\r\nDTSTAMP:20260916T101500Z\r\nSEQUENCE:0\r\nDTSTART;TZID=Europe/Saratov:20260921T135000\r\nDTEND;TZID=Europe/Saratov:20260921T152000\r\nSUMMARY:Практика ЯП\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20261228T195959Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        const SERIES_PATH: &str = "/dav/calendars/events/9f1c0b7e-1d2a-4c55-8a36-5b8e2f4d7c10.ics";

        fn master_block(body: &str) -> &str {
            let start = body.find("BEGIN:VEVENT").unwrap();
            let end =
                body[start..].find("END:VEVENT\r\n").unwrap() + start + "END:VEVENT\r\n".len();
            &body[start..end]
        }

        fn with_blocks(extra_master_lines: &str, replacement: &str) -> String {
            let master = master_block(SERIES).replace(
                "SEQUENCE:0\r\n",
                &format!("SEQUENCE:1\r\n{extra_master_lines}"),
            );
            format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Calino//EN\r\n{master}{replacement}END:VCALENDAR\r\n"
            )
        }

        const MOVED: &str = "BEGIN:VEVENT\r\nUID:9f1c0b7e-1d2a-4c55-8a36-5b8e2f4d7c10\r\nDTSTAMP:20260916T101600Z\r\nSEQUENCE:1\r\nDTSTART;TZID=Europe/Saratov:20260928T160000\r\nDTEND;TZID=Europe/Saratov:20260928T173000\r\nSUMMARY:Практика ЯП (перенос)\r\nRECURRENCE-ID;TZID=Europe/Saratov:20260928T135000\r\nEND:VEVENT\r\n";

        #[tokio::test]
        async fn a_recurring_series_lives_through_calinos_edits() {
            let (router, events) = with_events();
            let count = || events.events.lock().unwrap().len();
            let before = count();

            // Create.
            let (status, etag) =
                put(&router, SERIES_PATH, Some(("If-None-Match", "*")), SERIES).await;
            assert_eq!(status, StatusCode::CREATED);
            let master = events.events.lock().unwrap().last().cloned().unwrap();
            assert_eq!(
                master.rrule.as_deref(),
                Some("FREQ=WEEKLY;BYDAY=MO;UNTIL=20261228T195959Z")
            );
            assert_eq!(master.start.as_ref().unwrap().raw, "2026-09-21T09:50:00Z");
            let (served, ics) = get_event(&router, "9f1c0b7e-1d2a-4c55-8a36-5b8e2f4d7c10").await;
            assert_eq!(Some(served.clone()), etag);
            assert!(
                ics.contains("DTSTART;TZID=Europe/Saratov:20260921T135000"),
                "{ics}"
            );
            assert!(
                ics.contains("RRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20261228T195959Z"),
                "{ics}"
            );

            // Edit only the occurrence of 28 September.
            let body = with_blocks("", MOVED);
            let (status, etag) =
                put(&router, SERIES_PATH, Some(("If-Match", &served)), &body).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(count(), before + 2);
            let replacement = events.events.lock().unwrap().last().cloned().unwrap();
            assert_eq!(
                replacement.series.as_deref(),
                Some(master.object_id.as_str())
            );
            assert_eq!(
                replacement.occurrence.as_ref().unwrap().raw,
                "2026-09-28T09:50:00Z"
            );
            assert_eq!(
                replacement.start.as_ref().unwrap().raw,
                "2026-09-28T12:00:00Z"
            );
            let (served, ics) = get_event(&router, "9f1c0b7e-1d2a-4c55-8a36-5b8e2f4d7c10").await;
            assert_eq!(Some(served.clone()), etag);
            assert!(
                ics.contains("RECURRENCE-ID;TZID=Europe/Saratov:20260928T135000"),
                "{ics}"
            );
            assert!(ics.contains("SUMMARY:Практика ЯП (перенос)"), "{ics}");
            // Served inside the series, not as a resource of its own.
            let (_, _, listing) = send(
                &router,
                "REPORT",
                "/dav/calendars/events/",
                Some("1"),
                EVENT_QUERY,
            )
            .await;
            assert_eq!(
                listing
                    .matches("<d:href>/dav/calendars/events/9f1c0b7e")
                    .count(),
                1,
                "{listing}"
            );

            // The same body again (a retry) creates nothing.
            let (status, _) = put(&router, SERIES_PATH, Some(("If-Match", &served)), &body).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(count(), before + 2);

            // Delete only that occurrence: EXDATE on the master, the
            // replacement leaves the group.
            let body = with_blocks("EXDATE;TZID=Europe/Saratov:20260928T135000\r\n", "");
            let (status, _) = put(&router, SERIES_PATH, Some(("If-Match", &served)), &body).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(count(), before + 1);
            let master_now = events
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.object_id == master.object_id)
                .cloned()
                .unwrap();
            assert_eq!(master_now.exdates.len(), 1);
            assert_eq!(master_now.exdates[0].raw, "2026-09-28T09:50:00Z");
            let (served, ics) = get_event(&router, "9f1c0b7e-1d2a-4c55-8a36-5b8e2f4d7c10").await;
            assert!(
                ics.contains("EXDATE;TZID=Europe/Saratov:20260928T135000"),
                "{ics}"
            );
            assert!(!ics.contains("RECURRENCE-ID"), "{ics}");

            // Delete the series.
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(SERIES_PATH)
                        .header(header::AUTHORIZATION, auth())
                        .header("If-Match", served)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert_eq!(count(), before);
        }

        #[tokio::test]
        async fn deleting_a_series_archives_its_replaced_occurrences() {
            let (router, events) = with_events();
            let before = events.events.lock().unwrap().len();
            let body = with_blocks("", MOVED).replace("SEQUENCE:1", "SEQUENCE:0");
            let (status, etag) =
                put(&router, SERIES_PATH, Some(("If-None-Match", "*")), &body).await;
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(events.events.lock().unwrap().len(), before + 2);
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(SERIES_PATH)
                        .header(header::AUTHORIZATION, auth())
                        .header("If-Match", etag.unwrap())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert_eq!(events.events.lock().unwrap().len(), before);
        }

        #[tokio::test]
        async fn deleting_an_event_archives_it() {
            let (router, events) = with_events();
            let (etag, _) = get_event(&router, "bafyreieee").await;
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/dav/calendars/events/bafyreieee.ics")
                        .header(header::AUTHORIZATION, auth())
                        .header("If-Match", etag)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert!(
                events
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|e| e.object_id != "bafyreieee")
            );
        }
    }
}
