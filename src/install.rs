//! `init`: checks a space against the properties the CalDAV facade owns, and
//! creates the ones that are missing.
//!
//! This is the only module that changes the space's schema, and it only ever
//! adds: nothing is removed, renamed or detached.
//! A property with the right key but the wrong format is reported, never
//! repaired: Anytype cannot change a format in place, so a repair is a data
//! migration the user decides on.
//!
//! Then the types: `event`, `recurring_task` and `recurring_event` are created
//! when missing, and the properties the server writes are listed on each type,
//! so the objects show them. That is REST:
//! an update sends back what the type lists with the missing keys appended,
//! and anytype-heart keeps the header and the hidden list as they were
//! (core/api/service/type.go). Finally the header: which properties a type
//! shows at the top of its objects. REST cannot set it, so this one step uses
//! gRPC, and only when a session token is given; it adds the missing
//! properties after what the header shows and removes nothing.

use std::collections::BTreeMap;

use anytype::{
    client::AnytypeClient,
    error::AnytypeError,
    objects::Color,
    properties::{Property, PropertyFormat},
    types::{CreateTypeProperty, Type, TypeLayout, TypePropertyClassification},
};

/// One property of the facade's schema.
///
/// Names and keys are English so the same schema reads the same in any space.
/// Option names are values the code parses (durations), not words.
#[derive(Debug)]
pub struct PropertySpec {
    pub key: &'static str,
    pub name: &'static str,
    pub format: PropertyFormat,
    pub options: &'static [(&'static str, Color)],
}

const fn plain(key: &'static str, name: &'static str, format: PropertyFormat) -> PropertySpec {
    PropertySpec {
        key,
        name,
        format,
        options: &[],
    }
}

/// Everything the facade reads or writes. `done` and `due_date` are bundled
/// with Anytype and normally already present; they are listed so a space that
/// lost one is caught here instead of at the first sync.
pub const SCHEMA: &[PropertySpec] = &[
    plain("done", "Done", PropertyFormat::Checkbox),
    plain("due_date", "Due date", PropertyFormat::Date),
    plain("scheduled", "Scheduled", PropertyFormat::Date),
    plain("start_date", "Start date", PropertyFormat::Date),
    plain("end_date", "End date", PropertyFormat::Date),
    plain("address", "Address", PropertyFormat::Text),
    PropertySpec {
        key: "priority",
        name: "Priority",
        format: PropertyFormat::Select,
        options: &[
            ("High", Color::Red),
            ("Medium", Color::Orange),
            ("Low", Color::Lime),
        ],
    },
    PropertySpec {
        key: "reminder_lead",
        name: "Remind",
        format: PropertyFormat::MultiSelect,
        options: &[
            ("15m", Color::Grey),
            ("30m", Color::Grey),
            ("1h", Color::Grey),
            ("2h", Color::Grey),
            ("3h", Color::Grey),
            ("12h", Color::Grey),
            ("1d", Color::Grey),
            ("2d", Color::Grey),
            ("1w", Color::Grey),
        ],
    },
    plain("rrule", "Recurrence rule", PropertyFormat::Text),
    // A task generated from a recurring task points at it, and remembers the
    // occurrence it stands for, so moving its planned date keeps its identity.
    plain("series", "Series", PropertyFormat::Objects),
    plain("occurrence", "Occurrence", PropertyFormat::Date),
    // Occurrences removed from a recurring event, one Anytype date per line.
    plain("exdate", "Excluded dates", PropertyFormat::Text),
    plain("ical_uid", "Calendar UID", PropertyFormat::Text),
    // An event's own deadline, served as an entry of its own (events.rs).
    plain("deadline", "Deadline", PropertyFormat::Date),
];

/// A type the server reads and writes objects of.
#[derive(Debug)]
pub struct TypeSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub plural_name: &'static str,
    pub layout: TypeLayout,
    /// Listed on the type, so its objects show them.
    pub properties: &'static [&'static str],
    /// Shown in the header of the type's objects, after what it already shows.
    pub header: &'static [&'static str],
}

/// `task` is bundled with Anytype; the other three are made when missing.
pub const TYPES: &[TypeSpec] = &[
    TypeSpec {
        key: "task",
        name: "Task",
        plural_name: "Tasks",
        layout: TypeLayout::Action,
        properties: &[
            "scheduled",
            "reminder_lead",
            "priority",
            "series",
            "occurrence",
        ],
        header: &["priority", "due_date", "scheduled", "reminder_lead"],
    },
    TypeSpec {
        key: "event",
        name: "Event",
        plural_name: "Events",
        layout: TypeLayout::Basic,
        properties: &[
            "start_date",
            "end_date",
            "deadline",
            "address",
            "tag",
            "reminder_lead",
        ],
        header: &["start_date", "end_date", "deadline", "reminder_lead"],
    },
    TypeSpec {
        key: "recurring_task",
        name: "Recurring task",
        plural_name: "Recurring tasks",
        layout: TypeLayout::Basic,
        properties: &["rrule", "start_date", "priority", "tag", "reminder_lead"],
        header: &["rrule", "start_date"],
    },
    TypeSpec {
        key: "recurring_event",
        name: "Recurring event",
        plural_name: "Recurring events",
        layout: TypeLayout::Basic,
        properties: &[
            "rrule",
            "start_date",
            "end_date",
            "address",
            "tag",
            "reminder_lead",
            "deadline",
        ],
        header: &["reminder_lead", "start_date", "rrule"],
    },
];

/// A property Anytype puts into every space. A list without it is not of a
/// space the account has loaded: `init` would take every property as missing
/// and create duplicates of what arrives with the space.
const BUNDLED_WITH_EVERY_SPACE: &str = "created_date";

/// Whether `existing` is the property list of a space the account has loaded.
pub fn space_is_loaded(existing: &[Property]) -> bool {
    existing
        .iter()
        .any(|property| property.key == BUNDLED_WITH_EVERY_SPACE)
}

/// The keys of `wanted` a type does not list yet, in `wanted`'s order.
pub fn missing_keys<'a>(listed: &[&str], wanted: &[&'a str]) -> Vec<&'a str> {
    wanted
        .iter()
        .filter(|key| !listed.contains(key))
        .copied()
        .collect()
}

/// The header and the ordinary list after adding `wanted` (property ids) to
/// the header: appended after what it shows, and taken out of the ordinary
/// list so none is listed twice. `None` when the header already shows them
/// all, so a repeated run writes nothing.
pub fn header_change(
    featured: &[String],
    recommended: &[String],
    wanted: &[String],
) -> Option<(Vec<String>, Vec<String>)> {
    let added: Vec<&String> = wanted.iter().filter(|id| !featured.contains(id)).collect();
    if added.is_empty() {
        return None;
    }
    let mut new_featured = featured.to_vec();
    new_featured.extend(added.iter().map(|id| (*id).clone()));
    let new_recommended = recommended
        .iter()
        .filter(|id| !added.contains(id))
        .cloned()
        .collect();
    Some((new_featured, new_recommended))
}

/// What `init` found for one spec.
#[derive(Debug)]
pub enum Step<'a> {
    Present { spec: &'a PropertySpec, id: String },
    Create(&'a PropertySpec),
}

/// A state `init` refuses to act on.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum Problem {
    #[error(
        "{key}: property {id} is {found}, expected {expected}; migrate its values and delete it first"
    )]
    WrongFormat {
        key: &'static str,
        id: String,
        found: PropertyFormat,
        expected: PropertyFormat,
    },
    #[error("{key}: {} properties share this key ({}); delete all but one first", ids.len(), ids.join(", "))]
    DuplicateKey { key: &'static str, ids: Vec<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("the space does not match the schema:\n{}", .0.iter().map(|p| format!("  {p}")).collect::<Vec<_>>().join("\n"))]
    Schema(Vec<Problem>),
    #[error("anytype: {0}")]
    Anytype(#[from] AnytypeError),
    #[error(
        "space {0} is not readable yet: the account is not a member, or has not loaded it; nothing was changed"
    )]
    NotLoaded(String),
    #[error("{0}")]
    Type(String),
    #[error("setting the header of {key}: {message}")]
    Header { key: &'static str, message: String },
    #[error(
        "{expected}: anytype created the property with key {actual}; delete {id} and investigate"
    )]
    KeyChanged {
        expected: &'static str,
        actual: String,
        id: String,
    },
}

/// Compares `existing` with `schema`. Pure, so every branch is testable
/// without a running Anytype.
///
/// All problems are collected rather than stopping at the first, so one run
/// shows the whole repair list.
pub fn plan<'a>(
    schema: &'a [PropertySpec],
    existing: &[Property],
) -> Result<Vec<Step<'a>>, Vec<Problem>> {
    let mut by_key: BTreeMap<&str, Vec<&Property>> = BTreeMap::new();
    for property in existing {
        by_key
            .entry(property.key.as_str())
            .or_default()
            .push(property);
    }

    let mut steps = Vec::new();
    let mut problems = Vec::new();
    for spec in schema {
        match by_key.get(spec.key).map(Vec::as_slice) {
            None | Some([]) => steps.push(Step::Create(spec)),
            Some([one]) if one.format() == spec.format => steps.push(Step::Present {
                spec,
                id: one.id.clone(),
            }),
            Some([one]) => problems.push(Problem::WrongFormat {
                key: spec.key,
                id: one.id.clone(),
                found: one.format(),
                expected: spec.format,
            }),
            Some(many) => problems.push(Problem::DuplicateKey {
                key: spec.key,
                ids: many.iter().map(|property| property.id.clone()).collect(),
            }),
        }
    }

    if problems.is_empty() {
        Ok(steps)
    } else {
        Err(problems)
    }
}

/// Prints the plan and, with `apply`, carries it out: missing properties,
/// then types, then headers. `grpc_available` says whether a session token
/// was given; without it the headers are left alone.
pub async fn run(
    client: &AnytypeClient,
    space_id: &str,
    apply: bool,
    grpc_available: bool,
) -> Result<(), InstallError> {
    // A space the account cannot read answers 404; one joined a moment ago
    // may be readable and still empty, which the property list shows.
    if client.space(space_id).get().await.is_err() {
        return Err(InstallError::NotLoaded(space_id.to_string()));
    }
    let mut changes = create_properties(client, space_id, apply).await?;
    changes |= prepare_types(client, space_id, apply).await?;
    if grpc_available {
        changes |= set_headers(client, space_id, apply).await?;
    } else {
        println!("  skip    headers: no session token (ANYTYPE_SESSION_TOKEN) for gRPC");
    }

    if !changes {
        println!("nothing to change");
    } else if !apply {
        println!("dry run: nothing was changed; re-run with --apply to make the changes above");
    }
    Ok(())
}

/// Whether the plan had anything to create.
async fn create_properties(
    client: &AnytypeClient,
    space_id: &str,
    apply: bool,
) -> Result<bool, InstallError> {
    let existing = client
        .properties(space_id)
        .list()
        .await?
        .collect_all()
        .await?;
    if !space_is_loaded(&existing) {
        return Err(InstallError::NotLoaded(space_id.to_string()));
    }
    let steps = plan(SCHEMA, &existing).map_err(InstallError::Schema)?;

    let mut missing = Vec::new();
    for step in &steps {
        match step {
            Step::Present { spec, id } => println!("  ok      {} ({}) {id}", spec.key, spec.format),
            Step::Create(spec) => {
                println!("  create  {} ({}) \"{}\"", spec.key, spec.format, spec.name);
                missing.push(*spec);
            }
        }
    }
    if !apply {
        return Ok(!missing.is_empty());
    }

    for spec in &missing {
        let mut request = client
            .new_property(space_id, spec.name, spec.format)
            .key(spec.key);
        for (name, color) in spec.options {
            request = request.tag(name, None, color.clone());
        }
        let created = request.create().await?;
        // The API normalises keys; a silently different key would leave the
        // facade looking for a property that does not exist.
        if created.key != spec.key {
            return Err(InstallError::KeyChanged {
                expected: spec.key,
                actual: created.key,
                id: created.id,
            });
        }
        println!("  created {} {}", spec.key, created.id);
    }
    Ok(!missing.is_empty())
}

/// The space's properties by key. Run after [`create_properties`], so every
/// key of [`SCHEMA`] is there once.
async fn properties_by_key(
    client: &AnytypeClient,
    space_id: &str,
) -> Result<BTreeMap<String, Property>, InstallError> {
    let existing = client
        .properties(space_id)
        .list()
        .await?
        .collect_all()
        .await?;
    // A type update that created a second property with a key would show here.
    plan(SCHEMA, &existing).map_err(InstallError::Schema)?;
    Ok(existing
        .into_iter()
        .map(|property| (property.key.clone(), property))
        .collect())
}

/// The space's types by key.
async fn types_by_key(
    client: &AnytypeClient,
    space_id: &str,
) -> Result<BTreeMap<String, Type>, InstallError> {
    let types = client.types(space_id).list().await?.collect_all().await?;
    Ok(types
        .into_iter()
        .map(|typ| (typ.key.clone(), typ))
        .collect())
}

fn type_property(
    properties: &BTreeMap<String, Property>,
    key: &str,
) -> Result<CreateTypeProperty, InstallError> {
    let property = properties
        .get(key)
        .ok_or_else(|| InstallError::Type(format!("the space has no property {key}")))?;
    Ok(CreateTypeProperty {
        format: property.format(),
        key: property.key.clone(),
        name: property.name.clone(),
    })
}

/// Creates the missing types and lists the missing properties on the rest.
/// Whether anything was missing.
async fn prepare_types(
    client: &AnytypeClient,
    space_id: &str,
    apply: bool,
) -> Result<bool, InstallError> {
    let types = types_by_key(client, space_id).await?;
    let properties = if apply {
        properties_by_key(client, space_id).await?
    } else {
        BTreeMap::new()
    };

    let mut changes = false;
    for spec in TYPES {
        let Some(typ) = types.get(spec.key) else {
            changes = true;
            println!("  create  type {} \"{}\"", spec.key, spec.name);
            if apply {
                let listed = spec
                    .properties
                    .iter()
                    .map(|key| type_property(&properties, key))
                    .collect::<Result<Vec<_>, _>>()?;
                let created = client
                    .new_type(space_id, spec.name)
                    .key(spec.key)
                    .plural_name(spec.plural_name)
                    .layout(spec.layout.clone())
                    .properties(listed)
                    .create()
                    .await?;
                if created.key != spec.key {
                    return Err(InstallError::Type(format!(
                        "{}: anytype created the type with key {}; delete {} and investigate",
                        spec.key, created.key, created.id
                    )));
                }
                println!("  created type {} {}", spec.key, created.id);
            }
            continue;
        };

        let listed: Vec<&str> = typ.properties.iter().map(|p| p.key.as_str()).collect();
        let missing = missing_keys(&listed, spec.properties);
        if missing.is_empty() {
            println!("  ok      type {} {}", spec.key, typ.id);
            continue;
        }
        changes = true;
        println!("  list    {} on type {}", missing.join(", "), spec.key);
        if apply {
            // An update replaces the type's list, so it carries the current
            // one; anytype-heart keeps the header as it is.
            let mut all = typ
                .properties
                .iter()
                .map(|p| type_property(&properties, &p.key))
                .collect::<Result<Vec<_>, _>>()?;
            for key in missing {
                all.push(type_property(&properties, key)?);
            }
            client
                .update_type(space_id, &typ.id)
                .properties(all)
                .update()
                .await?;
        }
    }
    Ok(changes)
}

/// Adds the missing properties to each type's header. Whether any was missing.
async fn set_headers(
    client: &AnytypeClient,
    space_id: &str,
    apply: bool,
) -> Result<bool, InstallError> {
    let types = types_by_key(client, space_id).await?;
    let properties = properties_by_key(client, space_id).await?;
    // In a dry run a property may not exist yet; its key stands in for its id.
    let id_of = |key: &str| {
        properties
            .get(key)
            .map_or_else(|| key.to_string(), |property| property.id.clone())
    };

    let mut changes = false;
    for spec in TYPES {
        let Some(typ) = types.get(spec.key) else {
            // Only in a dry run: with --apply the type was just created.
            changes = true;
            println!("  header  {}: {}", spec.key, spec.header.join(", "));
            continue;
        };
        let classes = classify(client, space_id, &typ.id).await?;
        let recommended: Vec<String> = classes.recommended.iter().map(|p| p.id.clone()).collect();
        let wanted: Vec<String> = spec.header.iter().map(|key| id_of(key)).collect();
        let Some((featured, recommended)) =
            header_change(&classes.featured_ids, &recommended, &wanted)
        else {
            println!("  ok      header {}", spec.key);
            continue;
        };
        changes = true;
        let added: Vec<&str> = spec
            .header
            .iter()
            .filter(|key| !classes.featured_ids.contains(&id_of(key)))
            .copied()
            .collect();
        println!("  header  {}: {}", spec.key, added.join(", "));
        if apply {
            set_type_details(client, &typ.id, featured, recommended)
                .await
                .map_err(|message| InstallError::Header {
                    key: spec.key,
                    message,
                })?;
        }
    }
    Ok(changes)
}

/// Reads the header over gRPC. Right after a type is created, or its list
/// changed, the read can fail for some seconds; it is tried again for up to
/// half a minute.
async fn classify(
    client: &AnytypeClient,
    space_id: &str,
    type_id: &str,
) -> Result<TypePropertyClassification, AnytypeError> {
    let mut tries = 0;
    loop {
        match client
            .get_type(space_id, type_id)
            .classify_properties()
            .await
        {
            Err(err) if tries < 6 => {
                tries += 1;
                println!("  wait    type {type_id}: {err}; trying again in 5 s");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            result => return result,
        }
    }
}

/// The one gRPC call: `ObjectSetDetails` on the type, writing both lists at
/// once so no property is ever in both.
async fn set_type_details(
    client: &AnytypeClient,
    type_id: &str,
    featured: Vec<String>,
    recommended: Vec<String>,
) -> Result<(), String> {
    use anytype_rpc::{anytype::rpc::object::set_details, model::Detail};
    use prost_types::{ListValue, Value, value::Kind};

    let list = |ids: Vec<String>| Value {
        kind: Some(Kind::ListValue(ListValue {
            values: ids
                .into_iter()
                .map(|id| Value {
                    kind: Some(Kind::StringValue(id)),
                })
                .collect(),
        })),
    };
    let request = set_details::Request {
        context_id: type_id.to_string(),
        details: vec![
            Detail {
                key: "recommendedFeaturedRelations".to_string(),
                value: Some(list(featured)),
            },
            Detail {
                key: "recommendedRelations".to_string(),
                value: Some(list(recommended)),
            },
        ],
    };

    let grpc = client.grpc_client().await.map_err(|err| err.to_string())?;
    let request = anytype_rpc::auth::with_token(tonic::Request::new(request), grpc.token())
        .map_err(|err| err.to_string())?;
    let response = grpc
        .client_commands()
        .object_set_details(request)
        .await
        .map_err(|status| status.to_string())?
        .into_inner();
    match response.error {
        Some(error) if error.code != 0 => Err(error.description),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn property(id: &str, key: &str, format: &str) -> Property {
        serde_json::from_value(serde_json::json!({
            "object": "property", "id": id, "key": key, "name": key, "format": format,
        }))
        .unwrap()
    }

    const SMALL: &[PropertySpec] = &[
        plain("done", "Done", PropertyFormat::Checkbox),
        plain("rrule", "Recurrence rule", PropertyFormat::Text),
    ];

    #[test]
    fn an_empty_space_creates_everything() {
        let steps = plan(SMALL, &[]).unwrap();
        assert!(steps.iter().all(|step| matches!(step, Step::Create(_))));
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn a_matching_property_is_left_alone() {
        let steps = plan(SMALL, &[property("id-done", "done", "checkbox")]).unwrap();
        assert!(matches!(&steps[0], Step::Present { id, .. } if id == "id-done"));
        assert!(matches!(steps[1], Step::Create(spec) if spec.key == "rrule"));
    }

    /// A space set up by hand may have `priority` as a multi-select; the schema wants a select.
    #[test]
    fn a_wrong_format_is_refused() {
        let schema = &[plain("priority", "Priority", PropertyFormat::Select)];
        let problems = plan(schema, &[property("id-p", "priority", "multi_select")]).unwrap_err();
        assert_eq!(
            problems,
            vec![Problem::WrongFormat {
                key: "priority",
                id: "id-p".into(),
                found: PropertyFormat::MultiSelect,
                expected: PropertyFormat::Select,
            }]
        );
    }

    /// The state an earlier migration can leave behind: two properties, one key.
    #[test]
    fn a_duplicated_key_is_refused_even_if_one_matches() {
        let problems = plan(
            SMALL,
            &[
                property("a", "done", "checkbox"),
                property("b", "done", "number"),
            ],
        )
        .unwrap_err();
        assert_eq!(
            problems,
            vec![Problem::DuplicateKey {
                key: "done",
                ids: vec!["a".into(), "b".into()],
            }]
        );
    }

    #[test]
    fn all_problems_are_reported_together() {
        let problems = plan(
            SMALL,
            &[
                property("a", "done", "text"),
                property("b", "rrule", "date"),
            ],
        )
        .unwrap_err();
        assert_eq!(problems.len(), 2);
    }

    #[test]
    fn schema_keys_are_unique_and_snake_case() {
        let mut keys: Vec<&str> = SCHEMA.iter().map(|spec| spec.key).collect();
        assert!(
            keys.iter()
                .all(|key| key.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        );
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), SCHEMA.len());
    }

    #[test]
    fn an_empty_or_partial_list_is_not_a_loaded_space() {
        assert!(!space_is_loaded(&[]));
        assert!(!space_is_loaded(&[property("a", "priority", "select")]));
        assert!(space_is_loaded(&[
            property("a", "priority", "select"),
            property("b", "created_date", "date"),
        ]));
    }

    #[test]
    fn types_use_only_known_properties() {
        // `tag` is bundled with Anytype and not part of the schema.
        let known = |key: &str| key == "tag" || SCHEMA.iter().any(|spec| spec.key == key);
        for spec in TYPES {
            for key in spec.properties.iter().chain(spec.header) {
                assert!(known(key), "{}: unknown property {key}", spec.key);
            }
        }
    }

    #[test]
    fn missing_keys_keeps_the_wanted_order() {
        assert_eq!(missing_keys(&["b"], &["c", "b", "a"]), ["c", "a"]);
        assert!(missing_keys(&["a", "b"], &["a"]).is_empty());
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn the_header_is_appended_to_and_the_ordinary_list_loses_what_moved() {
        let (featured, recommended) = header_change(
            &ids(&["type", "tag"]),
            &ids(&["priority", "done", "rrule"]),
            &ids(&["rrule", "tag", "priority"]),
        )
        .unwrap();
        assert_eq!(featured, ids(&["type", "tag", "rrule", "priority"]));
        assert_eq!(recommended, ids(&["done"]));
    }

    #[test]
    fn a_complete_header_is_not_written_again() {
        assert_eq!(
            header_change(&ids(&["type", "rrule"]), &ids(&["done"]), &ids(&["rrule"])),
            None
        );
    }
}
