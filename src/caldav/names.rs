//! Names a reader gives the calendars, kept per reader.

use axum::{http::StatusCode, response::Response};
use tracing::{info, warn};

use super::xml::{multistatus, response, status, unescape};
use crate::{http::AppState, state::StateStore};

/// The name a reader gave a calendar, or the one the server gives it.
pub(super) fn named(state: &AppState, names: &str, path: &str, default: &str) -> String {
    state
        .documents
        .as_ref()
        .and_then(|store| store.document(names, path).ok().flatten())
        .unwrap_or_else(|| default.to_string())
}

/// Keeps the name a reader gave a calendar. Calino renames with a PROPPATCH
/// of `displayname` alone and reads the name back on every sync, so a name the
/// server does not keep is gone by the next one. An empty name, or a
/// `displayname` under `remove`, returns the calendar to its own name.
pub(super) fn rename(store: &StateStore, names: &str, path: &str, body: &str) -> Response {
    let Some(name) = display_name(body) else {
        info!(path, "caldav proppatch without a displayname refused");
        return status(StatusCode::FORBIDDEN);
    };
    let name = name.trim();
    let stored = if name.is_empty() {
        store.delete_document(names, path).map(|_| ())
    } else {
        store.put_document(names, path, name)
    };
    if let Err(err) = stored {
        warn!(path, %err, "caldav rename not stored");
        return status(StatusCode::INTERNAL_SERVER_ERROR);
    }
    info!(path, name, "caldav calendar renamed");
    multistatus(vec![response(path, &["<d:displayname/>".to_string()])])
}

/// The text of the first `displayname` element in a request body, whatever
/// its namespace prefix; empty for `<displayname/>`.
pub(super) fn display_name(body: &str) -> Option<String> {
    let mut rest = body;
    loop {
        let at = rest.find("displayname")?;
        let before = rest[..at].chars().next_back();
        let after = &rest[at + "displayname".len()..];
        rest = after;
        if !matches!(before, Some('<' | ':')) || !after.starts_with(['>', ' ', '/']) {
            continue;
        }
        let open_end = after.find('>')?;
        if after[..open_end].ends_with('/') {
            return Some(String::new());
        }
        let text = &after[open_end + 1..];
        let text = &text[..text.find('<')?];
        return Some(unescape(text));
    }
}
