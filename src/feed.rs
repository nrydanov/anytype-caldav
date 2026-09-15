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
}

/// One task as a CalDAV resource.
#[derive(Debug, Clone)]
pub struct Resource {
    pub ics: Arc<str>,
    pub etag: String,
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

        let batch = match result {
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
        let objects: BTreeMap<String, Resource> = self
            .renderer
            .render_each(&batch.tasks)
            .into_iter()
            .map(|(id, ics)| {
                let etag = etag_for(&ics);
                (
                    id,
                    Resource {
                        ics: Arc::from(ics.as_str()),
                        etag,
                    },
                )
            })
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

fn etag_for(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    format!("\"{}\"", hex::encode(&digest[..16]))
}
