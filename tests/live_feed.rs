//! Validates a feed captured from a real Anytype installation.
//!
//! Skipped unless `LIVE_FEED_ICS` points at a captured `.ics`, so the normal
//! suite stays hermetic. Unit tests cover the renderer with synthetic tasks;
//! this covers what real data actually produces.

use std::{collections::HashSet, io::BufReader};

#[test]
fn a_captured_live_feed_is_valid() {
    let Ok(path) = std::env::var("LIVE_FEED_ICS") else {
        eprintln!("LIVE_FEED_ICS not set; skipping");
        return;
    };
    let text = std::fs::read_to_string(&path).expect("readable capture");

    // Parsed by a different crate than the one that produced it.
    let calendars: Vec<_> = ical::IcalParser::new(BufReader::new(text.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .expect("an independent parser accepts the feed");
    assert_eq!(calendars.len(), 1);
    let todos = &calendars[0].todos;
    assert!(!todos.is_empty(), "the capture has components");

    let mut uids = HashSet::new();
    for todo in todos {
        let prop = |name: &str| {
            todo.properties
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| p.value.clone())
        };

        let uid = prop("UID").expect("every component has a UID");
        assert!(uids.insert(uid.clone()), "duplicate UID published: {uid}");
        assert!(uid.ends_with("@anytype-task-exporter"), "{uid}");
        assert!(prop("SUMMARY").is_some(), "{uid} has no SUMMARY");
        assert!(prop("DTSTAMP").is_some(), "{uid} has no DTSTAMP");

        // Completion state is always explicit and never invented.
        let status = prop("STATUS").expect("STATUS");
        assert!(
            status == "NEEDS-ACTION" || status == "COMPLETED",
            "{uid} has unexpected STATUS {status}"
        );
        assert_eq!(
            prop("PERCENT-COMPLETE").as_deref(),
            Some(if status == "COMPLETED" { "100" } else { "0" }),
            "{uid} percent disagrees with status"
        );
        assert!(prop("COMPLETED").is_none(), "{uid} invented a COMPLETED");
    }

    // Properties that would oblige a VTIMEZONE, or that the PoC excludes.
    for forbidden in ["BEGIN:VTIMEZONE", "TZID=", "RRULE", "RDATE"] {
        assert!(!text.contains(forbidden), "the feed contains {forbidden}");
    }

    for line in text.split("\r\n") {
        assert!(
            line.len() <= 75,
            "line of {} octets exceeds the RFC 5545 limit",
            line.len()
        );
    }
}
