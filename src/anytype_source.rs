//! Reads tasks from Anytype. Schema writes live in `install`.

use std::collections::BTreeMap;

use anytype::{
    client::{AnytypeClient, ClientConfig},
    objects::Object,
    properties::{PropertyValue, PropertyWithValue, SetProperty},
    spaces::SpaceModel,
};
use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use tracing::{debug, error, info, warn};

use crate::{
    config::{AnytypeConfig, PropertiesConfig, PropertySelector},
    model::{AnytypeDate, Task, TaskBatch},
    source::{SourceError, TaskSource, TaskWriter},
    writeback::Patch,
};

/// The bundled type of the objects a space keeps for the people in it.
const PROFILE_TYPE_KEY: &str = "profile";

/// Space memberships are addressed as `_participant_<space>_<identity>`.
const PARTICIPANT_PREFIX: &str = "_participant_";

/// The people a task can be assigned to, read once per refresh.
#[derive(Default)]
struct Directory {
    /// Display name by the id a calendar is keyed on.
    names: BTreeMap<String, String>,
    /// Membership id to the id of that person's own object, so that one person
    /// gets one calendar however a task addresses them.
    profiles_by_participant: BTreeMap<String, String>,
    /// The people who may sign in.
    account_holders: std::collections::BTreeSet<String>,
}

pub struct AnytypeTaskSource {
    client: AnytypeClient,
    config: AnytypeConfig,
    properties: PropertiesConfig,
    /// Membership id to the person's own object, as of the last listing. A
    /// single task read before a write is normalized through it, so that the
    /// write compares the same ids the calendars are keyed by.
    profiles_by_participant: std::sync::RwLock<BTreeMap<String, String>>,
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

/// The space to serve when the configuration names none: the only one the
/// account is a member of, as its id and name. Chats are listed as spaces too
/// and are left out, since there are no tasks in them to serve.
pub async fn only_space(client: &AnytypeClient) -> Result<(String, String), String> {
    let spaces = client
        .spaces()
        .list()
        .await
        .map_err(|err| format!("cannot list the account's spaces: {err}"))?
        .collect_all()
        .await
        .map_err(|err| format!("cannot list the account's spaces: {err}"))?;
    pick_only_space(
        spaces
            .into_iter()
            .filter(|space| matches!(space.object, SpaceModel::Space))
            .map(|space| (space.id, space.name))
            .collect(),
    )
}

/// Exactly one space is the answer; none or several is an error that says
/// what to do, listing the spaces there are.
pub fn pick_only_space(spaces: Vec<(String, String)>) -> Result<(String, String), String> {
    match spaces.len() {
        1 => Ok(spaces.into_iter().next().expect("one space")),
        0 => Err(
            "anytype.space_id is not set and the account is a member of no space: \
                  let it into the space to serve, or set the id"
                .to_string(),
        ),
        count => Err(format!(
            "anytype.space_id is not set and the account is a member of {count} spaces; \
             set it to one of: {}",
            spaces
                .iter()
                .map(|(id, name)| format!("{name} ({id})"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
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
            profiles_by_participant: Default::default(),
        })
    }

    /// One person, one id, however a task addresses them.
    fn normalize_assignees(&self, task: &mut Task) {
        let table = self
            .profiles_by_participant
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for assignee in &mut task.assignees {
            if let Some(profile) = table.get(assignee) {
                *assignee = profile.clone();
            }
        }
    }
}

#[async_trait]
impl TaskSource for AnytypeTaskSource {
    async fn list_tasks(&self) -> Result<TaskBatch, SourceError> {
        let started = std::time::Instant::now();
        debug!(space_id = %self.config.space_id, type_key = %self.config.type_key, "anytype task listing started");
        let paged = self
            .client
            .search_in(&self.config.space_id)
            .types([self.config.type_key.as_str()])
            .execute()
            .await
            .map_err(|err| {
                error!(error = %err, error_debug = ?err, elapsed_ms = started.elapsed().as_millis(), "anytype task search failed");
                SourceError::Transport(err.to_string())
            })?;

        let mut objects = Vec::new();
        let mut stream = paged.into_stream();
        while let Some(item) = stream.next().await {
            let object = item.map_err(|err| {
                error!(error = %err, error_debug = ?err, read = objects.len(), "anytype task page failed");
                SourceError::Transport(err.to_string())
            })?;
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

        self.verify_schema(&live).await?;

        let archived = objects.len() - live.len();
        let mut batch = TaskBatch::default();
        for object in live {
            batch.tasks.push(self.to_task(object, &mut batch.warnings)?);
        }
        if self.properties.assignee.is_some() {
            let directory = self.read_directory().await?;
            *self
                .profiles_by_participant
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                directory.profiles_by_participant;
            for task in &mut batch.tasks {
                self.normalize_assignees(task);
            }
            batch.members = directory.names;
            batch.account_holders = directory.account_holders;
        }
        debug!(
            objects = objects.len(),
            archived,
            tasks = batch.tasks.len(),
            members = batch.members.len(),
            warnings = batch.warnings.len(),
            done = batch.tasks.iter().filter(|t| t.done).count(),
            elapsed_ms = started.elapsed().as_millis(),
            "anytype task listing finished"
        );
        Ok(batch)
    }
}

/// Writes address properties by the schema's keys (`install::SCHEMA`), not by
/// the configured id selectors: the facade's schema is the same in every
/// space, and a key is what the write API takes.
#[async_trait]
impl TaskWriter for AnytypeTaskSource {
    async fn get_task(&self, object_id: &str) -> Result<Option<Task>, SourceError> {
        let started = std::time::Instant::now();
        let object = match self
            .client
            .object(&self.config.space_id, object_id)
            .get()
            .await
        {
            Ok(object) => object,
            Err(anytype::error::AnytypeError::NotFound { .. }) => {
                debug!(object_id, "anytype get task: not found");
                return Ok(None);
            }
            Err(err) => {
                error!(object_id, error = %err, error_debug = ?err, "anytype get task failed");
                return Err(SourceError::Transport(err.to_string()));
            }
        };
        if object.archived {
            debug!(object_id, "anytype get task: archived");
            return Ok(None);
        }
        let mut warnings = Vec::new();
        let mut task = self.to_task(&object, &mut warnings)?;
        self.normalize_assignees(&mut task);
        for warning in warnings {
            warn!(%warning, "anytype get task reported a malformed value");
        }
        debug!(
            object_id,
            done = task.done,
            elapsed_ms = started.elapsed().as_millis(),
            "anytype get task finished"
        );
        Ok(Some(task))
    }

    async fn update_task(&self, object_id: &str, patch: &Patch) -> Result<(), SourceError> {
        let started = std::time::Instant::now();
        let mut request = self.client.update_object(&self.config.space_id, object_id);
        if let Some(name) = &patch.name {
            request = request.name(name.clone());
        }
        for property in patch_properties(patch) {
            request = request.add_property(property);
        }
        if let Some(tags) = self.tag_property(patch).await? {
            request = request.add_property(tags);
        }
        if let Some(assignees) = self.assignee_property(patch).await? {
            request = request.add_property(assignees);
        }
        info!(object_id, ?patch, "anytype update task started");
        request.update().await.map_err(|err| {
            error!(object_id, ?patch, error = %err, error_debug = ?err, "anytype update task failed");
            SourceError::Transport(err.to_string())
        })?;
        info!(
            object_id,
            elapsed_ms = started.elapsed().as_millis(),
            "anytype update task finished"
        );
        Ok(())
    }

    async fn create_task(&self, uid: &str, patch: &Patch) -> Result<String, SourceError> {
        let started = std::time::Instant::now();
        let mut request = self
            .client
            .new_object(&self.config.space_id, &self.config.type_key)
            .name(patch.name.clone().unwrap_or_else(|| "(unnamed)".into()))
            .set_text("ical_uid", uid);
        for property in patch_properties(patch) {
            request = request.add_property(property);
        }
        if let Some(tags) = self.tag_property(patch).await? {
            request = request.add_property(tags);
        }
        if let Some(assignees) = self.assignee_property(patch).await? {
            request = request.add_property(assignees);
        }
        info!(uid, ?patch, "anytype create task started");
        let object = request.create().await.map_err(|err| {
            error!(uid, ?patch, error = %err, error_debug = ?err, "anytype create task failed");
            SourceError::Transport(err.to_string())
        })?;
        info!(uid, object_id = %object.id, elapsed_ms = started.elapsed().as_millis(), "anytype create task finished");
        Ok(object.id)
    }

    async fn archive_task(&self, object_id: &str) -> Result<(), SourceError> {
        info!(object_id, "anytype archive task started");
        self.client
            .object(&self.config.space_id, object_id)
            .delete()
            .await
            .map_err(|err| {
                error!(object_id, error = %err, error_debug = ?err, "anytype archive task failed");
                SourceError::Transport(err.to_string())
            })?;
        info!(object_id, "anytype archive task finished");
        Ok(())
    }
}

/// The property values of a patch, in the API's JSON shape. A `null` date
/// clears the property (anytype-heart `processProperties`: a nil value is
/// written as null).
fn patch_properties(patch: &Patch) -> Vec<serde_json::Value> {
    let mut properties = Vec::new();
    if let Some(done) = patch.done {
        properties.push(serde_json::json!({ "key": "done", "checkbox": done }));
    }
    for (key, value) in [
        ("scheduled", &patch.scheduled),
        ("due_date", &patch.deadline),
    ] {
        if let Some(value) = value {
            properties.push(serde_json::json!({ "key": key, "date": value }));
        }
    }
    properties
}

impl AnytypeTaskSource {
    /// The tag property of a patch, by the key the configured tags selector
    /// names. Tags are written only when a selector is configured.
    async fn tag_property(&self, patch: &Patch) -> Result<Option<serde_json::Value>, SourceError> {
        let (Some(names), Some(selector)) = (&patch.tags, &self.properties.tags) else {
            return Ok(None);
        };
        let key = self.property_key(selector, "tags").await?;
        let ids =
            crate::events::option_ids(&self.client, &self.config.space_id, &key, names).await?;
        Ok(Some(serde_json::json!({ "key": key, "multi_select": ids })))
    }

    /// The assignee property of a patch. Unlike a tag there is nothing to
    /// create when an id is unknown: Anytype refuses the write.
    async fn assignee_property(
        &self,
        patch: &Patch,
    ) -> Result<Option<serde_json::Value>, SourceError> {
        let (Some(ids), Some(selector)) = (&patch.assignees, &self.properties.assignee) else {
            return Ok(None);
        };
        let key = self.property_key(selector, "assignee").await?;
        Ok(Some(serde_json::json!({ "key": key, "objects": ids })))
    }

    /// The key a selector names, which is what the write API takes.
    async fn property_key(
        &self,
        selector: &PropertySelector,
        role: &str,
    ) -> Result<String, SourceError> {
        match selector {
            PropertySelector::Key(key) => Ok(key.clone()),
            PropertySelector::Id(id) => self
                .client
                .properties(&self.config.space_id)
                .list()
                .await
                .map_err(|err| SourceError::Transport(err.to_string()))?
                .collect_all()
                .await
                .map_err(|err| SourceError::Transport(err.to_string()))?
                .into_iter()
                .find(|property| &property.id == id)
                .map(|property| property.key)
                .ok_or_else(|| SourceError::Schema(format!("{role} property {id} not found"))),
        }
    }

    /// Fails loudly when a configured selector matches nothing.
    ///
    /// A selector that silently resolves to nothing produces a feed where every
    /// task has no dates and none are complete — wrong output that looks like
    /// valid output. The error names what the type actually offers, which is
    /// how an operator discovers the opaque ids without a setup wizard.
    async fn verify_schema(&self, objects: &[&Object]) -> Result<(), SourceError> {
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
                unresolved.push((field, selector.clone()));
            }
        }

        if unresolved.is_empty() {
            return Ok(());
        }

        // Anytype omits a property nobody has filled in, so "no task carries
        // it" is not yet "the selector is wrong". The space's property list
        // tells the two apart; it is read only in this rare case.
        let properties = self
            .client
            .properties(&self.config.space_id)
            .list()
            .await
            .map_err(|err| SourceError::Transport(err.to_string()))?
            .collect_all()
            .await
            .map_err(|err| SourceError::Transport(err.to_string()))?;
        unresolved.retain(|(field, selector)| {
            let exists = properties.iter().any(|property| match selector {
                PropertySelector::Id(id) => &property.id == id,
                PropertySelector::Key(key) => &property.key == key,
            });
            if exists {
                debug!(field, %selector, "property exists but no task has a value for it");
            }
            !exists
        });
        if unresolved.is_empty() {
            return Ok(());
        }
        let unresolved: Vec<String> = unresolved
            .iter()
            .map(|(field, selector)| format!("properties.{field} = \"{selector}\""))
            .collect();

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

    /// The space's directory of people, which is what the assignee calendars
    /// are keyed by.
    ///
    /// Objects of type `profile` rather than the space's members: a member
    /// carries nothing but a name and does not survive moving a space between
    /// servers, so a space keeps objects of its own for the people in it, and
    /// that is what an assignee relation points at. The members list is a
    /// fallback for someone who has no object of their own.
    async fn read_directory(&self) -> Result<Directory, SourceError> {
        let paged = self
            .client
            .search_in(&self.config.space_id)
            .types([PROFILE_TYPE_KEY])
            .execute()
            .await
            .map_err(|err| SourceError::Transport(err.to_string()))?;
        let mut stream = paged.into_stream();
        let mut directory = Directory::default();
        while let Some(item) = stream.next().await {
            let object = item.map_err(|err| SourceError::Transport(err.to_string()))?;
            if object.archived {
                continue;
            }
            // A person's own page links their space membership, and that is the
            // only bridge between the two id forms an assignee can hold.
            if let Some(PropertyValue::Objects { objects }) =
                object.get_property("links").map(|property| &property.value)
            {
                for participant in objects
                    .iter()
                    .filter(|link| link.starts_with(PARTICIPANT_PREFIX))
                {
                    directory
                        .profiles_by_participant
                        .insert(participant.clone(), object.id.clone());
                }
            }
            let name = object
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(&object.id)
                .to_string();
            directory.names.insert(object.id, name);
        }

        let members = self
            .client
            .members(&self.config.space_id)
            .list()
            .await
            .map_err(|err| SourceError::Transport(err.to_string()))?
            .collect_all()
            .await
            .map_err(|err| SourceError::Transport(err.to_string()))?;
        for member in members.into_iter().filter(|member| member.is_active()) {
            // Their tasks are normalized onto the object, so an entry here
            // would only add a second, empty calendar for the same person.
            // An active member behind an object is also what an account takes:
            // leaving the space closes it at the next refresh.
            if let Some(profile) = directory.profiles_by_participant.get(&member.id) {
                directory.account_holders.insert(profile.clone());
                continue;
            }
            let name = member.display_name().to_string();
            directory.names.insert(member.id, name);
        }
        Ok(directory)
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
        let tags = self.read_tags(object, warnings)?;
        let assignees = self.read_assignees(object, warnings)?;
        let ical_uid = match object.get_property("ical_uid").map(|p| &p.value) {
            Some(PropertyValue::Text { text }) if !text.trim().is_empty() => {
                Some(text.trim().to_string())
            }
            _ => None,
        };

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
            tags,
            assignees,
            ical_uid,
            object_url: Some(object.get_link()),
            last_modified,
        })
    }

    /// Reads the ids of the assignee property, if one is configured. A wrong
    /// format costs the assignees of that task, never the task.
    fn read_assignees(
        &self,
        object: &Object,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<String>, SourceError> {
        let Some(selector) = self.properties.assignee.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(property) = resolve_property(&object.properties, selector, "assignee")? else {
            return Ok(Vec::new());
        };
        let ids: Vec<String> = match &property.value {
            PropertyValue::Objects { objects } => objects.clone(),
            other => {
                warnings.push(format!(
                    "object {} has assignee of format {:?}, expected objects; assignees omitted",
                    object.id,
                    other.format()
                ));
                return Ok(Vec::new());
            }
        };
        Ok(ids
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect())
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

    /// Reads tag option names, if the property is configured. A wrong format
    /// costs the tags of that task, never the task.
    fn read_tags(
        &self,
        object: &Object,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<String>, SourceError> {
        let Some(selector) = self.properties.tags.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(property) = resolve_property(&object.properties, selector, "tags")? else {
            return Ok(Vec::new());
        };
        let tags = match &property.value {
            PropertyValue::MultiSelect { multi_select } => multi_select.as_slice(),
            PropertyValue::Select { select } => std::slice::from_ref(select),
            other => {
                warnings.push(format!(
                    "object {} has tags of format {:?}, expected a select; tags omitted",
                    object.id,
                    other.format()
                ));
                return Ok(Vec::new());
            }
        };
        Ok(tags
            .iter()
            .map(|tag| tag.name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect())
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
