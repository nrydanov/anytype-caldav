//! Who may use the facade: the shared login and the accounts per person.

use axum::http::{HeaderMap, header};
use base64::Engine;
use sha2::{Digest, Sha256};

use super::{NAMES, SETTINGS};
use crate::feed::Outcome;

/// Who may use the facade. The password is only ever held as a digest.
pub struct Credentials {
    username: String,
    password_digest: [u8; 32],
    /// Set when every person of the space has an account of their own.
    accounts: Option<crate::accounts::Accounts>,
}

/// Who a request was made by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reader {
    /// The one configured username and password: nobody in particular.
    Shared,
    /// A person of the space, by the id their calendar is kept under.
    Person(String),
}

impl Reader {
    /// Where this reader's client documents are kept. A person's are their
    /// own: Calino syncs its settings as one document per account, so people
    /// sharing a collection would overwrite each other's.
    pub(super) fn settings_storage(&self) -> String {
        match self {
            Reader::Shared => SETTINGS.to_string(),
            Reader::Person(id) => format!("{SETTINGS}{}/", crate::accounts::username(id)),
        }
    }

    /// Where this reader's names for the calendars are kept. Two instances,
    /// or two people, see the same collections under names of their own.
    pub(super) fn names_storage(&self) -> String {
        match self {
            Reader::Shared => NAMES.to_string(),
            Reader::Person(id) => format!("{NAMES}{}/", crate::accounts::username(id)),
        }
    }
}

impl Credentials {
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            username: username.to_string(),
            password_digest: Sha256::digest(password.as_bytes()).into(),
            accounts: None,
        }
    }

    /// Also accepts the account of every person who may sign in, derived from
    /// this secret (`accounts`).
    pub fn with_accounts(mut self, secret: &str) -> Self {
        self.accounts = Some(crate::accounts::Accounts::new(secret));
        self
    }

    /// Who signed in, if anyone. A person is looked up among the calendars of
    /// the current snapshot, so an account exists exactly as long as the
    /// directory says it does.
    pub async fn reader(
        &self,
        feed: &crate::feed::FeedService,
        headers: &HeaderMap,
    ) -> Option<Reader> {
        if self.accepts(headers) {
            return Some(Reader::Shared);
        }
        let accounts = self.accounts.as_ref()?;
        let (username, password) = basic(headers)?;
        let (Outcome::Fresh(snapshot) | Outcome::Stale(snapshot)) = feed.get().await else {
            return None;
        };
        let people = snapshot
            .collections
            .values()
            .filter(|collection| collection.account)
            .filter_map(|collection| collection.member_id.as_deref());
        accounts
            .person(&username, &password, people)
            .map(|id| Reader::Person(id.to_string()))
    }

    /// Checks an `Authorization: Basic …` header. The password comparison
    /// runs over fixed-length digests so its duration does not depend on how
    /// much of the password matched.
    pub fn accepts(&self, headers: &HeaderMap) -> bool {
        let Some((user, password)) = basic(headers) else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(password.as_bytes()).into();
        let same_password = digest
            .iter()
            .zip(self.password_digest.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
        same_password & (user == self.username)
    }
}

/// The username and password of an `Authorization: Basic …` header.
fn basic(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(user: &str, password: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {token}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn only_the_exact_credentials_are_accepted() {
        let credentials = Credentials::new("me", "s3cret:with:colons");
        assert!(credentials.accepts(&basic("me", "s3cret:with:colons")));
        assert!(!credentials.accepts(&basic("me", "s3cret")));
        assert!(!credentials.accepts(&basic("you", "s3cret:with:colons")));
        assert!(!credentials.accepts(&HeaderMap::new()));
    }
}
