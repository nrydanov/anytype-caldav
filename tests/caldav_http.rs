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
async fn a_query_outside_the_calendar_is_forbidden_rather_than_missing() {
    for path in ["/dav/", "/dav/principal/", "/dav/calendars/"] {
        let (status, _, _) = send(&router(), "REPORT", path, Some("1"), TODO_QUERY).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
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
    struct Store {
        tasks: Mutex<Vec<Task>>,
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

    fn writable() -> (Router, Arc<Store>) {
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
            let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc 1\r\nSUMMARY:Встреча\r\nDTSTART;VALUE=DATE:20261001\r\nDTEND;VALUE=DATE:20261003\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
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
            // All day, 1–2 October: last day inclusive in Anytype.
            assert_eq!(stored.start.unwrap().raw, "2026-09-30T20:00:00Z");
            assert_eq!(stored.end.unwrap().raw, "2026-10-01T20:00:00Z");
        }

        #[tokio::test]
        async fn a_recurring_event_is_refused() {
            let (router, _) = with_events();
            let body = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:r\r\nSUMMARY:x\r\nDTSTART:20261001T100000Z\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
            let (status, _) = put(
                &router,
                "/dav/calendars/events/r.ics",
                Some(("If-None-Match", "*")),
                body,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
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
