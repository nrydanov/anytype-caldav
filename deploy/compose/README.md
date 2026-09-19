# Compose kit

anytype-caldav with a headless Anytype of its own and Caddy in front, which
obtains the TLS certificate. Nothing but Docker is needed on the host:

- Docker Engine or Docker Desktop with Compose v2.17 or later (`docker
  compose`, not the old `docker-compose`);
- a 64-bit host, x86-64 or ARM64;
- ports 80 and 443 free, which rootless Docker and Podman cannot bind; see
  [Behind a proxy of your own](#behind-a-proxy-of-your-own) otherwise;
- outbound access to Anytype's network, or to the self-hosted one.

The images are pulled, so the host builds nothing; building the server from
source (`docker compose up --build`) takes far more memory than running it.

1. In Anytype, the owner of the space makes an invite link with editor
   rights (the bot creates types and writes tasks) and without approval, so
   the bot joins at once. Anyone holding such a link can join: revoke it once
   the bot is in.
2. Fill in the settings:

   ```sh
   cp env.example .env && chmod 600 .env
   ```

   `ANYTYPE_CALDAV_DOMAIN`, `ANYTYPE_INVITE_LINK` and
   `ANYTYPE_CALDAV__CALENDAR__TIMEZONE` are required. Every other setting of
   `config.example.toml` can be added as `ANYTYPE_CALDAV__<SECTION>__<KEY>`.
   The variables of the kit itself are prefixed too, since Compose prefers a
   variable of the shell it runs in over `.env`, and a shell may well have a
   `DOMAIN` or a `LANGUAGE` of its own.

   If the space lives on a self-hosted any-sync network, put the network's
   `client.yml` into [`network/`](network/) before the first start.

3. Start:

   ```sh
   docker compose up -d
   docker compose logs -f
   ```

   On the first start the `anytype` service creates a bot account and prints
   its account key. Put it into `.env` as `ANYTYPE_ACCOUNT_KEY`: with it, lost
   volumes or a new host bring the same bot back.
   `docker compose exec anytype account-key` shows it again. The bot then asks to join the space by the invite
   link, trying again every minute until the request goes through, and hands
   the space's id to the server, together with the API key and the session
   token `init` uses to set type headers over gRPC. If the invite needs
   approval, the owner approves the bot in Anytype; until then the server
   tries `init` again every 30 seconds and comes up by itself once the bot is
   in.

4. Connect a CalDAV client to `https://<ANYTYPE_CALDAV_DOMAIN>/dav/` with the
   username `anytype` and `CALDAV_PASSWORD`. Without one in `.env`, the server
   made it on the first start and printed it in its log;
   `docker compose exec server cat /data/caldav-password` shows it again. With
   `ACCOUNTS_SECRET` set, everyone's own login is printed by
   `docker compose run --rm server users`.

Every step can run again without harm: after changing `.env`, run
`docker compose up -d` again. The bot is created only once and kept in a
volume, a link joined once is not joined again (a new link is), a fresh API
key is issued on every
start, `init --apply` creates only what is missing, and the VAPID key is made
only when there is none.

The kit serves tasks and events, makes the occurrences of recurring series and
reads the reminder and tag properties `init` creates; `.env` can turn events
and series off.

## Calino

The kit can serve [the Calino fork](https://github.com/nrydanov/calino), a
calendar in the browser, at the domain's root. Add to `.env`:

```sh
COMPOSE_PROFILES=calino
```

and run `docker compose up -d`. In Calino, add a CalDAV account with the
address `https://<ANYTYPE_CALDAV_DOMAIN>/dav/`, the username `anytype` and
the CalDAV password, or a person's own login. Then:

- reminders: turn them on in Calino's settings; on an iPhone, first add
  Calino to the Home Screen and open it from there;
- offline: once opened, Calino starts without a network and shows the
  calendar it last synced; after an update the next visit online loads the
  new build.

The same origin needs no CORS. To take Calino away again, remove the line and
run `docker compose rm -sf calino`.

## Behind a proxy of your own

When the host already runs a reverse proxy on ports 80 and 443, leave Caddy
out and publish the server's port on localhost with a `compose.override.yaml`:

```yaml
services:
  server:
    ports: ["127.0.0.1:8080:8080"]
```

Then start only the two services, `docker compose up -d anytype server`
(`ANYTYPE_CALDAV_DOMAIN` is still read, so leave it set), and route the paths of
[`Caddyfile`](Caddyfile) to `127.0.0.1:8080` in your proxy: `/dav`, `/dav/*`,
`/push/*` and `/healthz`, plus a redirect of
`/.well-known/caldav` to `/dav/`. Calino is then yours to serve as well.

## Backups

Back up the volumes `anytype-data`, `anytype-config` and `state` together
with `.env`: they hold the bot, the push subscriptions and the VAPID key. The
iCalendar feed is not routed by Caddy, since it has no password.
