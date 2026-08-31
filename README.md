# anytype-task-exporter

A local, read-only bridge from one Anytype space to an iCalendar `VTODO`
subscription, with optional server-side Web Push reminders. Anytype stays the
source of truth; the exporter never writes task state back.

Design and rationale: [`docs/superpowers/specs/2026-08-29-anytype-vtodo-exporter-design.md`](docs/superpowers/specs/2026-08-29-anytype-vtodo-exporter-design.md).

**Picking this up?** Start with [`docs/HANDOFF.md`](docs/HANDOFF.md): current
state, the ordered next steps, and the constraints that are expensive to
rediscover (Apple cannot show
`VTODO` by any route).

## Running

```sh
cp config.example.toml config.toml     # then edit space_id and the properties
export ANYTYPE_API_KEY=...             # never goes in the TOML file
cargo run -- --config config.toml
```

Subscribe a `VTODO`-capable client to `http://127.0.0.1:8080/todos.ics`.
The target client is [Calino](https://calino.io/); Google Calendar and Apple
Calendar ignore `VTODO` in subscribed feeds and will show an empty calendar.

`GET /healthz` reports on this process only and never contacts Anytype.

## Server-side Web Push

Web Push is optional. When enabled, subscriptions and handled reminder IDs are
stored in SQLite so they survive restarts:

```toml
[push]
enabled = true
private_key_file = "~/.config/vapid_private.pem"
state_file = "~/.local/share/anytype-task-exporter/state.sqlite3"
poll_interval = "30s"
late_window = "1h"
```

Keep the VAPID key stable: replacing it invalidates existing browser
subscriptions. Treat both the key and SQLite file as private and back them up
together. The database is created with mode `0600` on Unix.

The scheduler reads Anytype independently of feed requests, once per
`poll_interval`, continuously — `server.min_refresh_interval` does not bound
this. Raise `poll_interval` on a small host; the only cost is that reminders
land up to that much later.

It sends reminders missed by at most `late_window`; older ones are recorded as
expired. Reminder claims are persisted before contacting the push service, so
delivery is **at-most-once**: restarts do not duplicate notifications, but a
crash or transport failure after the claim is not retried. A reminder that
falls due while nothing is subscribed is not claimed at all, so it still
arrives if a browser subscribes within the late window.

On iOS, Web Push works only after the site is added to the Home Screen and the
subscription is created from that installed app.

## Per-task reminders

Set `properties.reminder` to a select or multi-select property whose option
**names** are durations — `15m`, `30m`, `1h`, `2h`, `1d`, `1w`. Every chosen
option produces one reminder, so a task can warn you a day ahead *and* half an
hour ahead. Anytype snake_cases option keys (`1d` is stored as `1_d`), which is
why the name is what gets parsed; an option that is not a duration is logged
and skipped rather than failing the task.

A task that leaves the property empty falls back to `reminders.lead_time`.

The notification titles itself with the task name and says why it arrived —
«Дедлайн через 2 часа — сегодня в 18:00», «Дедлайн завтра в 18:00», «Дедлайн
сегодня» — computed when the push is sent, so one delayed by the late window
reads «Дедлайн был сегодня в 18:00» instead of promising a passed future.

The lead counts back from the deadline, or from the scheduled date when there
is no deadline. For an all-day date there is no *default* lead — such a task
announces itself at `reminders.all_day_time` — but an explicitly chosen lead
does count back from that hour, so `1d` on an all-day task due Monday fires at
09:00 on Sunday.

## Finding your property selectors

Each property is configured as `key:<stable-key>` or `id:<opaque-id>`. If a
selector matches nothing, the refresh fails and every property the type
actually offers is logged with its key, id, name and format:

```
WARN property available on type task property=key:due_date id:bafy... name:"Deadline" format:Date
```

Set the selectors from that list. A selector is never matched by display name,
so renaming a property in Anytype does not break the feed.

## Notes on behaviour

- **Timed values are emitted as UTC instants**, not `TZID=...`. A `TZID`
  reference obliges the document to carry a matching `VTIMEZONE` component
  (RFC 5545 §3.6.5); a UTC instant is unambiguous without one.
- **A value at midnight becomes an all-day task.** Which midnight is decided by
  `calendar.date_only_timezone`. If your all-day tasks appear at 04:00, set it
  to `"UTC"`.
- **A failed refresh serves the last good feed** with `X-Exporter-Stale: true`.
  Before any successful refresh it returns `503` rather than an empty calendar,
  because a subscribed client reads an empty calendar as "delete everything".
- **Zero tasks is logged as a warning**, since a misconfigured space or a
  renamed type looks identical to a genuinely empty one.
- **CORS is off unless you list origins.** A wildcard is refused: the feed is
  unauthenticated, so `*` would let any page you visit read your whole task
  list, which the loopback bind alone would otherwise prevent.

## Tests

```sh
cargo test
```

Generated feeds are parsed back by a different crate than the one that produced
them. The Anytype boundary is covered by fixtures; a live test against a real
installation is environment-gated and not part of the normal run.
