//! An account per person, derived and never stored.
//!
//! The username is the key of that person's calendar and the password is an
//! HMAC-SHA256 (RFC 2104) of the person's id under one server secret. A person
//! who joins the space has an account after the next refresh, and changing the
//! secret changes every password at once.

use anytype::{client::AnytypeClient, properties::PropertyValue};
use futures::StreamExt;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use crate::{feed::collection_key, source::SourceError};

/// The bundled type of the objects a space keeps for the people in it.
const PROFILE_TYPE_KEY: &str = "profile";

/// Space memberships are addressed as `_participant_<space>_<identity>`.
const PARTICIPANT_PREFIX: &str = "_participant_";

/// RFC 4648 base32, in lower case: a password is typed or pasted once, and
/// lower case reads better in a message.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Ten bytes are eighty bits, which base32 writes as sixteen characters with
/// no padding.
const PASSWORD_BYTES: usize = 10;

/// The name a person signs in with: the key of their own calendar.
pub fn username(person_id: &str) -> String {
    collection_key(person_id)
}

/// The password of one person under one secret.
pub fn password(secret: &[u8], person_id: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(person_id.as_bytes());
    let digest = mac.finalize().into_bytes();
    base32(&digest[..PASSWORD_BYTES])
}

fn base32(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            text.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        text.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    text
}

/// The people who get an account, as `(person id, name)` sorted by name: every
/// `profile` whose `links` holds an active member of the space. A person the
/// space describes but who is not in it has nowhere to sign in from, and a
/// member with no object of their own has no calendar key of a stable kind.
///
/// Mirrors `AnytypeTaskSource::read_directory`, which is private to the source
/// and keeps every profile, because a calendar is served for all of them.
pub async fn holders(
    client: &AnytypeClient,
    space_id: &str,
) -> Result<Vec<(String, String)>, SourceError> {
    let transport = |err: anytype::error::AnytypeError| SourceError::Transport(err.to_string());
    let active: std::collections::BTreeSet<String> = client
        .members(space_id)
        .list()
        .await
        .map_err(transport)?
        .collect_all()
        .await
        .map_err(transport)?
        .into_iter()
        .filter(|member| member.is_active())
        .map(|member| member.id)
        .collect();

    let paged = client
        .search_in(space_id)
        .types([PROFILE_TYPE_KEY])
        .execute()
        .await
        .map_err(transport)?;
    let mut stream = paged.into_stream();
    let mut people = Vec::new();
    while let Some(item) = stream.next().await {
        let object = item.map_err(transport)?;
        if object.archived {
            continue;
        }
        let is_member = matches!(
            object.get_property("links").map(|property| &property.value),
            Some(PropertyValue::Objects { objects }) if objects
                .iter()
                .any(|link| link.starts_with(PARTICIPANT_PREFIX) && active.contains(link))
        );
        if !is_member {
            continue;
        }
        let name = object
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&object.id)
            .to_string();
        people.push((object.id, name));
    }
    people.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(people)
}

/// Checks a username and a password against the people of the space.
pub struct Accounts {
    secret: Vec<u8>,
}

impl Accounts {
    pub fn new(secret: &str) -> Self {
        Self {
            secret: secret.as_bytes().to_vec(),
        }
    }

    pub fn password_of(&self, person_id: &str) -> String {
        password(&self.secret, person_id)
    }

    /// The person these credentials belong to, out of `people`. Every person
    /// is checked and the passwords are compared over fixed-length digests, so
    /// the duration says neither where the match was nor how much of the
    /// password matched.
    pub fn person<'a>(
        &self,
        username: &str,
        password: &str,
        people: impl Iterator<Item = &'a str>,
    ) -> Option<&'a str> {
        let sent: [u8; 32] = Sha256::digest(password.as_bytes()).into();
        let mut found = None;
        for person_id in people {
            let expected: [u8; 32] = Sha256::digest(self.password_of(person_id).as_bytes()).into();
            let same_password = sent
                .iter()
                .zip(expected.iter())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
            if same_password & (self::username(person_id) == username) {
                found = Some(person_id);
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "bafyreialice";
    const BOB: &str = "bafyreibob";

    #[test]
    fn a_password_is_the_same_every_time() {
        assert_eq!(password(b"secret", ALICE), password(b"secret", ALICE));
        assert_ne!(password(b"secret", ALICE), password(b"secret", BOB));
    }

    #[test]
    fn another_secret_gives_another_password() {
        assert_ne!(password(b"secret", ALICE), password(b"other", ALICE));
    }

    #[test]
    fn a_password_is_sixteen_base32_characters() {
        let password = password(b"secret", ALICE);
        assert_eq!(password.len(), 16, "{password}");
        assert!(
            password.bytes().all(|c| ALPHABET.contains(&c)),
            "{password}"
        );
    }

    #[test]
    fn base32_matches_the_rfc_4648_vectors() {
        // The vectors of RFC 4648 section 10, lower-cased, without padding.
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "my");
        assert_eq!(base32(b"fo"), "mzxq");
        assert_eq!(base32(b"foo"), "mzxw6");
        assert_eq!(base32(b"foob"), "mzxw6yq");
        assert_eq!(base32(b"fooba"), "mzxw6ytb");
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn the_right_pair_names_the_person() {
        let accounts = Accounts::new("secret");
        let people = [ALICE, BOB];
        assert_eq!(
            accounts.person(
                &username(BOB),
                &accounts.password_of(BOB),
                people.iter().copied()
            ),
            Some(BOB)
        );
    }

    #[test]
    fn a_wrong_password_or_an_unknown_username_names_nobody() {
        let accounts = Accounts::new("secret");
        let people = [ALICE, BOB];
        assert_eq!(
            accounts.person(&username(BOB), "wrong", people.iter().copied()),
            None
        );
        // Somebody else's password under one's own name.
        assert_eq!(
            accounts.person(
                &username(BOB),
                &accounts.password_of(ALICE),
                people.iter().copied()
            ),
            None
        );
        assert_eq!(
            accounts.person(
                &username("bafyreicarol"),
                &accounts.password_of("bafyreicarol"),
                people.iter().copied()
            ),
            None
        );
    }
}
