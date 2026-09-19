# anytype-caldav

A small server that puts one [Anytype](https://anytype.io) space on a calendar.
Tasks and events live in Anytype; any CalDAV client shows and edits them, and
reminders arrive as Web Push on the phone, including iOS.

```
 Anytype (headless or desktop)
        │  REST API, one space
        ▼
 anytype-caldav ──── /dav/        CalDAV: tasks (VTODO), events (VEVENT), read and write
                 ├── /push/       Web Push subscriptions and reminders
                 ├── /f/…/todos.ics  read-only iCalendar feed
                 └── /capture     one line of text becomes a task
```

Anytype stays the source of truth. The server keeps no copy of the space; its
only state is a SQLite file with push subscriptions, sent reminders and the
calendar client's own settings.

## What it does

- **Tasks** of one type become `VTODO`s: scheduled date, deadline, done, tags as
  `CATEGORIES`, reminders as `VALARM`. Edits made in the client (done, dates,
  name, tags, new tasks, deletion as archive) are written back to Anytype.
- **Events** of type `event` become a second calendar, with recurring series,
  edited and deleted occurrences, and deadlines as entries of their own.
- **A calendar per person** when tasks have an assignee property. Every person
  of the space can get an account of their own; the password is derived from
  one server secret, so nothing is stored.
- **Server-side reminders** through Web Push, sent at most once, surviving
  restarts. A person's subscription receives only the reminders of their tasks.
- **Recurring tasks and events**: a `recurring_task` or `recurring_event`
  object holds the rule; the server creates the next occurrence in Anytype.
- **`init`** checks a space against the schema the server needs and creates
  what is missing.

What it does not do: Apple Calendar and Reminders cannot show `VTODO` by any
route. Calendar ignores it in a subscribed feed, and Reminders no longer
speaks CalDAV. One process serves one space; run a second process for a second
space.

## Deploy with Docker Compose

[`deploy/compose/`](deploy/compose/) runs everything: a headless Anytype of its
own, the server and Caddy for TLS. The host needs Docker and a domain name.

```sh
cd deploy/compose
cp env.example .env && chmod 600 .env   # DOMAIN, ANYTYPE_INVITE_LINK, CALDAV_PASSWORD
docker compose up -d
```

On the first start the kit creates a bot account, asks to join the space by
the invite link, issues an API key, prepares the space's schema and starts the
server. Every step can run again without harm, so after changing `.env`, run
`docker compose up -d` again. The kit's [README](deploy/compose/README.md) has
the details.

## Run from source

You need a running Anytype and an API key for it.

```sh
export ANYTYPE_API_KEY=...
export ANYTYPE_CALDAV__ANYTYPE__URL=http://127.0.0.1:31009
cargo run -- init --apply   # prepare the space's schema
cargo run                   # serve on 127.0.0.1:8080
```

Set `anytype.space_id` to the space to serve; without it the server lists the
account's spaces with their ids and stops. The compose kit takes the id from
the invite link instead. [`deploy/systemd/`](deploy/systemd/) has a unit for a host without
Docker.

## Configuration

Every setting has a default or can be left out, except `anytype.url` and
`anytype.space_id`, which the compose kit sets itself.
[`config.example.toml`](config.example.toml) describes them all. They can be
given in a TOML file (`--config`), as environment variables, or both; a
variable wins over the file:

```
ANYTYPE_CALDAV__<SECTION>__<KEY>=<value>

ANYTYPE_CALDAV__CALENDAR__TIMEZONE=Europe/Berlin
ANYTYPE_CALDAV__CALDAV__ENABLED=true
ANYTYPE_CALDAV__SERVER__ALLOWED_ORIGINS=["https://calendar.example"]
```

Booleans, numbers and arrays are written as in TOML; anything else is taken as
a string, without quotes. The names of the variables in use are logged at
startup.

Secrets come only from the environment:

| Variable | Needed for |
|---|---|
| `ANYTYPE_API_KEY` | always |
| `CALDAV_PASSWORD` | `caldav.enabled`, the shared login |
| `ACCOUNTS_SECRET` | accounts per person (`properties.assignee` must be set) |

Each can also be read from a file named by the same variable with `_FILE`
appended, such as `ANYTYPE_API_KEY_FILE`, as Docker secrets are passed.

Notifications, the default calendar names and the deadline line in a task's
description are in English or Russian, set by `calendar.language` (`en` by
default, or `ru`).

Properties are selected as `key:<stable-key>` or `id:<opaque-id>`, never by
display name, so renaming a property in Anytype breaks nothing. The three
required ones default to the keys `init` creates. If a selector matches
nothing, the refresh fails and every property the type offers is logged:

```
WARN property available on type task property=key:due_date id:bafy... name:"Deadline" format:Date
```

## Commands

| Command | What it does |
|---|---|
| `anytype-caldav [--config c.toml]` | runs the server |
| `… init [--apply]` | checks the space's schema; `--apply` creates missing properties |
| `… generate [--apply]` | shows the occurrences of series due today; `--apply` creates them |
| `… users` | prints every person's name, login and password (needs `ACCOUNTS_SECRET`) |

`GET /healthz` reports on the process only and never contacts Anytype.

## Reminders

Web Push is optional (`push.enabled`). Subscriptions and handled reminders are
stored in SQLite, so they survive restarts. When `push.private_key_file` does
not exist, the VAPID key is made there on the first start, readable by its
owner only; an existing file is never replaced. Keep it, since a new key
invalidates every subscription, and back it up together with the state file.

- Delivery is **at most once**: a reminder is claimed before it is sent, so a
  restart never repeats one, and a crash right after the claim loses it.
- Reminders missed by less than `push.late_window` are sent late; older ones
  are skipped. A reminder due while nothing is subscribed is not claimed.
- `properties.reminder` is a select whose option names are durations (`15m`,
  `1h`, `1d`, `1w`); each chosen option is one reminder. Without it,
  `reminders.lead_time` applies.
- An all-day task reminds at `reminders.all_day_time`.
- On iOS, Web Push works only for a site added to the Home Screen and a
  subscription made from that installed app.

A client subscribes through three routes beside `/dav/`, the last two behind
the same Basic credentials as CalDAV:

| Route | What |
|---|---|
| `GET /push/key` | the VAPID public key, as `{"publicKey": "…"}` |
| `POST /push/subscribe` | a browser `PushSubscription` as JSON; a person's own login receives only their reminders |
| `POST /push/test` | sends a test notification: under a person's login to theirs, under the shared login to all |

## Notes on behaviour

- **Timed values are emitted as UTC instants.** A `TZID` would oblige the
  document to carry a `VTIMEZONE` (RFC 5545 §3.6.5).
- **A value at local midnight is an all-day date.** Which midnight is set by
  `calendar.date_only_timezone`; if all-day tasks appear at 04:00, set it to
  `"UTC"`.
- **A failed refresh serves the last good data** with `X-Exporter-Stale: true`.
  Before the first success it answers `503`, since a client reads an empty
  calendar as "delete everything".
- **The feed is unauthenticated**, protected by an unguessable path. CORS is off
  unless origins are listed, and `*` is refused for it. The compose kit does
  not route it at all.

## Repository layout

| Path | What |
|---|---|
| `src/`, `tests/` | the server; `cargo test` runs everything without a live Anytype |
| `deploy/compose/` | Docker Compose kit: headless Anytype, the server, Caddy |
| `deploy/systemd/` | unit file for a host without Docker |
| `scripts/` | `type_header.py`, which sets the properties in a type's header; only gRPC can |
| `cliff.toml` | [git-cliff](https://git-cliff.org) config; commits follow Conventional Commits |

## Development

```sh
cargo test
git cliff --unreleased   # the changelog since the last tag
```

Generated calendars are parsed back by a different crate than the one that
wrote them. The Anytype boundary is covered by fixtures; a live test against a
real installation is gated by the environment (`tests/live_feed.rs`).

## License

[GLWT (Good Luck With That) Public License](LICENSE).
