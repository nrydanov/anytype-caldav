//! Web Push delivery.
//!
//! The transport belongs to the browser vendor: Safari hands the client a
//! subscription endpoint on Apple's infrastructure, and this service only
//! signs and POSTs to it. Two independent crypto layers are involved — a VAPID
//! signature that proves *who is sending*, and payload encryption with keys
//! the browser generated, which means Apple relays ciphertext it cannot read.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
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

/// What a notification carries. The service worker reads these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// Opened when the notification is tapped.
    pub url: Option<String>,
    /// Collapses repeats for the same task rather than stacking them.
    pub tag: Option<String>,
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
    client: HyperWebPushClient,
}

impl PushService {
    pub fn load(
        private_key_path: &std::path::Path,
        state: Arc<StateStore>,
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
            client: HyperWebPushClient::new(),
        }))
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// Replaces any existing entry for the same endpoint: browsers re-issue a
    /// subscription on their own schedule, and a stale one only produces 410s.
    pub fn store(&self, subscription: BrowserSubscription) -> Result<(), StateError> {
        self.state.upsert_subscription(&subscription)?;
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
        let targets = match self.state.subscriptions() {
            Ok(targets) => targets,
            Err(err) => {
                warn!(error = %err, "cannot read push subscriptions");
                return (0, 0);
            }
        };

        let mut delivered = 0;
        let mut gone = Vec::new();
        for subscription in &targets {
            match self.send(subscription, notification).await {
                Ok(()) => delivered += 1,
                Err(PushError::Rejected(message)) if message.contains("410") => {
                    // The browser dropped this subscription; stop keeping it.
                    warn!(endpoint = %truncate(&subscription.endpoint), "subscription is gone");
                    gone.push(subscription.endpoint.clone());
                }
                Err(err) => warn!(error = %err, "push delivery failed"),
            }
        }

        for endpoint in gone {
            if let Err(err) = self.state.remove_subscription(&endpoint) {
                warn!(error = %err, endpoint = %truncate(&endpoint), "cannot remove gone subscription");
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

        let payload =
            serde_json::to_vec(notification).map_err(|err| PushError::Build(err.to_string()))?;

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
            PushService::load(std::path::Path::new(&path), state.clone()).expect("key loads");

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

        let reloaded = PushService::load(std::path::Path::new(&path), state).expect("reloads");
        assert_eq!(reloaded.subscription_count(), 1);
    }

    #[test]
    fn a_missing_key_file_is_an_actionable_error() {
        let err = PushService::load(std::path::Path::new("/nonexistent/vapid.pem"), test_state())
            .unwrap_err();
        assert!(matches!(err, PushError::KeyRead { .. }), "{err:?}");
        assert!(err.to_string().contains("/nonexistent/vapid.pem"), "{err}");
    }

    #[test]
    fn a_non_key_file_is_rejected_rather_than_half_loaded() {
        let path = std::env::temp_dir().join("not-a-vapid-key.pem");
        std::fs::write(&path, b"hello").expect("write");
        let err = PushService::load(&path, test_state()).unwrap_err();
        assert!(matches!(err, PushError::KeyInvalid(_)), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }
}
