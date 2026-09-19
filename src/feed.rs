//! Refresh coordination, caching, and the stale fallback.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex as AsyncMutex, watch},
    time::Instant,
};
use tracing::{debug, error, info, warn};

use crate::{render::VTodoRenderer, source::TaskSource};

/// A published feed body plus its validators.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub body: Arc<str>,
    pub etag: String,
    pub last_modified: DateTime<Utc>,
    /// The same tasks as CalDAV resources, keyed by object id. Rendered in the
    /// same refresh as `body`, so the feed and a CalDAV client never see two
    /// different reads of Anytype.
    pub objects: Arc<BTreeMap<String, Resource>>,
    /// One view per assignee, by collection key. Empty unless an assignee
    /// selector is configured, and then the flat `objects` above still holds
    /// every task: these views add calendars, they do not replace the one.
    pub collections: Arc<BTreeMap<String, Collection>>,
    /// The resource name each task's UID is served under. A client that moves
    /// a task addresses it by a name of its own, and only the UID tells which
    /// task it means.
    pub names_by_uid: Arc<BTreeMap<String, String>>,
    /// Tasks with more than one assignee, by resource name. `collections`
    /// serves each in its first assignee's calendar; a person reading is shown
    /// it in their own instead.
    pub shared: Arc<BTreeMap<String, Vec<String>>>,
}

/// One calendar of its own, served at `/dav/calendars/tasks-<key>/`.
#[derive(Debug, Clone)]
pub struct Collection {
    /// What a client shows: the member's name, or «Без исполнителя».
    pub display_name: String,
    /// Whose calendar this is, as `Task::assignees` names them. The key is a
    /// digest and cannot be turned back into it. `None` for the unassigned.
    pub member_id: Option<String>,
    /// Whether that person may sign in (`TaskBatch::account_holders`).
    pub account: bool,
    /// Over this collection's members only, so a change in one calendar does
    /// not make every client re-list the others.
    pub ctag: String,
    /// A subset of `Snapshot::objects`, under the same resource names.
    pub objects: BTreeMap<String, Resource>,
}

/// The collection of tasks nobody is assigned.
pub const UNASSIGNED: &str = "unassigned";

/// The path segment a member id is served under. A participant id runs to
/// about 160 characters and a display name is neither stable nor URL-safe, so
/// the path carries a digest and the name travels in `displayname`.
pub fn collection_key(member_id: &str) -> String {
    let digest = Sha256::digest(member_id.as_bytes());
    hex::encode(&digest[..6])
}

/// One task as a CalDAV resource.
#[derive(Debug, Clone)]
pub struct Resource {
    pub object_id: String,
    pub ics: Arc<str>,
    pub etag: String,
}

/// The resource name a task is served under. A task created in Calino keeps
/// the name Calino computed from its UID, because Calino addresses it by that
/// name and ignores any `Location` the server returns.
pub fn resource_name(task: &crate::model::Task) -> String {
    match &task.ical_uid {
        Some(uid) => calino_filename(uid),
        None => task.object_id.clone(),
    }
}

/// Calino's `eventResourceFilename` without the `.ics`: `encodeURIComponent`,
/// then `!'()*~` percent-encoded too, then every `%` replaced by `~`.
pub fn calino_filename(uid: &str) -> String {
    let mut out = String::new();
    for byte in uid.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(byte as char),
            other => out.push_str(&format!("~{other:02X}")),
        }
    }
    out
}

/// What a caller should serve.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The most recent refresh attempt succeeded.
    Fresh(Snapshot),
    /// The most recent attempt failed but an earlier body is still held.
    Stale(Snapshot),
    /// No successful refresh has ever completed.
    Unavailable { category: &'static str },
}

#[derive(Debug, Default)]
struct State {
    snapshot: Option<Snapshot>,
    last_attempt: Option<Instant>,
    last_ok: bool,
    last_failure_category: Option<&'static str>,
}

pub struct FeedService {
    source: Arc<dyn TaskSource>,
    renderer: VTodoRenderer,
    state: Mutex<State>,
    /// Held by whichever caller is performing a refresh. Waiters do not queue
    /// on it; they watch for the generation to advance instead, so each one
    /// stays bounded by its own deadline rather than the refresher's.
    refresh_guard: AsyncMutex<()>,
    generation_tx: watch::Sender<u64>,
    min_refresh_interval: Duration,
    request_timeout: Duration,
}

impl FeedService {
    pub fn new(
        source: Arc<dyn TaskSource>,
        renderer: VTodoRenderer,
        min_refresh_interval: Duration,
        request_timeout: Duration,
    ) -> Self {
        let (generation_tx, _) = watch::channel(0u64);
        Self {
            source,
            renderer,
            state: Mutex::new(State::default()),
            refresh_guard: AsyncMutex::new(()),
            generation_tx,
            min_refresh_interval,
            request_timeout,
        }
    }

    /// Serves the feed, refreshing if the cached body has aged out.
    /// The language of the calendars this feed names.
    pub fn language(&self) -> crate::locale::Language {
        self.renderer.language()
    }

    pub async fn get(&self) -> Outcome {
        if let Some(outcome) = self.cached_within_interval() {
            debug!("serving cached feed within min_refresh_interval");
            return outcome;
        }

        let generation_before = *self.generation_tx.borrow();

        match self.refresh_guard.try_lock() {
            Ok(_guard) => {
                self.refresh().await;
                self.generation_tx.send_modify(|g| *g += 1);
                self.current_outcome()
            }
            Err(_) => {
                // Another caller is already refreshing. Consume that attempt's
                // outcome rather than starting a second Anytype request.
                let mut rx = self.generation_tx.subscribe();
                let waited = tokio::time::timeout(self.request_timeout, async {
                    while *rx.borrow_and_update() <= generation_before {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                })
                .await;

                if waited.is_err() {
                    warn!("timed out waiting for an in-progress refresh");
                }
                self.current_outcome()
            }
        }
    }

    /// One task rendered exactly as the snapshot would carry it.
    pub fn resource_for(&self, task: &crate::model::Task) -> Resource {
        let ics = self.renderer.render_one(task);
        Resource {
            object_id: task.object_id.clone(),
            etag: etag_for(&ics),
            ics: Arc::from(ics.as_str()),
        }
    }

    pub fn renderer(&self) -> &VTodoRenderer {
        &self.renderer
    }

    /// Forces the next `get` to read Anytype again. Called after a write, so a
    /// client's follow-up request sees the new ETag instead of a cached one.
    pub fn invalidate(&self) {
        let mut state = self.state.lock().expect("feed state poisoned");
        state.last_attempt = None;
        debug!("feed cache invalidated after a write");
    }

    fn cached_within_interval(&self) -> Option<Outcome> {
        let state = self.state.lock().expect("feed state poisoned");
        let last_attempt = state.last_attempt?;
        if last_attempt.elapsed() >= self.min_refresh_interval {
            return None;
        }
        let snapshot = state.snapshot.clone()?;
        Some(if state.last_ok {
            Outcome::Fresh(snapshot)
        } else {
            Outcome::Stale(snapshot)
        })
    }

    fn current_outcome(&self) -> Outcome {
        let state = self.state.lock().expect("feed state poisoned");
        match (&state.snapshot, state.last_ok) {
            (Some(snapshot), true) => Outcome::Fresh(snapshot.clone()),
            (Some(snapshot), false) => Outcome::Stale(snapshot.clone()),
            (None, _) => Outcome::Unavailable {
                category: state.last_failure_category.unwrap_or("anytype unavailable"),
            },
        }
    }

    async fn refresh(&self) {
        let started = Instant::now();
        // The deadline covers fetching and rendering together: a caller cares
        // about time to a response, not about which stage consumed it.
        let result = tokio::time::timeout(self.request_timeout, async {
            let batch = self.source.list_tasks().await?;
            Ok::<_, crate::source::SourceError>(batch)
        })
        .await;

        let mut batch = match result {
            Ok(Ok(batch)) => batch,
            Ok(Err(err)) => {
                error!(error = %err, "refresh failed");
                self.record_failure(err.public_category());
                return;
            }
            Err(_) => {
                error!(
                    timeout_ms = self.request_timeout.as_millis(),
                    "refresh timed out"
                );
                self.record_failure("anytype unavailable");
                return;
            }
        };

        for warning in &batch.warnings {
            warn!(warning = %warning, "source reported a malformed value");
        }
        if batch.tasks.is_empty() {
            // Not an error, but a subscribed client reads an empty calendar as
            // "every task was deleted", and a misconfigured space or a renamed
            // type looks exactly like this.
            warn!("refresh returned zero tasks; verify anytype.space_id and anytype.type_key");
        }

        keep_distinct_uids(&mut batch.tasks);
        let body = match self.renderer.render(&batch.tasks) {
            Ok(body) => body,
            Err(err) => {
                error!(error = %err, "render failed");
                self.record_failure("exporter misconfigured");
                return;
            }
        };

        let last_modified = batch
            .tasks
            .iter()
            .filter_map(|task| task.last_modified)
            .max()
            .unwrap_or_else(Utc::now);
        let etag = etag_for(&body);
        let objects: BTreeMap<String, Resource> = batch
            .tasks
            .iter()
            .map(|task| (resource_name(task), self.resource_for(task)))
            .collect();
        let collections = partition(&batch, &objects, self.renderer.language());
        let names_by_uid = batch
            .tasks
            .iter()
            .map(|task| (task.uid(), resource_name(task)))
            .collect();
        let shared = batch
            .tasks
            .iter()
            .filter(|task| task.assignees.len() > 1)
            .map(|task| (resource_name(task), task.assignees.clone()))
            .collect();

        info!(
            tasks = batch.tasks.len(),
            duration_ms = started.elapsed().as_millis(),
            "refresh succeeded"
        );

        let mut state = self.state.lock().expect("feed state poisoned");
        state.snapshot = Some(Snapshot {
            body: Arc::from(body.as_str()),
            etag,
            last_modified,
            objects: Arc::new(objects),
            collections: Arc::new(collections),
            names_by_uid: Arc::new(names_by_uid),
            shared: Arc::new(shared),
        });
        state.last_attempt = Some(Instant::now());
        state.last_ok = true;
        state.last_failure_category = None;
    }

    fn record_failure(&self, category: &'static str) {
        let mut state = self.state.lock().expect("feed state poisoned");
        state.last_attempt = Some(Instant::now());
        state.last_ok = false;
        state.last_failure_category = Some(category);
        if state.snapshot.is_some() {
            warn!(category, "serving the last successful feed");
        }
    }
}

/// The ctag of a calendar, over its members' etags, which are already
/// computed. The feed's own etag is a hash of the whole rendered body and would
/// move for every calendar whenever any task changed.
pub fn collection_ctag(objects: &BTreeMap<String, Resource>) -> String {
    let members = objects
        .values()
        .map(|resource| resource.etag.as_str())
        .collect::<Vec<_>>()
        .join(",");
    etag_for(&members)
}

/// Leaves out all but one of the tasks that share a UID. A copy of a task
/// keeps its `ical_uid`, and two components with one UID would fail the whole
/// render. The task whose UID is made of its own id is the original and stays;
/// otherwise the first one listed does.
fn keep_distinct_uids(tasks: &mut Vec<crate::model::Task>) {
    let mut kept: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for task in tasks.iter() {
        let original = task.ical_uid.is_none();
        match kept.get(&task.uid()) {
            Some((_, kept_original)) if *kept_original || !original => {}
            _ => {
                kept.insert(task.uid(), (task.object_id.clone(), original));
            }
        }
    }
    tasks.retain(|task| {
        let (keeper, _) = &kept[&task.uid()];
        if keeper != &task.object_id {
            warn!(uid = %task.uid(), left_out = %task.object_id, kept = %keeper, "two tasks share a UID; one is left out");
        }
        keeper == &task.object_id
    });
}

/// Splits the rendered resources into one calendar per person.
///
/// A person gets one when a member of the space stands behind them, which is
/// also what lets them sign in. The directory lists people the space merely
/// describes as well, and a calendar for each of them is noise. Anyone else a
/// task names first still gets a calendar, so that no task is left unserved. A task assigned to several people is served in the
/// first one's calendar only: Calino keeps one entry per UID across all its
/// calendars, so a task listed twice belongs to whichever was synced last.
fn partition(
    batch: &crate::model::TaskBatch,
    objects: &BTreeMap<String, Resource>,
    language: crate::locale::Language,
) -> BTreeMap<String, Collection> {
    if batch.members.is_empty() {
        return BTreeMap::new();
    }

    let empty = |display_name: String, member_id: Option<String>| Collection {
        display_name,
        account: member_id
            .as_ref()
            .is_some_and(|id| batch.account_holders.contains(id)),
        member_id,
        ctag: String::new(),
        objects: BTreeMap::new(),
    };
    let mut collections: BTreeMap<String, Collection> = batch
        .members
        .iter()
        .filter(|(id, _)| batch.account_holders.contains(*id))
        .map(|(id, name)| (collection_key(id), empty(name.clone(), Some(id.clone()))))
        .collect();
    collections.insert(
        UNASSIGNED.to_string(),
        empty(language.unassigned().to_string(), None),
    );

    for task in &batch.tasks {
        let name = resource_name(task);
        let Some(resource) = objects.get(&name) else {
            continue;
        };
        let Some(member_id) = task.assignees.first() else {
            if let Some(collection) = collections.get_mut(UNASSIGNED) {
                collection.objects.insert(name.clone(), resource.clone());
            }
            continue;
        };
        // Someone with no member behind them, or whom the directory no longer
        // lists, still holds tasks. They keep a calendar of their own, under
        // their name when the directory knows it, rather than having their
        // tasks vanish from every calendar.
        collections
            .entry(collection_key(member_id))
            .or_insert_with(|| {
                let name = batch.members.get(member_id).unwrap_or(member_id);
                empty(name.clone(), Some(member_id.clone()))
            })
            .objects
            .insert(name.clone(), resource.clone());
    }

    for (key, collection) in collections.iter_mut() {
        collection.ctag = collection_ctag(&collection.objects);
        // A hashed path says nothing in a log or a curl; this is what makes it
        // readable again.
        debug!(
            key = %key,
            name = %collection.display_name,
            tasks = collection.objects.len(),
            "collection"
        );
    }

    collections
}

pub fn etag_for(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    format!("\"{}\"", hex::encode(&digest[..16]))
}
