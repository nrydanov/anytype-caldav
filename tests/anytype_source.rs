//! Contract tests for the Anytype boundary, driven by the SDK's scripted
//! loopback fixture rather than by a live installation.

use std::sync::Once;

use anytype::test_util::scripted_http::{
    ScriptedHttpContentType, ScriptedHttpFixture, ScriptedHttpResponse,
};
use anytype_caldav::{
    anytype_source::AnytypeTaskSource,
    config::{AnytypeConfig, PropertiesConfig, PropertySelector},
    source::{SourceError, TaskSource},
};
use reqwest::StatusCode;

const SPACE: &str = "bafyreiaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// A profile object id, the shape an `objects` assignee usually holds.
const ALICE: &str = "bafyreibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ALICE_PARTICIPANT: &str = "_participant_space_alice";
const CAROL_PARTICIPANT: &str = "_participant_space_carol";

/// The SDK accepts a token only through its keystore, and the `env` store
/// reads this variable once per client construction.
fn ensure_token() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("ANYTYPE_KEY_HTTP_TOKEN", "test-token");
    });
}

fn properties() -> PropertiesConfig {
    PropertiesConfig {
        scheduled: PropertySelector::Key("scheduled".into()),
        deadline: PropertySelector::Key("due_date".into()),
        done: PropertySelector::Key("done".into()),
        reminder: None,
        tags: None,
        assignee: None,
    }
}

fn properties_with_assignee() -> PropertiesConfig {
    PropertiesConfig {
        assignee: Some(PropertySelector::Key("assignee".into())),
        ..properties()
    }
}

fn properties_with_reminder() -> PropertiesConfig {
    PropertiesConfig {
        reminder: Some(PropertySelector::Key("reminder_lead".into())),
        ..properties()
    }
}

/// One object whose reminder property carries the given option *names*.
/// Anytype snake_cases the keys, so the names are what must be parsed.
fn object_with_reminders(id: &str, names: &[&str]) -> String {
    let tags = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            format!(r#"{{"id":"t-{index}","key":"t_{index}","name":"{name}","color":"lime"}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{
          "archived": false,
          "id": "{id}",
          "space_id": "{SPACE}",
          "name": "With reminders",
          "type": null,
          "properties": [
            {{"name":"Scheduled","key":"scheduled","id":"p-sched","format":"date",
              "date":"2026-08-29T00:00:00+04:00"}},
            {{"name":"Deadline","key":"due_date","id":"p-due","format":"date",
              "date":"2026-08-30T14:30:00+04:00"}},
            {{"name":"Done","key":"done","id":"p-done","format":"checkbox",
              "checkbox":false}},
            {{"name":"Напомнить","key":"reminder_lead","id":"p-rem","format":"multi_select",
              "multi_select":[{tags}]}}
          ]
        }}"#
    )
}

fn object_with_tags(id: &str, names: &[&str]) -> String {
    object_with_reminders(id, &[]).replace(
        r#"{"name":"Напомнить","key":"reminder_lead","id":"p-rem","format":"multi_select",
              "multi_select":[]}"#,
        &format!(
            r#"{{"name":"Tag","key":"tag","id":"p-tag","format":"multi_select","multi_select":[{}]}}"#,
            names
                .iter()
                .enumerate()
                .map(|(i, name)| format!(
                    r#"{{"id":"g-{i}","key":"g_{i}","name":"{name}","color":"teal"}}"#
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
}

/// One object whose assignee relation points at the given ids.
fn object_with_assignees(id: &str, assignees: &[&str]) -> String {
    let ids = assignees
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(",");
    object_with_reminders(id, &[]).replace(
        r#""multi_select":[]}"#,
        &format!(
            r#""multi_select":[]}},
            {{"name":"Assignee","key":"assignee","id":"p-asg","format":"objects",
              "objects":[{ids}]}}"#
        ),
    )
}

/// The members endpoint's answer. Ids are the participant form the live API
/// returns, which is not the form an `objects` assignee usually holds.
fn members_page(members: &[(&str, &str)]) -> ScriptedHttpResponse {
    let data = members
        .iter()
        .map(|(id, name)| {
            format!(
                r#"{{"object":"member","id":"{id}","name":"{name}","icon":null,
                    "identity":"{id}","global_name":"","status":"active","role":"editor"}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    ScriptedHttpResponse::new(
        StatusCode::OK,
        ScriptedHttpContentType::Json,
        format!(
            r#"{{"data":[{data}],
                "pagination":{{"has_more":false,"limit":100,"offset":0,"total":{}}}}}"#,
            members.len()
        ),
    )
}

/// The space's people, as the `profile` search answers. A person's own page
/// links their membership, which is what ties the two id forms together.
fn profiles_page(profiles: &[(&str, &str, Option<&str>)]) -> ScriptedHttpResponse {
    let data = profiles
        .iter()
        .map(|(id, name, participant)| {
            let links = participant.map(|p| format!(r#""{p}""#)).unwrap_or_default();
            format!(
                r#"{{"archived":false,"id":"{id}","space_id":"{SPACE}","name":"{name}",
                    "type":null,"properties":[
                      {{"name":"Links","key":"links","id":"p-links","format":"objects",
                        "objects":[{links}]}}]}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    page(&data, false, profiles.len())
}

fn page(objects: &str, has_more: bool, total: usize) -> ScriptedHttpResponse {
    ScriptedHttpResponse::new(
        StatusCode::OK,
        ScriptedHttpContentType::Json,
        format!(
            r#"{{"data":[{objects}],
                "pagination":{{"has_more":{has_more},"limit":100,"offset":0,"total":{total}}}}}"#
        ),
    )
}

/// One object with the standard three mapped properties.
fn object(id: &str, name: &str, done: bool) -> String {
    format!(
        r#"{{
          "archived": false,
          "id": "{id}",
          "space_id": "{SPACE}",
          "name": "{name}",
          "type": null,
          "properties": [
            {{"name":"Scheduled","key":"scheduled","id":"p-sched","format":"date",
              "date":"2026-08-29T00:00:00+04:00"}},
            {{"name":"Deadline","key":"due_date","id":"p-due","format":"date",
              "date":"2026-08-30T14:30:00+04:00"}},
            {{"name":"Done","key":"done","id":"p-done","format":"checkbox",
              "checkbox":{done}}},
            {{"name":"Last modified","key":"last_modified_date","id":"p-lm","format":"date",
              "date":"2026-08-29T12:00:00Z"}}
          ]
        }}"#
    )
}

async fn source_for(fixture: &ScriptedHttpFixture, max_objects: usize) -> AnytypeTaskSource {
    source_with(fixture, max_objects, properties()).await
}

async fn source_with(
    fixture: &ScriptedHttpFixture,
    max_objects: usize,
    properties: PropertiesConfig,
) -> AnytypeTaskSource {
    ensure_token();
    AnytypeTaskSource::connect(
        AnytypeConfig {
            url: fixture.base_url(),
            space_id: SPACE.into(),
            type_key: "task".into(),
            max_objects,
        },
        properties,
    )
    .expect("client builds")
}

#[tokio::test]
async fn tag_options_become_category_names() {
    let fixture = ScriptedHttpFixture::start(vec![page(
        &object_with_tags("a", &["Финансы", "Семья"]),
        false,
        1,
    )])
    .await
    .expect("fixture starts");
    let properties = PropertiesConfig {
        tags: Some(PropertySelector::Key("tag".into())),
        ..properties()
    };
    let source = source_with(&fixture, 5000, properties).await;

    let batch = source.list_tasks().await.expect("lists tasks");

    assert_eq!(batch.tasks[0].tags, vec!["Финансы", "Семья"]);
    assert!(batch.warnings.is_empty(), "{:?}", batch.warnings);
}

/// One person, one calendar, however a task addresses them: the membership id
/// is rewritten onto the object the space keeps for that person.
#[tokio::test]
async fn an_assignee_is_normalized_onto_the_persons_own_object() {
    let fixture = ScriptedHttpFixture::start(vec![
        page(
            &format!(
                "{},{}",
                object_with_assignees("a", &[ALICE_PARTICIPANT]),
                object_with_assignees("b", &[ALICE])
            ),
            false,
            2,
        ),
        profiles_page(&[(ALICE, "Alice", Some(ALICE_PARTICIPANT))]),
        members_page(&[(ALICE_PARTICIPANT, "alice.any")]),
    ])
    .await
    .expect("fixture starts");
    let source = source_with(&fixture, 5000, properties_with_assignee()).await;

    let batch = source.list_tasks().await.expect("lists tasks");

    assert_eq!(batch.tasks[0].assignees, vec![ALICE]);
    assert_eq!(batch.tasks[1].assignees, vec![ALICE]);
    assert_eq!(batch.members[ALICE], "Alice");
    assert!(
        !batch.members.contains_key(ALICE_PARTICIPANT),
        "the membership would only add a second, empty calendar: {:?}",
        batch.members
    );
    assert!(
        batch.account_holders.contains(ALICE),
        "an active member stands behind her object, so she may sign in"
    );
    assert!(batch.warnings.is_empty(), "{:?}", batch.warnings);
}

/// Somebody the space has no object for still gets a name and a calendar, and
/// keeps the membership id, because nothing else identifies them.
#[tokio::test]
async fn a_member_without_an_object_of_their_own_keeps_the_membership_id() {
    let fixture = ScriptedHttpFixture::start(vec![
        page(&object_with_assignees("a", &[CAROL_PARTICIPANT]), false, 1),
        profiles_page(&[(ALICE, "Alice", Some(ALICE_PARTICIPANT))]),
        members_page(&[(CAROL_PARTICIPANT, "Carol")]),
    ])
    .await
    .expect("fixture starts");
    let source = source_with(&fixture, 5000, properties_with_assignee()).await;

    let batch = source.list_tasks().await.expect("lists tasks");

    assert_eq!(batch.tasks[0].assignees, vec![CAROL_PARTICIPANT]);
    assert_eq!(batch.members[CAROL_PARTICIPANT], "Carol");
    assert_eq!(
        batch.members[ALICE], "Alice",
        "a person with no tasks still gets a calendar"
    );
    // Alice's membership is not in the space any more, and Carol has no object
    // of her own to keep an account under.
    assert!(
        batch.account_holders.is_empty(),
        "{:?}",
        batch.account_holders
    );
}

/// Without the selector the relation is not read at all, and no member lookup
/// is made — the fixture would run out of scripted responses if one were.
#[tokio::test]
async fn assignees_are_ignored_when_the_property_is_not_configured() {
    let fixture =
        ScriptedHttpFixture::start(vec![page(&object_with_assignees("a", &[ALICE]), false, 1)])
            .await
            .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");

    assert!(batch.tasks[0].assignees.is_empty());
    assert!(batch.members.is_empty());
}

#[tokio::test]
async fn reminder_options_become_per_task_lead_times() {
    let fixture = ScriptedHttpFixture::start(vec![page(
        &object_with_reminders("a", &["1d", "2h"]),
        false,
        1,
    )])
    .await
    .expect("fixture starts");
    let source = source_with(&fixture, 5000, properties_with_reminder()).await;

    let batch = source.list_tasks().await.expect("lists tasks");

    assert_eq!(
        batch.tasks[0].reminder_leads,
        vec![chrono::Duration::days(1), chrono::Duration::hours(2)]
    );
    assert!(batch.warnings.is_empty(), "{:?}", batch.warnings);
}

#[tokio::test]
async fn an_unusable_reminder_option_is_warned_about_and_skipped() {
    let fixture = ScriptedHttpFixture::start(vec![page(
        &object_with_reminders("a", &["1d", "завтра"]),
        false,
        1,
    )])
    .await
    .expect("fixture starts");
    let source = source_with(&fixture, 5000, properties_with_reminder()).await;

    let batch = source.list_tasks().await.expect("lists tasks");

    assert_eq!(
        batch.tasks[0].reminder_leads,
        vec![chrono::Duration::days(1)]
    );
    assert_eq!(batch.warnings.len(), 1);
    assert!(
        batch.warnings[0].contains("завтра"),
        "{}",
        batch.warnings[0]
    );
}

/// Without the property configured the task falls back to the global default,
/// which is expressed as "no lead times of its own".
#[tokio::test]
async fn reminder_options_are_ignored_when_the_property_is_not_configured() {
    let fixture =
        ScriptedHttpFixture::start(vec![page(&object_with_reminders("a", &["1d"]), false, 1)])
            .await
            .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");

    assert!(batch.tasks[0].reminder_leads.is_empty());
}

#[tokio::test]
async fn searches_the_configured_space_for_the_configured_type() {
    let fixture = ScriptedHttpFixture::start(vec![page(&object("a", "One", false), false, 1)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    assert_eq!(batch.tasks.len(), 1);

    let requests = fixture.finish().await.expect("records requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method(), "POST");
    assert!(
        requests[0]
            .path()
            .contains(&format!("/v1/spaces/{SPACE}/search")),
        "unexpected path: {}",
        requests[0].path()
    );
    let body = String::from_utf8_lossy(requests[0].body());
    assert!(
        body.contains("task"),
        "type filter missing from body: {body}"
    );
}

#[tokio::test]
async fn maps_dates_status_url_and_last_modified() {
    let fixture = ScriptedHttpFixture::start(vec![page(&object("a", "One", true), false, 1)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    let task = &batch.tasks[0];

    assert_eq!(task.object_id, "a");
    assert_eq!(task.name, "One");
    assert!(task.done);
    assert_eq!(
        task.scheduled.as_ref().unwrap().raw,
        "2026-08-29T00:00:00+04:00",
        "the raw text is retained, not a normalized form"
    );
    assert_eq!(
        task.deadline.as_ref().unwrap().raw,
        "2026-08-30T14:30:00+04:00"
    );
    assert_eq!(
        task.last_modified.unwrap().to_rfc3339(),
        "2026-08-29T12:00:00+00:00"
    );
    assert!(task.object_url.is_some());
}

#[tokio::test]
async fn collects_every_page() {
    let fixture = ScriptedHttpFixture::start(vec![
        page(&object("a", "One", false), true, 2),
        page(&object("b", "Two", false), false, 2),
    ])
    .await
    .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    let mut ids: Vec<_> = batch.tasks.iter().map(|t| t.object_id.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
}

#[tokio::test]
async fn excludes_archived_objects() {
    let archived =
        object("b", "Archived", false).replace("\"archived\": false", "\"archived\": true");
    let objects = format!("{},{}", object("a", "Live", false), archived);
    let fixture = ScriptedHttpFixture::start(vec![page(&objects, false, 2)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    assert_eq!(batch.tasks.len(), 1);
    assert_eq!(batch.tasks[0].object_id, "a");
}

#[tokio::test]
async fn a_page_failure_fails_the_whole_refresh() {
    let fixture = ScriptedHttpFixture::start(vec![
        page(&object("a", "One", false), true, 2),
        ScriptedHttpResponse::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ScriptedHttpContentType::Text,
            "boom",
        ),
    ])
    .await
    .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let err = source.list_tasks().await.unwrap_err();
    assert!(
        matches!(err, SourceError::Transport(_)),
        "partial results must never be published, got {err:?}"
    );
}

#[tokio::test]
async fn more_objects_than_max_objects_fails_the_refresh() {
    let objects = format!(
        "{},{},{}",
        object("a", "One", false),
        object("b", "Two", false),
        object("c", "Three", false)
    );
    let fixture = ScriptedHttpFixture::start(vec![page(&objects, false, 3)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 2).await;

    let err = source.list_tasks().await.unwrap_err();
    assert!(matches!(err, SourceError::TooManyObjects(_)), "{err:?}");
}

/// A selector that matches nothing must fail loudly. Silently producing a feed
/// where nothing has a date and nothing is complete is wrong output dressed up
/// as valid output.
#[tokio::test]
async fn an_unresolved_selector_fails_with_a_schema_error() {
    let stripped =
        object("a", "One", false).replace("\"key\":\"scheduled\"", "\"key\":\"renamed\"");
    // The space has no `scheduled` property either: the selector is wrong.
    let fixture = ScriptedHttpFixture::start(vec![
        page(&stripped, false, 1),
        page(&property("p-other", "renamed"), false, 1),
    ])
    .await
    .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let err = source.list_tasks().await.unwrap_err();
    match err {
        SourceError::Schema(message) => {
            assert!(message.contains("properties.scheduled"), "{message}");
        }
        other => panic!("expected a schema error, got {other:?}"),
    }
}

/// Anytype omits a property nobody filled in. A property the space has but no
/// task carries is not a misconfiguration, and the feed must keep working.
#[tokio::test]
async fn a_property_no_task_has_filled_in_is_not_a_schema_error() {
    let unset = object("a", "One", false).replace(
        r#"{"name":"Scheduled","key":"scheduled","id":"p-sched","format":"date",
              "date":"2026-08-29T00:00:00+04:00"},"#,
        "",
    );
    assert!(!unset.contains("scheduled"), "{unset}");
    let fixture = ScriptedHttpFixture::start(vec![
        page(&unset, false, 1),
        page(&property("p-sched", "scheduled"), false, 1),
    ])
    .await
    .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    assert_eq!(batch.tasks.len(), 1);
    assert!(batch.tasks[0].scheduled.is_none());
}

fn property(id: &str, key: &str) -> String {
    format!(r#"{{"object":"property","id":"{id}","key":"{key}","name":"{key}","format":"date"}}"#)
}

/// Unlike a date, a wrong `done` format cannot degrade quietly: defaulting to
/// false would mark every completed task as outstanding.
#[tokio::test]
async fn a_done_property_of_the_wrong_format_is_a_schema_error() {
    let wrong = object("a", "One", false).replace(
        r#""format":"checkbox",
              "checkbox":false"#,
        r#""format":"text","text":"yes""#,
    );
    let fixture = ScriptedHttpFixture::start(vec![page(&wrong, false, 1)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let err = source.list_tasks().await.unwrap_err();
    assert!(matches!(err, SourceError::Schema(_)), "{err:?}");
}

/// A malformed date costs that one value, never the surrounding task.
#[tokio::test]
async fn a_malformed_date_is_dropped_with_a_warning() {
    let broken = object("a", "One", false).replace("2026-08-29T00:00:00+04:00", "not-a-date");
    let fixture = ScriptedHttpFixture::start(vec![page(&broken, false, 1)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    assert_eq!(batch.tasks.len(), 1, "the task survives");
    assert!(batch.tasks[0].scheduled.is_none());
    assert!(batch.tasks[0].deadline.is_some(), "other values are kept");
    assert_eq!(batch.warnings.len(), 1, "{:?}", batch.warnings);
}

/// A date selector resolving to a non-date property is a degraded value, not a
/// fatal one — but the warning must name the object, not the format.
#[tokio::test]
async fn a_date_selector_pointing_at_a_non_date_property_warns_and_drops_the_value() {
    let wrong = object("a", "One", false).replace(
        r#""format":"date",
              "date":"2026-08-29T00:00:00+04:00""#,
        r#""format":"text","text":"tomorrow""#,
    );
    let fixture = ScriptedHttpFixture::start(vec![page(&wrong, false, 1)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    assert_eq!(batch.tasks.len(), 1);
    assert!(batch.tasks[0].scheduled.is_none());
    assert_eq!(batch.warnings.len(), 1, "{:?}", batch.warnings);
    let warning = &batch.warnings[0];
    assert!(
        warning.contains("object a"),
        "object id must come first: {warning}"
    );
    assert!(
        warning.contains("Text"),
        "the offending format must be named: {warning}"
    );
}

#[tokio::test]
async fn an_unnamed_object_falls_back_to_its_snippet() {
    let unnamed =
        object("a", "One", false).replace(r#""name": "One","#, r#""snippet": "From the body","#);
    let fixture = ScriptedHttpFixture::start(vec![page(&unnamed, false, 1)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    assert_eq!(batch.tasks[0].name, "From the body");
}

/// Anytype permits two distinct properties to share one key — a real space had
/// "Deadline" and "Scheduled" both keyed `deadline`. Taking the first match
/// would map both calendar fields to one source value and look perfectly
/// normal in the output.
#[tokio::test]
async fn an_ambiguous_key_selector_is_a_schema_error() {
    let ambiguous = object("a", "One", false).replace(
        r#"{"name":"Deadline","key":"due_date","id":"p-due","format":"date",
              "date":"2026-08-30T14:30:00+04:00"}"#,
        r#"{"name":"Deadline","key":"scheduled","id":"p-due","format":"date",
              "date":"2026-08-30T14:30:00+04:00"}"#,
    );
    let fixture = ScriptedHttpFixture::start(vec![page(&ambiguous, false, 1)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let err = source.list_tasks().await.unwrap_err();
    match err {
        SourceError::Schema(message) => {
            assert!(message.contains("ambiguous"), "{message}");
            assert!(
                message.contains("p-sched"),
                "both candidates named: {message}"
            );
            assert!(
                message.contains("p-due"),
                "both candidates named: {message}"
            );
            assert!(message.contains("by id"), "the fix is suggested: {message}");
        }
        other => panic!("expected a schema error, got {other:?}"),
    }
}

#[tokio::test]
async fn zero_objects_is_a_successful_empty_batch() {
    let fixture = ScriptedHttpFixture::start(vec![page("", false, 0)])
        .await
        .expect("fixture starts");
    let source = source_for(&fixture, 5000).await;

    let batch = source.list_tasks().await.expect("lists tasks");
    assert!(batch.tasks.is_empty());
}

/// With no space in the configuration none is picked; the account's spaces are
/// listed by name and id, a personal space without a name among them.
#[test]
fn without_a_space_the_spaces_are_listed_to_pick_from() {
    use anytype_caldav::anytype_source::describe_spaces;

    let none = describe_spaces(Vec::new());
    assert!(none.contains("member of no space"), "{none}");

    let listed = describe_spaces(vec![
        ("id-own".to_string(), String::new()),
        (SPACE.to_string(), "Team".to_string()),
    ]);
    assert!(listed.contains("anytype.space_id is not set"), "{listed}");
    assert!(listed.contains("(no name): id-own"), "{listed}");
    assert!(listed.contains(&format!("Team: {SPACE}")), "{listed}");
}
