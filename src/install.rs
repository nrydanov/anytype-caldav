//! `init`: checks a space against the properties the CalDAV facade owns, and
//! creates the ones that are missing.
//!
//! This is the only module that writes to Anytype, and it only ever creates.
//! A property with the right key but the wrong format is reported, never
//! repaired: Anytype cannot change a format in place, so a repair is a data
//! migration the user decides on.
//!
//! Attaching properties to types is deliberately absent. The REST type update
//! replaces a type's whole recommended-property list, and the SDK's exact way to
//! read that list (`classify_properties`) needs gRPC credentials this service
//! does not hold. Sending a guessed list could silently detach a user's own
//! properties.

use std::collections::BTreeMap;

use anytype::{
    client::AnytypeClient,
    error::AnytypeError,
    objects::Color,
    properties::{Property, PropertyFormat},
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
    plain("ical_uid", "Calendar UID", PropertyFormat::Text),
];

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

/// Prints the plan and, with `apply`, creates the missing properties.
pub async fn run(client: &AnytypeClient, space_id: &str, apply: bool) -> Result<(), InstallError> {
    let existing = client
        .properties(space_id)
        .list()
        .await?
        .collect_all()
        .await?;
    let steps = plan(SCHEMA, &existing).map_err(InstallError::Schema)?;

    for step in &steps {
        match step {
            Step::Present { spec, id } => println!("  ok      {} ({}) {id}", spec.key, spec.format),
            Step::Create(spec) => {
                println!("  create  {} ({}) \"{}\"", spec.key, spec.format, spec.name)
            }
        }
    }

    let missing: Vec<&PropertySpec> = steps
        .iter()
        .filter_map(|step| match step {
            Step::Create(spec) => Some(*spec),
            Step::Present { .. } => None,
        })
        .collect();

    if missing.is_empty() {
        println!("nothing to create");
        return Ok(());
    }
    if !apply {
        println!(
            "dry run: nothing was created; re-run with --apply to create the properties above"
        );
        return Ok(());
    }

    for spec in missing {
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
    Ok(())
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
}
