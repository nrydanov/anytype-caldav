# Compose kit

anytype-caldav with a headless Anytype of its own and Caddy in front, which
obtains the TLS certificate. Nothing but Docker is needed on the host.

1. In Anytype, the owner of the space makes an invite link.
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
   its account key: keep it. The bot then asks to join the space by the invite
   link, trying again every minute until the request goes through, and hands
   the space's id to the server, together with the API key and the session
   token `init` uses to set type headers over gRPC. If the invite needs
   approval, the owner approves the bot in Anytype; until then `init` fails and the server is
   restarted, so it comes up by itself once the bot is in.

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
`/.well-known/caldav` to `/dav/`.

## Backups

Back up the volumes `anytype-data`, `anytype-config` and `state` together
with `.env`: they hold the bot, the push subscriptions and the VAPID key. The
iCalendar feed is not routed by Caddy, since it has no password.
