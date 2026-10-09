//! WebDAV responses: multistatus, properties, escaping and REPORT kinds.

use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};

use crate::feed::Resource;

#[derive(Debug, PartialEq)]
pub(super) enum Report {
    Tasks,
    TasksOnly,
    EventsOnly,
    SyncCollection,
}

/// Classifies a REPORT body by its text, as Calino's mock does. Prefixes are
/// free in XML, so only local names and attribute values are looked at.
pub(super) fn report_kind(body: &str) -> Report {
    if body.contains("sync-collection") {
        return Report::SyncCollection;
    }
    let asks_events = body.contains("\"VEVENT\"") || body.contains("'VEVENT'");
    let asks_tasks = body.contains("\"VTODO\"") || body.contains("'VTODO'");
    if asks_events && !asks_tasks {
        Report::EventsOnly
    } else if asks_tasks && !asks_events {
        Report::TasksOnly
    } else {
        Report::Tasks
    }
}

/// The hrefs a `calendar-multiget` REPORT names (RFC 4791 §7.9), or `None`
/// for any other REPORT. Read by its text, as `report_kind` does: a multiget
/// has no href outside the list of resources it asks for. A href given as a
/// whole URL is cut down to its path.
pub(super) fn multiget_hrefs(body: &str) -> Option<Vec<String>> {
    if !body.contains("calendar-multiget") {
        return None;
    }
    let path = |href: &str| match href.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |at| &rest[at..]).to_string(),
        None => href.to_string(),
    };
    Some(
        body.split("href>")
            .skip(1)
            .step_by(2)
            .filter_map(|piece| piece.rsplit_once('<'))
            .map(|(href, _)| path(unescape(href.trim()).as_str()))
            .collect(),
    )
}

/// The responses of a REPORT over `members`, each a href and its resource.
/// A multiget is answered with the members it names, in its order, and with
/// 404 for a href that names none; any other REPORT with every member.
pub(super) fn report_responses<'a>(
    hrefs: Option<&[String]>,
    members: impl Iterator<Item = (String, &'a Resource)>,
) -> Vec<String> {
    let data = |href: &str, resource: &Resource| {
        response(
            href,
            &[
                prop_text("d:getetag", &resource.etag),
                prop_text("d:getcontenttype", "text/calendar"),
                prop_text("c:calendar-data", &resource.ics),
            ],
        )
    };
    let Some(hrefs) = hrefs else {
        return members
            .map(|(href, resource)| data(&href, resource))
            .collect();
    };
    let mut found = vec![None; hrefs.len()];
    for (href, resource) in members {
        if let Some(at) = hrefs.iter().position(|asked| asked == &href) {
            found[at] = Some(data(&href, resource));
        }
    }
    hrefs
        .iter()
        .zip(found)
        .map(|(href, found)| {
            found.unwrap_or_else(|| {
                format!(
                    "<d:response><d:href>{}</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>",
                    escape(href)
                )
            })
        })
        .collect()
}

pub(super) fn collection_props(
    components: &[&str],
    name: &str,
    ctag: &str,
    writable: bool,
) -> Vec<String> {
    let components: String = components
        .iter()
        .map(|component| format!("<c:comp name=\"{component}\"/>"))
        .collect();
    vec![
        "<d:resourcetype><d:collection/><c:calendar/></d:resourcetype>".to_string(),
        prop_text("d:displayname", name),
        format!(
            "<c:supported-calendar-component-set>{components}</c:supported-calendar-component-set>"
        ),
        // Changes whenever any member changes, so Calino can skip an unchanged
        // collection without listing it.
        prop_text("cs:getctag", ctag),
        if writable {
            "<d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege><d:privilege><d:write-content/></d:privilege><d:privilege><d:bind/></d:privilege><d:privilege><d:unbind/></d:privilege></d:current-user-privilege-set>".to_string()
        } else {
            "<d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set>".to_string()
        },
    ]
}

pub(super) fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub(super) fn response(href: &str, props: &[String]) -> String {
    format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop>{}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        escape(href),
        props.concat()
    )
}

/// What a PROPFIND tells of a task or an event. Thunderbird fetches only the
/// members listed as `text/calendar` (`CalDavRequestHandlers.sys.mjs`).
pub(super) fn member_props(etag: &str) -> [String; 2] {
    [
        prop_text("d:getetag", etag),
        prop_text("d:getcontenttype", "text/calendar"),
    ]
}

pub(super) fn prop_text(name: &str, value: &str) -> String {
    format!("<{name}>{}</{name}>", escape(value))
}

pub(super) fn prop_href(name: &str, href: &str) -> String {
    format!("<{name}><d:href>{}</d:href></{name}>", escape(href))
}

pub(super) fn multistatus(responses: Vec<String>) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\" xmlns:cs=\"http://calendarserver.org/ns/\" xmlns:a=\"http://apple.com/ns/ical/\">{}</d:multistatus>",
        responses.concat()
    );
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
        .body(Body::from(body))
        .expect("valid response")
}

pub(super) fn options() -> Response {
    let mut response = status(StatusCode::OK);
    let out = response.headers_mut();
    out.insert(
        header::ALLOW,
        HeaderValue::from_static("OPTIONS, GET, HEAD, PROPFIND, REPORT, PUT, DELETE"),
    );
    out.insert("DAV", HeaderValue::from_static("1, 3, calendar-access"));
    response
}

pub(super) fn status(code: StatusCode) -> Response {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .expect("static response")
}

/// XML text escaping. `calendar-data` must be escaped, not raw: a SUMMARY
/// with `&` or `<` would otherwise break the document for the whole listing.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calino's own bodies, from `CalDAVClient.ts` and tsdav's calendar-query.
    #[test]
    fn reports_are_classified_by_their_text() {
        let todo = r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VTODO"/></c:comp-filter></c:filter></c:calendar-query>"#;
        let event = todo.replace("VTODO", "VEVENT");
        let sync = r#"<D:sync-collection xmlns:D="DAV:"><D:sync-token/><D:sync-level>1</D:sync-level></D:sync-collection>"#;
        assert_eq!(report_kind(todo), Report::TasksOnly);
        assert_eq!(report_kind("<c:calendar-query/>"), Report::Tasks);
        assert_eq!(report_kind(&event), Report::EventsOnly);
        assert_eq!(report_kind(sync), Report::SyncCollection);
    }

    /// Thunderbird's body after a PUT (`CalDavRequestHandlers.sys.mjs`).
    #[test]
    fn a_multiget_names_the_resources_it_asks_for() {
        let multiget = r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>/dav/calendars/events/a.ics</D:href><D:href>https://host:8444/dav/calendars/events/b.ics</D:href></C:calendar-multiget>"#;
        assert_eq!(
            multiget_hrefs(multiget),
            Some(vec![
                "/dav/calendars/events/a.ics".to_string(),
                "/dav/calendars/events/b.ics".to_string()
            ])
        );
        assert_eq!(multiget_hrefs("<c:calendar-query/>"), None);
    }

    #[test]
    fn calendar_data_is_escaped() {
        let prop = prop_text("c:calendar-data", "SUMMARY:Tom & Jerry <3");
        assert_eq!(
            prop,
            "<c:calendar-data>SUMMARY:Tom &amp; Jerry &lt;3</c:calendar-data>"
        );
    }
}
