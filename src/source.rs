//! The boundary between the application and whatever supplies tasks.

use async_trait::async_trait;

use crate::{
    model::{Task, TaskBatch},
    writeback::Patch,
};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The source could not be reached or answered with a transport failure.
    #[error("anytype request failed: {0}")]
    Transport(String),
    /// The configuration does not match the live schema. Distinguished from
    /// `Transport` because retrying will never fix it.
    #[error("configuration does not match the anytype schema: {0}")]
    Schema(String),
    /// More objects than `max_objects`.
    #[error("{0}")]
    TooManyObjects(String),
}

impl SourceError {
    /// The category shown over HTTP. Detailed diagnostics stay in the log so
    /// nothing from the API key or an upstream response body can leak.
    pub fn public_category(&self) -> &'static str {
        match self {
            Self::Transport(_) => "anytype unavailable",
            Self::Schema(_) => "exporter misconfigured",
            Self::TooManyObjects(_) => "source too large",
        }
    }
}

#[async_trait]
pub trait TaskSource: Send + Sync + 'static {
    async fn list_tasks(&self) -> Result<TaskBatch, SourceError>;
}

/// Writes coming back from a calendar client.
#[async_trait]
pub trait TaskWriter: Send + Sync + 'static {
    /// A fresh read of one task; `None` when it is gone or archived.
    async fn get_task(&self, object_id: &str) -> Result<Option<Task>, SourceError>;
    async fn update_task(&self, object_id: &str, patch: &Patch) -> Result<(), SourceError>;
    /// Creates a task that remembers the client's UID; returns its object id.
    async fn create_task(&self, uid: &str, patch: &Patch) -> Result<String, SourceError>;
    /// Archives, which Anytype's API calls delete and keeps in the Bin.
    async fn archive_task(&self, object_id: &str) -> Result<(), SourceError>;
}
