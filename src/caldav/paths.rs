//! Which collection and resource a path under `/dav/` addresses.

use super::{EVENTS, TASKS, TASKS_PREFIX};

/// The task collection a path addresses: the key that follows `tasks-`, or the
/// empty string for the flat one. A key is a digest or the reserved
/// `unassigned`, so anything else is not a task path at all.
pub(super) fn tasks_key(path: &str) -> Option<&str> {
    if path.starts_with(TASKS) {
        return Some("");
    }
    let (key, _) = path.strip_prefix(TASKS_PREFIX)?.split_once('/')?;
    key.chars()
        .all(|c| c.is_ascii_alphanumeric())
        .then_some(key)
}

/// A task or event collection, which a reader may rename.
pub(super) fn is_calendar(path: &str) -> bool {
    path == EVENTS || tasks_key(path).is_some_and(|key| collection_path(key) == path)
}

pub(super) fn collection_path(key: &str) -> String {
    if key.is_empty() {
        TASKS.to_string()
    } else {
        format!("{TASKS_PREFIX}{key}/")
    }
}

/// `/dav/calendars/tasks/<name>.ics` → `("", <name>)`. A name is an object id
/// or a name Calino derived from a UID, so only `[A-Za-z0-9._~-]` is accepted;
/// `..` and anything path-like is refused rather than looked up.
pub(super) fn object_id(path: &str) -> Option<(&str, &str)> {
    let key = tasks_key(path)?;
    let name = resource_name_in(path, &collection_path(key))?;
    Some((key, name))
}

pub(super) fn resource_name_in<'a>(path: &'a str, collection: &str) -> Option<&'a str> {
    let name = path.strip_prefix(collection)?.strip_suffix(".ics")?;
    let plain = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'));
    (!name.is_empty() && plain && !name.contains("..")).then_some(name)
}

pub(super) fn href_for(key: &str, object_id: &str) -> String {
    format!("{}{object_id}.ics", collection_path(key))
}

/// Collections are addressed with a trailing slash; accept them without one.
pub(super) fn normalize(path: &str) -> String {
    match path {
        "/dav"
        | "/dav/principal"
        | "/dav/calendars"
        | "/dav/calendars/events"
        | "/dav/calendars/calino-settings" => format!("{path}/"),
        // There is one task collection per assignee, so they cannot be listed.
        other
            if other
                .strip_prefix("/dav/calendars/tasks")
                .is_some_and(|rest| !rest.contains('/')) =>
        {
            format!("{other}/")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_object_ids_are_resources() {
        assert_eq!(
            object_id("/dav/calendars/tasks/bafyreiabc123.ics"),
            Some(("", "bafyreiabc123"))
        );
        assert_eq!(object_id("/dav/calendars/tasks/../x.ics"), None);
        assert_eq!(
            object_id("/dav/calendars/tasks/0b9a4f2c-5d1e-4c3a-9f7e-2a6b8c1d0e3f.ics"),
            Some(("", "0b9a4f2c-5d1e-4c3a-9f7e-2a6b8c1d0e3f"))
        );
        assert_eq!(object_id("/dav/calendars/tasks/.ics"), None);
        assert_eq!(object_id("/dav/calendars/tasks/abc"), None);
    }

    #[test]
    fn a_grouped_collection_carries_its_key() {
        assert_eq!(tasks_key("/dav/calendars/tasks/"), Some(""));
        assert_eq!(
            tasks_key("/dav/calendars/tasks-a1b2c3d4e5f6/"),
            Some("a1b2c3d4e5f6")
        );
        assert_eq!(
            tasks_key("/dav/calendars/tasks-unassigned/"),
            Some("unassigned")
        );
        assert_eq!(tasks_key("/dav/calendars/tasks-a1b2"), None);
        assert_eq!(tasks_key("/dav/calendars/events/"), None);
        // A key with a path behind it names no collection and no resource, so
        // both the collection arms and the member arms miss it.
        assert_eq!(object_id("/dav/calendars/tasks-a1b2/../x.ics"), None);
        assert_eq!(
            object_id("/dav/calendars/tasks-unassigned/bafyreiabc123.ics"),
            Some(("unassigned", "bafyreiabc123"))
        );
    }
}
