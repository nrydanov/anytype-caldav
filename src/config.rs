//! Configuration loading and validation.
//!
//! Every error here terminates startup: a service that runs with a
//! half-understood configuration publishes a silently wrong feed, which is
//! worse than not starting at all.

use std::{fmt, net::SocketAddr, path::Path, time::Duration};

use chrono_tz::Tz;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// How a mapped property is located on an Anytype object.
///
/// Selection is exact and unambiguous: never by display name, which users
/// rename freely. `key` is the stable snake_case identifier an operator can
/// actually discover; `id` is the opaque one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertySelector {
    Id(String),
    Key(String),
}

impl PropertySelector {
    fn parse(field: &str, raw: &str) -> Result<Self, ConfigError> {
        let raw = raw.trim();
        let (kind, value) = raw.split_once(':').ok_or_else(|| {
            ConfigError::Invalid(format!(
                "properties.{field} = \"{raw}\" must be prefixed with \"id:\" or \"key:\""
            ))
        })?;
        let value = value.trim();
        if value.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "properties.{field} has an empty selector value"
            )));
        }
        match kind.trim() {
            "id" => Ok(Self::Id(value.to_string())),
            "key" => Ok(Self::Key(value.to_string())),
            other => Err(ConfigError::Invalid(format!(
                "properties.{field} has unknown selector kind \"{other}\", expected \"id\" or \"key\""
            ))),
        }
    }
}

impl fmt::Display for PropertySelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(v) => write!(f, "id:{v}"),
            Self::Key(v) => write!(f, "key:{v}"),
        }
    }
}

// ---------------------------------------------------------------- raw shapes

#[derive(Debug, Deserialize)]
struct RawConfig {
    anytype: RawAnytype,
    properties: RawProperties,
    calendar: RawCalendar,
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    reminders: RawReminders,
    #[serde(default)]
    push: RawPush,
    #[serde(default)]
    series: RawSeries,
    #[serde(default)]
    caldav: RawCaldav,
}

#[derive(Debug, Default, Deserialize)]
struct RawCaldav {
    #[serde(default)]
    enabled: bool,
    username: Option<String>,
    #[serde(default)]
    writable: bool,
    #[serde(default)]
    events: bool,
    #[serde(default)]
    settings: bool,
}

#[derive(Debug, Deserialize)]
struct RawSeries {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_series_poll_interval", with = "humantime_serde")]
    poll_interval: Duration,
}

impl Default for RawSeries {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval: default_series_poll_interval(),
        }
    }
}

/// The generator only has to act once a day per series; five minutes keeps the
/// day boundary tight without re-reading every task every 30 seconds.
fn default_series_poll_interval() -> Duration {
    Duration::from_secs(300)
}

#[derive(Debug, Deserialize)]
struct RawPush {
    #[serde(default)]
    enabled: bool,
    private_key_file: Option<String>,
    state_file: Option<String>,
    #[serde(default = "default_push_poll_interval", with = "humantime_serde")]
    poll_interval: Duration,
    #[serde(default = "default_push_late_window", with = "humantime_serde")]
    late_window: Duration,
    app_url: Option<String>,
}

impl Default for RawPush {
    fn default() -> Self {
        Self {
            enabled: false,
            private_key_file: None,
            state_file: None,
            poll_interval: default_push_poll_interval(),
            late_window: default_push_late_window(),
            app_url: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawAnytype {
    url: String,
    space_id: String,
    #[serde(default = "default_type_key")]
    type_key: String,
    #[serde(default = "default_max_objects")]
    max_objects: usize,
}

#[derive(Debug, Deserialize)]
struct RawProperties {
    scheduled: String,
    deadline: String,
    done: String,
    reminder: Option<String>,
    tags: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCalendar {
    #[serde(default = "default_timezone")]
    timezone: String,
    #[serde(default = "default_calendar_name")]
    name: String,
    /// Defaults to `timezone` when absent.
    date_only_timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawReminders {
    #[serde(default = "default_reminders_enabled")]
    enabled: bool,
    #[serde(default = "default_lead_time", with = "humantime_serde")]
    lead_time: Duration,
    #[serde(default = "default_all_day_time")]
    all_day_time: String,
}

impl Default for RawReminders {
    fn default() -> Self {
        Self {
            enabled: default_reminders_enabled(),
            lead_time: default_lead_time(),
            all_day_time: default_all_day_time(),
        }
    }
}

fn default_reminders_enabled() -> bool {
    true
}
fn default_lead_time() -> Duration {
    Duration::from_secs(30 * 60)
}
fn default_all_day_time() -> String {
    "09:00".to_string()
}
fn default_push_poll_interval() -> Duration {
    Duration::from_secs(30)
}
fn default_push_late_window() -> Duration {
    Duration::from_secs(60 * 60)
}

#[derive(Debug, Deserialize)]
struct RawServer {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_request_timeout", with = "humantime_serde")]
    request_timeout: Duration,
    #[serde(default = "default_min_refresh", with = "humantime_serde")]
    min_refresh_interval: Duration,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default = "default_feed_path")]
    feed_path: String,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            request_timeout: default_request_timeout(),
            min_refresh_interval: default_min_refresh(),
            allowed_origins: Vec::new(),
            feed_path: default_feed_path(),
        }
    }
}

fn default_type_key() -> String {
    "task".to_string()
}
fn default_max_objects() -> usize {
    5000
}
fn default_timezone() -> String {
    "Europe/Saratov".to_string()
}
fn default_calendar_name() -> String {
    "Anytype Tasks".to_string()
}
fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}
fn default_request_timeout() -> Duration {
    Duration::from_secs(10)
}
fn default_min_refresh() -> Duration {
    Duration::from_secs(30)
}
fn default_feed_path() -> String {
    "/todos.ics".to_string()
}

// ------------------------------------------------------------ validated form

#[derive(Debug, Clone)]
pub struct Config {
    pub anytype: AnytypeConfig,
    pub properties: PropertiesConfig,
    pub calendar: CalendarConfig,
    pub server: ServerConfig,
    pub reminders: RemindersConfig,
    pub push: PushConfig,
    pub series: SeriesConfig,
    pub caldav: CaldavConfig,
}

/// The CalDAV facade under `/dav/`. The password is not here: it is read from
/// `CALDAV_PASSWORD`, next to the Anytype API key, so this file holds no secret.
#[derive(Debug, Clone)]
pub struct CaldavConfig {
    pub enabled: bool,
    pub username: String,
    /// Accept PUT and DELETE from clients. Off by default, separately from
    /// `enabled`, so a deploy never makes the calendar editable by itself.
    pub writable: bool,
    /// Serve objects of type `event` as a second collection, `events/`.
    pub events: bool,
    /// Store the calendar app's own settings documents, outside Anytype.
    pub settings: bool,
}

/// The recurring-task generator. Off by default: turning it on is the
/// switch-over from whatever made these tasks before, and it must not happen on a deploy.
#[derive(Debug, Clone)]
pub struct SeriesConfig {
    pub enabled: bool,
    pub poll_interval: Duration,
}

#[derive(Debug, Clone)]
pub struct PushConfig {
    pub enabled: bool,
    /// PEM file holding the VAPID private key. Stable for the life of every
    /// subscription: replacing it invalidates all of them, because each was
    /// created against the matching public key.
    pub private_key_file: Option<std::path::PathBuf>,
    /// SQLite file containing subscriptions and handled reminder identities.
    pub state_file: Option<std::path::PathBuf>,
    pub poll_interval: Duration,
    pub late_window: chrono::Duration,
    /// The calendar app a tapped notification opens, e.g. Calino's root.
    /// When set, pushes also carry a Declarative Web Push envelope, which
    /// Safari shows without a service worker and which requires this URL.
    pub app_url: Option<String>,
}

/// Controls the `VALARM` emitted with each task.
///
/// Triggers are absolute instants rather than durations. RFC 5545 resolves a
/// relative trigger on a `VTODO` against `DTSTART` unless `RELATED=END` is
/// given, and an all-day task starts at midnight — so "30 minutes before"
/// would fire at 23:30 the previous night. Computing the instant here removes
/// that ambiguity and lets an all-day task alarm at a civil hour instead.
#[derive(Debug, Clone)]
pub struct RemindersConfig {
    pub enabled: bool,
    /// How long before a timed deadline the alarm fires.
    pub lead_time: chrono::Duration,
    /// Local time of day an all-day task alarms on its due date.
    pub all_day_time: chrono::NaiveTime,
}

#[derive(Debug, Clone)]
pub struct AnytypeConfig {
    pub url: String,
    pub space_id: String,
    pub type_key: String,
    pub max_objects: usize,
}

#[derive(Debug, Clone)]
pub struct PropertiesConfig {
    pub scheduled: PropertySelector,
    pub deadline: PropertySelector,
    pub done: PropertySelector,
    /// Optional per-task lead times. Without it every task uses
    /// `reminders.lead_time`.
    pub reminder: Option<PropertySelector>,
    /// Optional tags, written as one `CATEGORIES` line per option name.
    pub tags: Option<PropertySelector>,
}

#[derive(Debug, Clone)]
pub struct CalendarConfig {
    pub timezone: Tz,
    pub name: String,
    /// Timezone in which a value is tested for midnight to classify it as
    /// date-only. Separate from `timezone` because it is a property of how
    /// Anytype persists dates, not of how the calendar displays them.
    pub date_only_timezone: Tz,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub request_timeout: Duration,
    pub min_refresh_interval: Duration,
    pub allowed_origins: Vec<String>,
    /// Path the feed is served at. On a publicly reachable bind this is the
    /// only thing standing between the task list and anyone who visits, so it
    /// is expected to carry an unguessable token.
    pub feed_path: String,
}

impl ServerConfig {
    /// True when the bind address is not loopback, i.e. the feed is reachable
    /// from outside this machine.
    pub fn is_publicly_bound(&self) -> bool {
        !self.listen.ip().is_loopback()
    }

    /// True when the path carries no unguessable component.
    pub fn feed_path_is_guessable(&self) -> bool {
        self.feed_path == default_feed_path()
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml(&text, &path.display().to_string())
    }

    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_string(),
            source,
        })?;
        Self::validate(raw)
    }

    fn validate(raw: RawConfig) -> Result<Self, ConfigError> {
        let url = raw.anytype.url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return Err(ConfigError::Invalid("anytype.url must not be empty".into()));
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ConfigError::Invalid(format!(
                "anytype.url must start with http:// or https://, got \"{url}\""
            )));
        }
        let space_id = raw.anytype.space_id.trim().to_string();
        if space_id.is_empty() {
            return Err(ConfigError::Invalid(
                "anytype.space_id must not be empty".into(),
            ));
        }
        // The SDK rejects a malformed id with a redacted validation error on the
        // first refresh, which reads as "anytype is broken". Catching the shape
        // here turns that into an actionable startup message.
        if !anytype::validation::looks_like_object_id(&space_id) {
            return Err(ConfigError::Invalid(format!(
                "anytype.space_id = \"{space_id}\" is not an Anytype id; \
                 expected a 59-character id beginning with \"bafyrei\", \
                 optionally followed by \".<hash>\""
            )));
        }
        if raw.anytype.type_key.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "anytype.type_key must not be empty".into(),
            ));
        }
        if raw.anytype.max_objects == 0 {
            return Err(ConfigError::Invalid(
                "anytype.max_objects must be greater than zero".into(),
            ));
        }

        let scheduled = PropertySelector::parse("scheduled", &raw.properties.scheduled)?;
        let deadline = PropertySelector::parse("deadline", &raw.properties.deadline)?;
        let done = PropertySelector::parse("done", &raw.properties.done)?;
        let reminder = raw
            .properties
            .reminder
            .as_deref()
            .map(|value| PropertySelector::parse("reminder", value))
            .transpose()?;
        let tags = raw
            .properties
            .tags
            .as_deref()
            .map(|value| PropertySelector::parse("tags", value))
            .transpose()?;
        // Two selectors pointing at one property would silently map one source
        // value onto two calendar fields.
        let mut selectors = vec![
            ("scheduled", &scheduled),
            ("deadline", &deadline),
            ("done", &done),
        ];
        if let Some(reminder) = &reminder {
            selectors.push(("reminder", reminder));
        }
        if let Some(tags) = &tags {
            selectors.push(("tags", tags));
        }
        for (index, (an, a)) in selectors.iter().enumerate() {
            for (bn, b) in &selectors[index + 1..] {
                if a == b {
                    return Err(ConfigError::Invalid(format!(
                        "properties.{an} and properties.{bn} both select {a}; they must be distinct"
                    )));
                }
            }
        }

        let timezone = parse_tz("calendar.timezone", &raw.calendar.timezone)?;
        let date_only_timezone = match raw.calendar.date_only_timezone.as_deref() {
            Some(value) => parse_tz("calendar.date_only_timezone", value)?,
            None => timezone,
        };
        if raw.calendar.name.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "calendar.name must not be empty".into(),
            ));
        }

        let listen: SocketAddr = raw.server.listen.parse().map_err(|_| {
            ConfigError::Invalid(format!(
                "server.listen = \"{}\" is not a valid socket address",
                raw.server.listen
            ))
        })?;
        if raw.server.request_timeout.is_zero() {
            return Err(ConfigError::Invalid(
                "server.request_timeout must be greater than zero".into(),
            ));
        }

        let mut allowed_origins = Vec::with_capacity(raw.server.allowed_origins.len());
        for origin in &raw.server.allowed_origins {
            let origin = origin.trim().trim_end_matches('/');
            if origin.is_empty() {
                return Err(ConfigError::Invalid(
                    "server.allowed_origins contains an empty entry".into(),
                ));
            }
            if origin == "*" {
                return Err(ConfigError::Invalid(
                    "server.allowed_origins does not accept \"*\": the feed is unauthenticated, \
                     so a wildcard would let any page the user visits read the whole task list"
                        .into(),
                ));
            }
            if !(origin.starts_with("http://") || origin.starts_with("https://")) {
                return Err(ConfigError::Invalid(format!(
                    "server.allowed_origins entry \"{origin}\" must be a full origin, \
                     e.g. https://calino.io"
                )));
            }
            allowed_origins.push(origin.to_string());
        }

        let feed_path = raw.server.feed_path.trim().to_string();
        if !feed_path.starts_with('/') {
            return Err(ConfigError::Invalid(format!(
                "server.feed_path = \"{feed_path}\" must start with \"/\""
            )));
        }
        if feed_path == "/healthz" {
            return Err(ConfigError::Invalid(
                "server.feed_path must not collide with /healthz".into(),
            ));
        }
        if feed_path.contains("//") || feed_path.ends_with('/') {
            return Err(ConfigError::Invalid(format!(
                "server.feed_path = \"{feed_path}\" must be a normalized path"
            )));
        }

        let all_day_time =
            chrono::NaiveTime::parse_from_str(raw.reminders.all_day_time.trim(), "%H:%M").map_err(
                |_| {
                    ConfigError::Invalid(format!(
                        "reminders.all_day_time = \"{}\" must be a time of day like \"09:00\"",
                        raw.reminders.all_day_time
                    ))
                },
            )?;
        let lead_time = chrono::Duration::from_std(raw.reminders.lead_time)
            .map_err(|_| ConfigError::Invalid("reminders.lead_time is out of range".to_string()))?;

        let push_key_file = match (raw.push.enabled, raw.push.private_key_file.as_deref()) {
            (true, None) => {
                return Err(ConfigError::Invalid(
                    "push.enabled is true but push.private_key_file is not set".into(),
                ));
            }
            (_, Some(path)) => Some(std::path::PathBuf::from(shellexpand(path))),
            (false, None) => None,
        };
        let push_state_file = match (raw.push.enabled, raw.push.state_file.as_deref()) {
            (true, None) => {
                return Err(ConfigError::Invalid(
                    "push.enabled is true but push.state_file is not set".into(),
                ));
            }
            (_, Some(path)) => Some(std::path::PathBuf::from(shellexpand(path))),
            (false, None) => None,
        };
        if raw.push.poll_interval.is_zero() {
            return Err(ConfigError::Invalid(
                "push.poll_interval must be greater than zero".into(),
            ));
        }
        if raw.push.late_window.is_zero() {
            return Err(ConfigError::Invalid(
                "push.late_window must be greater than zero".into(),
            ));
        }
        let push_late_window = chrono::Duration::from_std(raw.push.late_window)
            .map_err(|_| ConfigError::Invalid("push.late_window is out of range".into()))?;
        // The generator records what it created in the same database, so a
        // task deleted by hand is not created again.
        if raw.series.enabled && push_state_file.is_none() {
            return Err(ConfigError::Invalid(
                "series.enabled is true but push.state_file is not set".into(),
            ));
        }
        let caldav_username = raw
            .caldav
            .username
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_string();
        if raw.caldav.enabled && caldav_username.is_empty() {
            return Err(ConfigError::Invalid(
                "caldav.enabled is true but caldav.username is not set".into(),
            ));
        }
        if raw.series.poll_interval.is_zero() {
            return Err(ConfigError::Invalid(
                "series.poll_interval must be greater than zero".into(),
            ));
        }

        Ok(Self {
            anytype: AnytypeConfig {
                url,
                space_id,
                type_key: raw.anytype.type_key.trim().to_string(),
                max_objects: raw.anytype.max_objects,
            },
            properties: PropertiesConfig {
                scheduled,
                deadline,
                done,
                reminder,
                tags,
            },
            calendar: CalendarConfig {
                timezone,
                name: raw.calendar.name.trim().to_string(),
                date_only_timezone,
            },
            server: ServerConfig {
                listen,
                request_timeout: raw.server.request_timeout,
                min_refresh_interval: raw.server.min_refresh_interval,
                allowed_origins,
                feed_path,
            },
            reminders: RemindersConfig {
                enabled: raw.reminders.enabled,
                lead_time,
                all_day_time,
            },
            push: PushConfig {
                enabled: raw.push.enabled,
                private_key_file: push_key_file,
                state_file: push_state_file,
                poll_interval: raw.push.poll_interval,
                late_window: push_late_window,
                app_url: raw
                    .push
                    .app_url
                    .map(|url| url.trim().to_string())
                    .filter(|url| !url.is_empty()),
            },
            series: SeriesConfig {
                enabled: raw.series.enabled,
                poll_interval: raw.series.poll_interval,
            },
            caldav: CaldavConfig {
                enabled: raw.caldav.enabled,
                username: caldav_username,
                writable: raw.caldav.enabled && raw.caldav.writable,
                events: raw.caldav.enabled && raw.caldav.events,
                settings: raw.caldav.enabled && raw.caldav.settings,
            },
        })
    }
}

fn parse_tz(field: &str, value: &str) -> Result<Tz, ConfigError> {
    value.trim().parse::<Tz>().map_err(|_| {
        ConfigError::Invalid(format!(
            "{field} = \"{value}\" is not a valid IANA timezone name"
        ))
    })
}

/// Expands a leading `~/` so a key path can be written the way an operator
/// would type it.
fn shellexpand(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntactically valid Anytype space id.
    const SPACE_ID: &str = "bafyreiaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn base() -> String {
        r#"
[anytype]
url = "http://127.0.0.1:31009"
space_id = "{SPACE_ID}"

[properties]
scheduled = "key:scheduled"
deadline = "key:due_date"
done = "key:done"

[calendar]
timezone = "Europe/Saratov"
name = "Anytype Tasks"

[server]
listen = "127.0.0.1:8080"
request_timeout = "10s"
min_refresh_interval = "30s"
allowed_origins = ["https://calino.io"]
"#
        .to_string()
        .replace("{SPACE_ID}", SPACE_ID)
    }

    fn load(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml(text, "test.toml")
    }

    #[test]
    fn parses_a_complete_configuration() {
        let config = load(&base()).expect("valid config");
        assert_eq!(config.anytype.space_id, SPACE_ID);
        assert_eq!(config.anytype.type_key, "task", "type_key defaults to task");
        assert_eq!(config.anytype.max_objects, 5000);
        assert_eq!(
            config.properties.scheduled,
            PropertySelector::Key("scheduled".into())
        );
        assert_eq!(config.server.request_timeout, Duration::from_secs(10));
        assert_eq!(config.server.min_refresh_interval, Duration::from_secs(30));
        assert_eq!(config.server.allowed_origins, vec!["https://calino.io"]);
    }

    #[test]
    fn date_only_timezone_defaults_to_the_display_timezone() {
        let config = load(&base()).expect("valid config");
        assert_eq!(config.calendar.date_only_timezone, config.calendar.timezone);
    }

    #[test]
    fn date_only_timezone_can_differ_from_the_display_timezone() {
        let text = base().replace(
            "name = \"Anytype Tasks\"",
            "name = \"Anytype Tasks\"\ndate_only_timezone = \"UTC\"",
        );
        let config = load(&text).expect("valid config");
        assert_eq!(config.calendar.timezone, chrono_tz::Europe::Saratov);
        assert_eq!(config.calendar.date_only_timezone, chrono_tz::UTC);
    }

    #[test]
    fn accepts_id_selectors() {
        let text = base().replace("scheduled = \"key:scheduled\"", "scheduled = \"id:abc123\"");
        let config = load(&text).expect("valid config");
        assert_eq!(
            config.properties.scheduled,
            PropertySelector::Id("abc123".into())
        );
    }

    #[test]
    fn rejects_a_selector_without_a_kind_prefix() {
        let text = base().replace("scheduled = \"key:scheduled\"", "scheduled = \"scheduled\"");
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("id:"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_selector_kind() {
        let text = base().replace(
            "scheduled = \"key:scheduled\"",
            "scheduled = \"name:Scheduled\"",
        );
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("unknown selector kind"), "{err}");
    }

    #[test]
    fn rejects_two_selectors_pointing_at_one_property() {
        let text = base().replace(
            "deadline = \"key:due_date\"",
            "deadline = \"key:scheduled\"",
        );
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("must be distinct"), "{err}");
    }

    #[test]
    fn rejects_a_wildcard_origin() {
        let text = base().replace("[\"https://calino.io\"]", "[\"*\"]");
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("does not accept"), "{err}");
    }

    #[test]
    fn rejects_an_origin_without_a_scheme() {
        let text = base().replace("[\"https://calino.io\"]", "[\"calino.io\"]");
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("full origin"), "{err}");
    }

    /// The SDK would otherwise reject this on the first refresh with a
    /// redacted validation error, which reads as an Anytype outage.
    #[test]
    fn rejects_a_space_id_that_is_not_an_anytype_id() {
        let text = base().replace(SPACE_ID, "my-space");
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("is not an Anytype id"), "{err}");
    }

    #[test]
    fn accepts_a_space_id_with_the_account_hash_suffix() {
        let text = base().replace(SPACE_ID, &format!("{SPACE_ID}.2lcxyz"));
        let config = load(&text).expect("valid config");
        assert!(config.anytype.space_id.ends_with(".2lcxyz"));
    }

    #[test]
    fn rejects_an_invalid_timezone() {
        let text = base().replace("Europe/Saratov", "Europe/Nowhere");
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("IANA timezone"), "{err}");
    }

    #[test]
    fn feed_path_defaults_and_is_flagged_as_guessable() {
        let config = load(&base()).expect("valid config");
        assert_eq!(config.server.feed_path, "/todos.ics");
        assert!(config.server.feed_path_is_guessable());
        assert!(!config.server.is_publicly_bound(), "127.0.0.1 is loopback");
    }

    #[test]
    fn an_unguessable_feed_path_is_accepted() {
        let text = format!(
            "{}\nfeed_path = \"/f/0f8c1d2e3a4b5c6d7e8f9a0b1c2d3e4f/todos.ics\"",
            base()
        );
        let config = load(&text).expect("valid config");
        assert!(!config.server.feed_path_is_guessable());
    }

    #[test]
    fn a_public_bind_is_detected() {
        let text = base().replace("127.0.0.1:8080", "0.0.0.0:8080");
        let config = load(&text).expect("valid config");
        assert!(config.server.is_publicly_bound());
    }

    #[test]
    fn rejects_a_feed_path_without_a_leading_slash() {
        let text = format!("{}\nfeed_path = \"todos.ics\"", base());
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("must start with"), "{err}");
    }

    #[test]
    fn rejects_a_feed_path_colliding_with_healthz() {
        let text = format!("{}\nfeed_path = \"/healthz\"", base());
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("healthz"), "{err}");
    }

    #[test]
    fn rejects_an_invalid_listen_address() {
        let text = base().replace("127.0.0.1:8080", "not-an-address");
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("socket address"), "{err}");
    }

    #[test]
    fn rejects_a_zero_request_timeout() {
        let text = base().replace("request_timeout = \"10s\"", "request_timeout = \"0s\"");
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("greater than zero"), "{err}");
    }

    #[test]
    fn omits_cors_headers_when_no_origins_are_configured() {
        let text = base().replace("allowed_origins = [\"https://calino.io\"]", "");
        let config = load(&text).expect("valid config");
        assert!(config.server.allowed_origins.is_empty());
    }

    #[test]
    fn push_requires_a_state_file_when_enabled() {
        let text = format!(
            "{}\n[push]\nenabled = true\nprivate_key_file = \"/tmp/key.pem\"",
            base()
        );
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("push.state_file"), "{err}");
    }

    #[test]
    fn the_generator_is_off_unless_asked_for() {
        let config = load(&base()).expect("valid config");
        assert!(!config.series.enabled);
        assert_eq!(config.series.poll_interval, Duration::from_secs(300));
    }

    #[test]
    fn caldav_is_off_unless_asked_for_and_needs_a_username() {
        assert!(!load(&base()).expect("valid config").caldav.enabled);
        let err = load(&format!("{}\n[caldav]\nenabled = true", base()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("caldav.username"), "{err}");
    }

    #[test]
    fn the_generator_requires_a_state_file() {
        let text = format!(
            "{}
[series]
enabled = true",
            base()
        );
        let err = load(&text).unwrap_err().to_string();
        assert!(err.contains("push.state_file"), "{err}");
    }

    #[test]
    fn push_parses_scheduler_durations() {
        let text = format!(
            "{}\n[push]\nenabled = true\nprivate_key_file = \"/tmp/key.pem\"\nstate_file = \"/tmp/state.sqlite3\"\npoll_interval = \"15s\"\nlate_window = \"1h\"",
            base()
        );
        let config = load(&text).expect("valid push configuration");
        assert_eq!(config.push.poll_interval, Duration::from_secs(15));
        assert_eq!(config.push.late_window, chrono::Duration::hours(1));
        assert_eq!(
            config.push.state_file.as_deref(),
            Some(Path::new("/tmp/state.sqlite3"))
        );
    }

    #[test]
    fn push_rejects_zero_scheduler_durations() {
        for field in ["poll_interval = \"0s\"", "late_window = \"0s\""] {
            let text = format!(
                "{}\n[push]\nenabled = true\nprivate_key_file = \"/tmp/key.pem\"\nstate_file = \"/tmp/state.sqlite3\"\n{field}",
                base()
            );
            let err = load(&text).unwrap_err().to_string();
            assert!(err.contains("greater than zero"), "{field}: {err}");
        }
    }
}
