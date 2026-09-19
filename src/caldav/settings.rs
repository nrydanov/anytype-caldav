//! `calino-settings/`: the client's own documents, kept outside Anytype.

use axum::{
    body::Body,
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use tracing::{debug, error, info, warn};

use super::paths::resource_name_in;
use super::xml::{collection_props, multistatus, prop_text, response, status};
use super::{SETTINGS, header_text, precondition_failed};
use crate::{feed::etag_for, state::StateStore};

const SETTINGS_NAMESPACE: &str = "http://calino.app/ns/";

const SETTINGS_DISPLAY_NAME: &str = "Calino Settings";

/// A settings document is a few kilobytes; this only stops a runaway client.
const MAX_DOCUMENT_BYTES: usize = 512 * 1024;

pub(super) fn settings_collection_props(store: &StateStore, storage: &str) -> Vec<String> {
    let ctag = store
        .documents(storage)
        .map(|documents| {
            etag_for(
                &documents
                    .iter()
                    .map(|(name, body)| format!("{name}:{body}"))
                    .collect::<Vec<_>>()
                    .join(","),
            )
        })
        .unwrap_or_else(|_| "\"unknown\"".to_string());
    let mut props = collection_props(&["VEVENT"], SETTINGS_DISPLAY_NAME, &ctag, true);
    // The marker Calino looks for; without it the client makes a calendar of
    // its own, which this server does not allow.
    props.push(format!(
        "<C:X-CALINO-SETTINGS-CALENDAR xmlns:C=\"{SETTINGS_NAMESPACE}\">1</C:X-CALINO-SETTINGS-CALENDAR>"
    ));
    props
}

fn document_name(path: &str) -> Option<&str> {
    resource_name_in(path, SETTINGS)
}

/// `storage` is where this reader's documents are kept (`Reader::settings_storage`);
/// the path a client sees is the same for everyone.
pub(super) fn settings_route(
    store: &StateStore,
    storage: &str,
    method: &Method,
    path: &str,
    depth: &str,
    headers: &HeaderMap,
    body: &str,
) -> Response {
    let name = document_name(path).map(str::to_string);
    let documents = |store: &StateStore| store.documents(storage).unwrap_or_default();
    match (method.as_str(), path, name) {
        ("PROPFIND", SETTINGS, _) => {
            let mut responses = vec![response(
                SETTINGS,
                &settings_collection_props(store, storage),
            )];
            if depth == "1" {
                responses.extend(documents(store).into_iter().map(|(name, body)| {
                    response(
                        &format!("{SETTINGS}{name}.ics"),
                        &[prop_text("d:getetag", &etag_for(&body))],
                    )
                }));
            }
            multistatus(responses)
        }
        // Calino filters by UID; every document of this collection is its own,
        // so the filter needs no reading.
        ("REPORT", SETTINGS, _) => multistatus(
            documents(store)
                .into_iter()
                .map(|(name, body)| {
                    response(
                        &format!("{SETTINGS}{name}.ics"),
                        &[
                            prop_text("d:getetag", &etag_for(&body)),
                            prop_text("d:getcontenttype", "text/calendar"),
                            prop_text("c:calendar-data", &body),
                        ],
                    )
                })
                .collect(),
        ),
        // The collection is already marked; a client setting properties on it
        // is told the write went nowhere rather than that it failed.
        ("PROPPATCH", SETTINGS, _) => multistatus(vec![response(SETTINGS, &[])]),
        ("GET" | "HEAD" | "PROPFIND", _, Some(name)) => {
            let Some(body) = store.document(storage, &name).ok().flatten() else {
                debug!(resource = %name, "caldav settings document not found");
                return status(StatusCode::NOT_FOUND);
            };
            let etag = etag_for(&body);
            if method.as_str() == "PROPFIND" {
                return multistatus(vec![response(
                    &format!("{SETTINGS}{name}.ics"),
                    &[prop_text("d:getetag", &etag)],
                )]);
            }
            let payload = if method == Method::HEAD {
                Body::empty()
            } else {
                Body::from(body)
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/calendar; charset=utf-8")
                .header(header::ETAG, etag)
                .body(payload)
                .expect("valid response")
        }
        ("PUT", _, Some(name)) => {
            if body.len() > MAX_DOCUMENT_BYTES {
                warn!(resource = %name, bytes = body.len(), "caldav settings document too large");
                return status(StatusCode::PAYLOAD_TOO_LARGE);
            }
            let current = store.document(storage, &name).ok().flatten();
            let if_match = header_text(headers, header::IF_MATCH).filter(|v| v != "*");
            let if_none_match = header_text(headers, header::IF_NONE_MATCH);
            if if_none_match.as_deref() == Some("*") && current.is_some() {
                return precondition_failed("resource exists");
            }
            if let Some(expected) = &if_match {
                let now = current.as_deref().map(etag_for);
                if now.as_ref() != Some(expected) {
                    warn!(resource = %name, client_etag = %expected, server_etag = ?now, "caldav settings put: stale etag");
                    return precondition_failed("etag mismatch");
                }
            }
            if let Err(err) = store.put_document(storage, &name, body) {
                error!(resource = %name, error = %err, "caldav settings put failed");
                return status(StatusCode::INTERNAL_SERVER_ERROR);
            }
            info!(resource = %name, bytes = body.len(), created = current.is_none(), "caldav settings document written");
            let code = if current.is_some() {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            };
            Response::builder()
                .status(code)
                .header(header::ETAG, etag_for(body))
                .body(Body::empty())
                .expect("valid response")
        }
        ("DELETE", _, Some(name)) => match store.delete_document(storage, &name) {
            Ok(true) => {
                info!(resource = %name, "caldav settings document deleted");
                status(StatusCode::NO_CONTENT)
            }
            Ok(false) => status(StatusCode::NOT_FOUND),
            Err(err) => {
                error!(resource = %name, error = %err, "caldav settings delete failed");
                status(StatusCode::INTERNAL_SERVER_ERROR)
            }
        },
        _ => {
            debug!(%method, path, "caldav settings path not found");
            status(StatusCode::NOT_FOUND)
        }
    }
}
