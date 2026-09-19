//! Web Push delivery.
//!
//! The transport belongs to the browser vendor: Safari hands the client a
//! subscription endpoint on Apple's infrastructure, and this service only
//! signs and POSTs to it. Two independent crypto layers are involved — a VAPID
//! signature that proves *who is sending*, and payload encryption with keys
//! the browser generated, which means Apple relays ciphertext it cannot read.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use web_push::{
    ContentEncoding, HyperWebPushClient, SubscriptionInfo, VapidSignatureBuilder, WebPushClient,
    WebPushMessageBuilder,
};

use crate::state::{StateError, StateStore};

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("cannot read the VAPID private key at {path}: {source}")]
    KeyRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the VAPID private key is not a usable P-256 PEM key: {0}")]
    KeyInvalid(String),
    #[error("cannot write a new VAPID private key at {path}: {source}")]
    KeyWrite {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("building the push message failed: {0}")]
    Build(String),
    #[error("the push service rejected the message: {0}")]
    Rejected(String),
    #[error("could not reach the push service: {0}")]
    Transport(String),
}

/// A subscription exactly as the browser produced it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BrowserSubscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

/// What a notification carries. The PoC service worker reads the flat
/// fields; Safari reads the declarative envelope added by [`payload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// Opened when the notification is tapped.
    pub url: Option<String>,
    /// Collapses repeats for the same task rather than stacking them.
    pub tag: Option<String>,
    /// The day the reminder is about, for opening that day in the calendar.
    #[serde(skip)]
    pub day: Option<chrono::NaiveDate>,
}

/// The JSON sent to the browser. With an `app_url` it also carries the
/// Declarative Web Push envelope (`web_push: 8030`, WebKit, iOS 18.4): Safari
/// shows `notification` itself, with no service worker, and opens `navigate`
/// on tap. The flat fields stay for the PoC page's service worker.
pub fn payload(notification: &Notification, app_url: Option<&str>) -> serde_json::Value {
    let mut value = serde_json::to_value(notification).expect("plain struct");
    if let Some(app_url) = app_url {
        let navigate = match notification.day {
            Some(day) => format!("{app_url}?date={}", day.format("%Y-%m-%d")),
            None => app_url.to_string(),
        };
        let mut declarative = serde_json::json!({
            "title": notification.title,
            "body": notification.body,
            "navigate": navigate,
        });
        if let Some(tag) = &notification.tag {
            declarative["tag"] = serde_json::json!(tag);
        }
        value["web_push"] = serde_json::json!(8030);
        value["notification"] = declarative;
    }
    value
}

impl std::fmt::Debug for PushService {
    /// Hand-written so the private key can never reach a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushService")
            .field("vapid_pem", &"<redacted>")
            .field("public_key", &self.public_key)
            .field("subscriptions", &self.subscription_count())
            .finish()
    }
}

pub struct PushService {
    vapid_pem: Vec<u8>,
    public_key: String,
    state: Arc<StateStore>,
    app_url: Option<String>,
    client: HyperWebPushClient,
}

/// Makes the VAPID key at `path` when there is none: a P-256 key in SEC1 PEM,
/// readable by its owner only, in a directory made if missing. An existing
/// file is never touched, not even an unreadable one: a new key would cut off
/// every subscription made with the old. The key is made before the file, so
/// a failure cannot leave an empty file behind. Returns whether it made one.
pub fn ensure_vapid_key(path: &std::path::Path) -> Result<bool, PushError> {
    use std::io::Write;

    if path.exists() {
        return Ok(false);
    }
    let pem = openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1)
        .and_then(|group| openssl::ec::EcKey::generate(&group))
        .and_then(|key| key.private_key_to_pem())
        .map_err(|err| PushError::KeyInvalid(err.to_string()))?;

    let write_error = |source| PushError::KeyWrite {
        path: path.display().to_string(),
        source,
    };
    if let Some(directory) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(directory).map_err(write_error)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        // Made by someone else in the meantime: theirs stands.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => return Err(write_error(err)),
    };
    file.write_all(&pem).map_err(write_error)?;
    Ok(true)
}

impl PushService {
    pub fn load(
        private_key_path: &std::path::Path,
        state: Arc<StateStore>,
        app_url: Option<String>,
    ) -> Result<Arc<Self>, PushError> {
        let vapid_pem = std::fs::read(private_key_path).map_err(|source| PushError::KeyRead {
            path: private_key_path.display().to_string(),
            source,
        })?;

        // Derived once at startup: served to the client, which hands it to
        // Safari when creating the subscription. Safari binds it to that
        // subscription, and Apple later checks our signature against it.
        let partial = VapidSignatureBuilder::from_pem_no_sub(vapid_pem.as_slice())
            .map_err(|err| PushError::KeyInvalid(err.to_string()))?;
        let public_key = base64_url(&partial.get_public_key());

        Ok(Arc::new(Self {
            vapid_pem,
            public_key,
            state,
            app_url,
            client: HyperWebPushClient::new(),
        }))
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// Replaces any existing entry for the same endpoint: browsers re-issue a
    /// subscription on their own schedule, and a stale one only produces 410s.
    pub fn store(&self, subscription: BrowserSubscription) -> Result<(), StateError> {
        self.store_of(subscription, None)
    }

    /// Stores a subscription made by a person of the space, who then receives
    /// only the reminders of their own tasks.
    pub fn store_of(
        &self,
        subscription: BrowserSubscription,
        person: Option<&str>,
    ) -> Result<(), StateError> {
        self.state.upsert_subscription_of(&subscription, person)?;
        // Best-effort count: the subscription is already durable, so a failure
        // to read it back is a log-quality problem, not a rejected request.
        info!(
            count = self.subscription_count(),
            "stored a push subscription"
        );
        Ok(())
    }

    pub fn subscription_count(&self) -> usize {
        match self.state.subscription_count() {
            Ok(count) => count,
            Err(err) => {
                warn!(error = %err, "cannot count push subscriptions");
                0
            }
        }
    }

    /// Sends to every stored subscription, returning how many were delivered.
    pub async fn notify_all(&self, notification: &Notification) -> (usize, usize) {
        self.notify(notification, None).await
    }

    /// Sends a reminder of a task with these assignees; `None` reaches
    /// everyone, whoever they subscribed as.
    pub async fn notify(
        &self,
        notification: &Notification,
        assignees: Option<&[String]>,
    ) -> (usize, usize) {
        let targets = match assignees {
            Some(assignees) => self.state.subscriptions_reaching(assignees),
            None => self.state.subscriptions(),
        };
        let targets = match targets {
            Ok(targets) => targets,
            Err(err) => {
                warn!(error = %err, "cannot read push subscriptions");
                return (0, 0);
            }
        };

        let mut delivered = 0;
        let mut gone = Vec::new();
        for subscription in &targets {
            let started = std::time::Instant::now();
            let endpoint = truncate(&subscription.endpoint);
            match self.send(subscription, notification).await {
                Ok(()) => {
                    delivered += 1;
                    debug!(%endpoint, tag = ?notification.tag, elapsed_ms = started.elapsed().as_millis(), "push delivered");
                }
                Err(PushError::Rejected(message)) if message.contains("410") => {
                    // The browser dropped this subscription; stop keeping it.
                    warn!(%endpoint, %message, "subscription is gone");
                    gone.push(subscription.endpoint.clone());
                }
                Err(err) => {
                    warn!(%endpoint, error = %err, error_debug = ?err, elapsed_ms = started.elapsed().as_millis(), "push delivery failed")
                }
            }
        }

        for endpoint in gone {
            match self.state.remove_subscription(&endpoint) {
                Ok(removed) => {
                    info!(endpoint = %truncate(&endpoint), removed, "removed gone subscription")
                }
                Err(err) => {
                    warn!(error = %err, endpoint = %truncate(&endpoint), "cannot remove gone subscription")
                }
            }
        }

        (delivered, targets.len())
    }

    async fn send(
        &self,
        subscription: &BrowserSubscription,
        notification: &Notification,
    ) -> Result<(), PushError> {
        let info = SubscriptionInfo::new(
            subscription.endpoint.clone(),
            subscription.keys.p256dh.clone(),
            subscription.keys.auth.clone(),
        );

        let signature = VapidSignatureBuilder::from_pem(self.vapid_pem.as_slice(), &info)
            .map_err(|err| PushError::Build(err.to_string()))?
            .build()
            .map_err(|err| PushError::Build(err.to_string()))?;

        let payload = serde_json::to_vec(&payload(notification, self.app_url.as_deref()))
            .map_err(|err| PushError::Build(err.to_string()))?;

        let mut builder = WebPushMessageBuilder::new(&info);
        builder.set_payload(ContentEncoding::Aes128Gcm, &payload);
        builder.set_vapid_signature(signature);
        let message = builder
            .build()
            .map_err(|err| PushError::Build(err.to_string()))?;

        self.client.send(message).await.map_err(|err| match err {
            // The browser dropped this subscription; the caller prunes it.
            web_push::WebPushError::EndpointNotValid(_)
            | web_push::WebPushError::EndpointNotFound(_) => {
                PushError::Rejected(format!("410 ({err})"))
            }
            other => PushError::Transport(other.to_string()),
        })
    }
}

fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Endpoints are long and semi-sensitive; logs only need enough to correlate.
fn truncate(endpoint: &str) -> String {
    endpoint.chars().take(48).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateStore;

    fn test_state() -> Arc<StateStore> {
        let file = tempfile::NamedTempFile::new().unwrap();
        Arc::new(StateStore::open(file.path()).unwrap())
    }

    /// The key an operator generates with `openssl ecparam -genkey -name
    /// prime256v1` must load, and must yield the base64url public key the
    /// browser expects in `applicationServerKey`.
    #[test]
    fn loads_an_openssl_generated_key_and_derives_the_public_half() {
        let Ok(path) = std::env::var("VAPID_TEST_PEM") else {
            eprintln!("VAPID_TEST_PEM not set; skipping");
            return;
        };
        let state = test_state();
        let service =
            PushService::load(std::path::Path::new(&path), state.clone(), None).expect("key loads");

        let public = service.public_key();
        // An uncompressed P-256 point is 65 bytes -> 87 base64url chars.
        assert_eq!(public.len(), 87, "unexpected key length: {public}");
        assert!(
            !public.contains('+') && !public.contains('/') && !public.contains('='),
            "must be base64url without padding: {public}"
        );
        assert_eq!(service.subscription_count(), 0);

        service
            .store(BrowserSubscription {
                endpoint: "https://push.example/subscription".into(),
                keys: SubscriptionKeys {
                    p256dh: "key".into(),
                    auth: "auth".into(),
                },
            })
            .expect("subscription is persisted");
        drop(service);

        let reloaded =
            PushService::load(std::path::Path::new(&path), state, None).expect("reloads");
        assert_eq!(reloaded.subscription_count(), 1);
    }

    fn reminder() -> Notification {
        Notification {
            title: "Семинар".into(),
            body: "Начало через 15 минут — сегодня в 10:00".into(),
            url: Some("https://object.any.coop/obj".into()),
            tag: Some("obj".into()),
            day: chrono::NaiveDate::from_ymd_opt(2026, 9, 20),
        }
    }

    #[test]
    fn without_an_app_url_the_payload_stays_flat() {
        let value = payload(&reminder(), None);
        assert_eq!(value["title"], "Семинар");
        assert_eq!(value["url"], "https://object.any.coop/obj");
        assert!(value.get("web_push").is_none(), "{value}");
        assert!(value.get("day").is_none(), "{value}");
    }

    #[test]
    fn with_an_app_url_the_payload_is_also_declarative() {
        let value = payload(&reminder(), Some("https://calendar.example/"));
        assert_eq!(value["web_push"], 8030);
        assert_eq!(value["notification"]["title"], "Семинар");
        assert_eq!(value["notification"]["body"], value["body"]);
        assert_eq!(
            value["notification"]["navigate"],
            "https://calendar.example/?date=2026-09-20"
        );
        assert_eq!(value["notification"]["tag"], "obj");
        // The PoC worker keeps reading the flat fields.
        assert_eq!(value["tag"], "obj");

        let mut test = reminder();
        test.day = None;
        test.tag = None;
        let value = payload(&test, Some("https://calendar.example/"));
        assert_eq!(
            value["notification"]["navigate"],
            "https://calendar.example/"
        );
        assert!(value["notification"].get("tag").is_none(), "{value}");
    }

    /// A key made on first start loads like one made by hand, is readable by
    /// its owner only, and is never replaced by a later start.
    #[test]
    fn a_missing_key_is_made_once_and_then_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keys").join("vapid.pem");

        assert!(ensure_vapid_key(&path).unwrap(), "made on first call");
        let made = std::fs::read(&path).unwrap();
        assert!(String::from_utf8_lossy(&made).contains("BEGIN EC PRIVATE KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let service = PushService::load(&path, test_state(), None).unwrap();
        assert!(!service.public_key().is_empty());

        assert!(
            !ensure_vapid_key(&path).unwrap(),
            "left alone on the next call"
        );
        assert_eq!(std::fs::read(&path).unwrap(), made);
    }

    #[test]
    fn a_missing_key_file_is_an_actionable_error() {
        let err = PushService::load(
            std::path::Path::new("/nonexistent/vapid.pem"),
            test_state(),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, PushError::KeyRead { .. }), "{err:?}");
        assert!(err.to_string().contains("/nonexistent/vapid.pem"), "{err}");
    }

    #[test]
    fn a_non_key_file_is_rejected_rather_than_half_loaded() {
        let path = std::env::temp_dir().join("not-a-vapid-key.pem");
        std::fs::write(&path, b"hello").expect("write");
        let err = PushService::load(&path, test_state(), None).unwrap_err();
        assert!(matches!(err, PushError::KeyInvalid(_)), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }
}
