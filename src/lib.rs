//! CalDAV server, iCalendar feed and Web Push reminders over one Anytype space.

pub mod accounts;
pub mod anytype_source;
pub mod caldav;
pub mod capture;
pub mod config;
pub mod events;
pub mod feed;
pub mod http;
pub mod install;
pub mod model;
pub mod push;
pub mod reminder;
pub mod render;
pub mod scheduler;
pub mod series;
pub mod source;
pub mod state;
pub mod writeback;
