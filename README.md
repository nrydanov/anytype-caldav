# anytype-caldav

A small server that puts one [Anytype](https://anytype.io) space on a calendar.
Tasks and events live in Anytype; a CalDAV client such as
[Calino](https://calino.io) shows and edits them, and reminders arrive as Web
Push on the phone, including iOS.

```
 Anytype (headless or desktop)
        │  REST API, one space
        ▼
 anytype-caldav ──── /dav/        CalDAV: tasks (VTODO), events (VEVENT), read and write
        │        ├── /push/       Web Push subscriptions and reminders
        │        ├── /f/…/todos.ics  read-only iCalendar feed
        │        └── /capture     one line of text becomes a task
        ▼
 Calino in the browser / on the Home Screen
```

Anytype stays the source of truth. The server keeps no copy of the space; its
only state is a SQLite file with push subscriptions, sent reminders and the
calendar app's own settings.

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
route (see [`docs/dev/2026-08-31-tasks-and-events-decision-log.md`](docs/dev/2026-08-31-tasks-and-events-decision-log.md)).
One process serves one space; run a second process for a second space.

## Quick start

You need a running Anytype with an API key and the id of the space.

```sh
cp config.example.toml config.toml        # set anytype.url and space_id
export ANYTYPE_API_KEY=...                # never goes in the TOML file
cargo run -- --config config.toml init    # check the space, --apply to fix it
cargo run -- --config config.toml         # serve
```

Or with Docker:

```sh
docker run --rm -v "$PWD/config.toml:/config.toml:ro" \
  -e ANYTYPE_API_KEY -p 8080:8080 ghcr.io/<owner>/anytype-caldav --config /config.toml
```

A full setup with headless Anytype, the server and Calino behind Caddy is in
[`deploy/compose/`](deploy/compose/).

## Configuration

Everything is in one TOML file; [`config.example.toml`](config.example.toml)
describes every option. Secrets come from the environment:

| Variable | Needed for |
|---|---|
| `ANYTYPE_API_KEY` | always |
| `CALDAV_PASSWORD` | `[caldav]`, the shared login |
| `ACCOUNTS_SECRET` | accounts per person (`properties.assignee` must be set) |

Notifications, the default calendar names and the deadline line in a task's
description are in English or Russian, set by `calendar.language` (`"en"` by
default, or `"ru"`).

Properties are selected as `key:<stable-key>` or `id:<opaque-id>`, never by
display name, so renaming a property in Anytype breaks nothing. If a selector
matches nothing, the refresh fails and every property the type offers is logged:

```
WARN property available on type task property=key:due_date id:bafy... name:"Deadline" format:Date
```

## Commands

| Command | What it does |
|---|---|
| `anytype-caldav --config c.toml` | runs the server |
| `… init [--apply]` | checks the space's schema; `--apply` creates missing properties |
| `… generate [--apply]` | shows the occurrences of series due today; `--apply` creates them |
| `… users` | prints every person's name, login and password (needs `ACCOUNTS_SECRET`) |

`GET /healthz` reports on the process only and never contacts Anytype.

## Reminders

Web Push is optional. Subscriptions and handled reminders are stored in SQLite,
so they survive restarts. Keep the VAPID key stable, since replacing it
invalidates every subscription, and back it up together with the state file.

- Delivery is **at most once**: a reminder is claimed before it is sent, so a
  restart never repeats one, and a crash right after the claim loses it.
- Reminders missed by less than `push.late_window` are sent late; older ones
  are skipped. A reminder due while nothing is subscribed is not claimed.
- `properties.reminder` is a select whose option names are durations (`15m`,
  `1h`, `1d`, `1w`); each chosen option is one reminder. Without it,
  `reminders.lead_time` applies.
- An all-day task reminds at `reminders.all_day_time`.
- On iOS, Web Push works only after the site is added to the Home Screen and
  subscribed from there.

[`deploy/calino/`](deploy/calino/) has the script that subscribes from inside
Calino.

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
  unless origins are listed, and `*` is refused for it.

## Repository layout

| Path | What |
|---|---|
| `src/`, `tests/` | the server; `cargo test` runs everything without a live Anytype |
| `deploy/compose/` | Docker Compose kit: Anytype, the server, Caddy |
| `deploy/systemd/` | unit file for a host without Docker |
| `deploy/calino/` | reminders inside Calino, and the patch for hidden calendars |
| `scripts/` | optional tools for preparing a space; `scripts/history/` is one-off migrations |
| `docs/dev/` | design, decision log, plans and the handoff notes |

## Development

```sh
cargo test
```

Generated calendars are parsed back by a different crate than the one that
wrote them. The Anytype boundary is covered by fixtures; a live test against a
real installation is gated by the environment (`tests/live_feed.rs`).

Start with [`docs/dev/HANDOFF.md`](docs/dev/HANDOFF.md) for the current state
and the constraints that were expensive to learn.

## License

[GLWT (Good Luck With That) Public License](LICENSE).
