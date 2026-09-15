//! Reads tasks from Anytype. Schema writes live in `install`.

use std::collections::BTreeMap;

use anytype::{
    client::{AnytypeClient, ClientConfig},
    objects::Object,
    properties::{PropertyValue, PropertyWithValue},
};
use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use tracing::warn;

use crate::{
    config::{AnytypeConfig, PropertiesConfig, PropertySelector},
    model::{AnytypeDate, Task, TaskBatch},
    source::{SourceError, TaskSource},
};

pub struct AnytypeTaskSource {
    client: AnytypeClient,
    config: AnytypeConfig,
    properties: PropertiesConfig,
}

/// Builds the SDK client.
///
/// `keystore = "env"` is required, not a preference: the default platform
/// keystore is an OS keyring, and the SDK's own comment on the credential
/// lookup notes it "may trigger user auth" — a Keychain prompt inside a
/// headless service. The env store reads `ANYTYPE_KEY_HTTP_TOKEN`, which
/// `main` populates from `ANYTYPE_API_KEY`.
pub fn build_client(config: &AnytypeConfig) -> Result<AnytypeClient, SourceError> {
    let client_config = ClientConfig {
        base_url: Some(config.url.clone()),
        app_name: env!("CARGO_PKG_NAME").to_string(),
        keystore: Some("env".to_string()),
        ..Default::default()
    };
    AnytypeClient::with_config(client_config).map_err(|err| SourceError::Transport(err.to_string()))
}

impl AnytypeTaskSource {
    pub fn connect(
        config: AnytypeConfig,
        properties: PropertiesConfig,
    ) -> Result<Self, SourceError> {
        let client = build_client(&config)?;
        Ok(Self {
            client,
            config,
            properties,
        })
    }
}

#[async_trait]
impl TaskSource for AnytypeTaskSource {
    async fn list_tasks(&self) -> Result<TaskBatch, SourceError> {
        let paged = self
            .client
            .search_in(&self.config.space_id)
            .types([self.config.type_key.as_str()])
            .execute()
            .await
            .map_err(|err| SourceError::Transport(err.to_string()))?;

        let mut objects = Vec::new();
        let mut stream = paged.into_stream();
        while let Some(item) = stream.next().await {
            let object = item.map_err(|err| SourceError::Transport(err.to_string()))?;
            if objects.len() >= self.config.max_objects {
                return Err(SourceError::TooManyObjects(format!(
                    "space {} holds more than max_objects = {} objects of type {}",
                    self.config.space_id, self.config.max_objects, self.config.type_key
                )));
            }
            objects.push(object);
        }

        // Search results can include archived objects, so this is required
        // rather than defensive.
        let live: Vec<&Object> = objects.iter().filter(|object| !object.archived).collect();

        self.verify_schema(&live)?;

        let mut batch = TaskBatch::default();
        for object in live {
            batch.tasks.push(self.to_task(object, &mut batch.warnings)?);
        }
        Ok(batch)
    }
}

impl AnytypeTaskSource {
    /// Fails loudly when a configured selector matches nothing.
    ///
    /// A selector that silently resolves to nothing produces a feed where every
    /// task has no dates and none are complete — wrong output that looks like
    /// valid output. The error names what the type actually offers, which is
    /// how an operator discovers the opaque ids without a setup wizard.
    fn verify_schema(&self, objects: &[&Object]) -> Result<(), SourceError> {
        if objects.is_empty() {
            // Nothing to verify against; the empty-result warning covers this.
            return Ok(());
        }

        let mut unresolved = Vec::new();
        for (field, selector) in [
            ("scheduled", &self.properties.scheduled),
            ("deadline", &self.properties.deadline),
            ("done", &self.properties.done),
        ] {
            let mut found = false;
            for object in objects {
                if resolve_property(&object.properties, selector, field)?.is_some() {
                    found = true;
                    break;
                }
            }
            if !found {
                unresolved.push(format!("properties.{field} = \"{selector}\""));
            }
        }

        if unresolved.is_empty() {
            return Ok(());
        }

        // Deduplicated across objects: one line per distinct property.
        let mut observed: BTreeMap<&str, String> = BTreeMap::new();
        for object in objects {
            for property in &object.properties {
                observed.entry(property.key.as_str()).or_insert_with(|| {
                    format!(
                        "key:{} id:{} name:{:?} format:{:?}",
                        property.key,
                        property.id,
                        property.name,
                        property.format()
                    )
                });
            }
        }
        for line in observed.values() {
            warn!(property = %line, "property available on type {}", self.config.type_key);
        }

        Err(SourceError::Schema(format!(
            "{} did not match any property on type {}; {} properties were observed and logged",
            unresolved.join(", "),
            self.config.type_key,
            observed.len()
        )))
    }

    fn to_task(&self, object: &Object, warnings: &mut Vec<String>) -> Result<Task, SourceError> {
        let name = object
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .or(object.snippet.as_deref())
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "(unnamed)".to_string());

        let scheduled =
            self.read_date(object, &self.properties.scheduled, "scheduled", warnings)?;
        let deadline = self.read_date(object, &self.properties.deadline, "deadline", warnings)?;
        let done = self.read_done(object)?;
        let reminder_leads = self.read_reminder_leads(object, warnings)?;

        let last_modified = object
            .get_property_date("last_modified_date")
            .map(|date| date.with_timezone(&Utc));

        Ok(Task {
            object_id: object.id.clone(),
            name,
            scheduled,
            deadline,
            done,
            reminder_leads,
            object_url: Some(object.get_link()),
            last_modified,
        })
    }

    /// Reads the per-task lead times, if the property is configured.
    ///
    /// Durations come from tag *names*, not keys: Anytype snake_cases a key,
    /// so an option displayed as `15m` is stored under `15_m`, which no
    /// duration parser accepts. An unusable option costs that one value and
    /// leaves the rest of the task intact.
    fn read_reminder_leads(
        &self,
        object: &Object,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<chrono::Duration>, SourceError> {
        let Some(selector) = self.properties.reminder.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(property) = resolve_property(&object.properties, selector, "reminder")? else {
            return Ok(Vec::new());
        };

        let tags = match &property.value {
            PropertyValue::MultiSelect { multi_select } => multi_select.as_slice(),
            // Tolerated so switching the property to a single select in
            // Anytype does not silently stop producing reminders.
            PropertyValue::Select { select } => std::slice::from_ref(select),
            other => {
                warnings.push(format!(
                    "object {} has reminder of format {:?}, expected a select; using the default lead time",
                    object.id,
                    other.format()
                ));
                return Ok(Vec::new());
            }
        };

        let mut leads = Vec::new();
        for tag in tags {
            match humantime::parse_duration(tag.name.trim())
                .ok()
                .and_then(|lead| chrono::Duration::from_std(lead).ok())
            {
                Some(lead) => leads.push(lead),
                None => warnings.push(format!(
                    "object {} has reminder option {:?}, which is not a duration like \"30m\" or \"1d\"; ignored",
                    object.id, tag.name
                )),
            }
        }
        Ok(leads)
    }

    /// A malformed date costs that one value, not the whole task.
    fn read_date(
        &self,
        object: &Object,
        selector: &PropertySelector,
        field: &str,
        warnings: &mut Vec<String>,
    ) -> Result<Option<AnytypeDate>, SourceError> {
        let Some(property) = resolve_property(&object.properties, selector, field)? else {
            return Ok(None);
        };
        Ok(match &property.value {
            PropertyValue::Date { date } => match AnytypeDate::parse(date) {
                Some(parsed) => Some(parsed),
                None => {
                    warnings.push(format!(
                        "object {} has an unparseable {field} date {date:?}; treated as absent",
                        object.id
                    ));
                    None
                }
            },
            other => {
                warnings.push(format!(
                    "object {} has {field} of format {:?}, expected a date; treated as absent",
                    object.id,
                    other.format()
                ));
                None
            }
        })
    }

    /// Unlike a date, a wrong `done` format is fatal: defaulting to `false`
    /// would mark every completed task as outstanding without any signal.
    fn read_done(&self, object: &Object) -> Result<bool, SourceError> {
        match resolve_property(&object.properties, &self.properties.done, "done")? {
            None => Ok(false),
            Some(property) => match &property.value {
                PropertyValue::Checkbox { checkbox } => Ok(*checkbox),
                other => Err(SourceError::Schema(format!(
                    "properties.done = \"{}\" resolves to a {:?} property, expected a checkbox",
                    self.properties.done,
                    other.format()
                ))),
            },
        }
    }
}

/// Resolves one selector against an object's properties.
///
/// A key is not guaranteed unique: Anytype lets two distinct properties share
/// one key, and a space can genuinely contain, say, a "Deadline" and a
/// "Scheduled" property both keyed `deadline`. Taking the first match would map
/// two different source values onto one calendar field and look entirely
/// normal in the output, so an ambiguous key is a hard error naming the
/// candidates. Ids are unique by construction and need no such check.
fn resolve_property<'a>(
    properties: &'a [PropertyWithValue],
    selector: &PropertySelector,
    field: &str,
) -> Result<Option<&'a PropertyWithValue>, SourceError> {
    match selector {
        PropertySelector::Id(id) => Ok(properties.iter().find(|property| &property.id == id)),
        PropertySelector::Key(key) => {
            let mut matches = properties.iter().filter(|property| &property.key == key);
            let first = matches.next();
            if let (Some(first), Some(second)) = (first, matches.next()) {
                return Err(SourceError::Schema(format!(
                    "properties.{field} = \"key:{key}\" is ambiguous: it matches \
                     id:{} (name {:?}) and id:{} (name {:?}); select one by id instead",
                    first.id, first.name, second.id, second.name
                )));
            }
            Ok(first)
        }
    }
}
