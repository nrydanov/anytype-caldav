//! Durable Web Push subscriptions and reminder delivery state.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use crate::push::{BrowserSubscription, SubscriptionKeys};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("cannot create the push state directory {path}: {source}")]
    Directory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create the push state file {path}: {source}")]
    File {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot open or initialize the push state database {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("push state schema version {found} is newer than supported version {supported}")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("push state database operation failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("push state lock is poisoned")]
    Poisoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderOutcome {
    Attempted,
    Expired,
}

impl ReminderOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attempted => "attempted",
            Self::Expired => "expired",
        }
    }
}

pub struct StateStore {
    connection: Mutex<Connection>,
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|source| StateError::Directory {
                path: parent.display().to_string(),
                source,
            })?;
        }

        create_private_file(path)?;
        let connection = Connection::open(path).map_err(|source| StateError::Open {
            path: path.display().to_string(),
            source,
        })?;
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|source| StateError::Open {
                path: path.display().to_string(),
                source,
            })?;
        if version > SCHEMA_VERSION {
            return Err(StateError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS subscriptions (
                    endpoint TEXT PRIMARY KEY NOT NULL,
                    p256dh TEXT NOT NULL,
                    auth TEXT NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS handled_reminders (
                    object_id TEXT NOT NULL,
                    trigger_at_ms INTEGER NOT NULL,
                    handled_at_ms INTEGER NOT NULL,
                    outcome TEXT NOT NULL CHECK (outcome IN ('attempted', 'expired')),
                    PRIMARY KEY (object_id, trigger_at_ms)
                );
                PRAGMA user_version = 1;",
            )
            .map_err(|source| StateError::Open {
                path: path.display().to_string(),
                source,
            })?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn upsert_subscription(
        &self,
        subscription: &BrowserSubscription,
    ) -> Result<(), StateError> {
        self.connection()?.execute(
            "INSERT INTO subscriptions (endpoint, p256dh, auth, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(endpoint) DO UPDATE SET
                p256dh = excluded.p256dh,
                auth = excluded.auth,
                updated_at_ms = excluded.updated_at_ms",
            params![
                subscription.endpoint,
                subscription.keys.p256dh,
                subscription.keys.auth,
                Utc::now().timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub fn subscriptions(&self) -> Result<Vec<BrowserSubscription>, StateError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT endpoint, p256dh, auth FROM subscriptions ORDER BY endpoint")?;
        let rows = statement.query_map([], |row| {
            Ok(BrowserSubscription {
                endpoint: row.get(0)?,
                keys: SubscriptionKeys {
                    p256dh: row.get(1)?,
                    auth: row.get(2)?,
                },
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StateError::from)
    }

    pub fn remove_subscription(&self, endpoint: &str) -> Result<bool, StateError> {
        let changed = self
            .connection()?
            .execute("DELETE FROM subscriptions WHERE endpoint = ?1", [endpoint])?;
        Ok(changed == 1)
    }

    pub fn subscription_count(&self) -> Result<usize, StateError> {
        let count: i64 =
            self.connection()?
                .query_row("SELECT COUNT(*) FROM subscriptions", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Atomically reserves one object/trigger pair. `false` means it was
    /// already handled by this or an earlier process.
    pub fn claim_reminder(
        &self,
        object_id: &str,
        trigger_at: DateTime<Utc>,
        outcome: ReminderOutcome,
    ) -> Result<bool, StateError> {
        let changed = self.connection()?.execute(
            "INSERT OR IGNORE INTO handled_reminders
             (object_id, trigger_at_ms, handled_at_ms, outcome)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                object_id,
                trigger_at.timestamp_millis(),
                Utc::now().timestamp_millis(),
                outcome.as_str(),
            ],
        )?;
        Ok(changed == 1)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StateError> {
        self.connection.lock().map_err(|_| StateError::Poisoned)
    }
}

fn create_private_file(path: &Path) -> Result<(), StateError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|source| StateError::File {
        path: path.display().to_string(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| StateError::File {
                path: path.display().to_string(),
                source,
            },
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ReminderOutcome, StateStore};
    use chrono::{TimeZone, Utc};

    use crate::push::{BrowserSubscription, SubscriptionKeys};

    fn subscription(p256dh: &str, auth: &str) -> BrowserSubscription {
        BrowserSubscription {
            endpoint: "https://push.example/subscription".into(),
            keys: SubscriptionKeys {
                p256dh: p256dh.into(),
                auth: auth.into(),
            },
        }
    }

    #[test]
    fn subscriptions_survive_reopen_and_upsert_by_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        StateStore::open(&path)
            .unwrap()
            .upsert_subscription(&subscription("old", "a"))
            .unwrap();
        StateStore::open(&path)
            .unwrap()
            .upsert_subscription(&subscription("new", "b"))
            .unwrap();

        let stored = StateStore::open(&path).unwrap().subscriptions().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].endpoint, "https://push.example/subscription");
        assert_eq!(stored[0].keys.p256dh, "new");
        assert_eq!(stored[0].keys.auth, "b");
    }

    #[test]
    fn removing_a_subscription_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let store = StateStore::open(&path).unwrap();
        store
            .upsert_subscription(&subscription("key", "auth"))
            .unwrap();
        assert!(
            store
                .remove_subscription("https://push.example/subscription")
                .unwrap()
        );
        drop(store);

        assert!(
            StateStore::open(&path)
                .unwrap()
                .subscriptions()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reminder_claim_survives_reopen_and_changed_trigger_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let at = Utc.with_ymd_and_hms(2026, 8, 30, 5, 0, 0).unwrap();
        assert!(
            StateStore::open(&path)
                .unwrap()
                .claim_reminder("task", at, ReminderOutcome::Attempted)
                .unwrap()
        );

        let reopened = StateStore::open(&path).unwrap();
        assert!(
            !reopened
                .claim_reminder("task", at, ReminderOutcome::Attempted)
                .unwrap()
        );
        assert!(
            reopened
                .claim_reminder(
                    "task",
                    at + chrono::Duration::hours(1),
                    ReminderOutcome::Attempted,
                )
                .unwrap()
        );
    }

    #[test]
    fn a_newer_schema_is_rejected_instead_of_downgraded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA user_version = 2;")
            .unwrap();
        drop(connection);

        let err = match StateStore::open(&path) {
            Ok(_) => panic!("newer schema must be rejected"),
            Err(err) => err,
        };
        assert!(
            matches!(
                err,
                super::StateError::UnsupportedSchema {
                    found: 2,
                    supported: 1
                }
            ),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state.sqlite3");
        StateStore::open(&path).unwrap();

        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
