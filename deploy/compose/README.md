# Compose kit

Runs the server and a build of Calino behind Caddy, which obtains the
certificate itself. Headless Anytype runs on the host.

1. Start headless Anytype on the host, let its account into the space and
   create an API key (`anytype serve`, see the Anytype CLI documentation).
2. Put a build of Calino (its `dist/`) into `./calino`.
3. Prepare the files:

   ```sh
   cp env.example .env && chmod 600 .env          # fill it in
   cp ../../config.example.toml config.toml       # see below
   openssl ecparam -name prime256v1 -genkey -noout -out vapid_private.pem
   ```

   In `config.toml` set `anytype.url = "http://host.docker.internal:31012"`,
   `space_id`, `server.listen = "0.0.0.0:8080"`,
   `push.private_key_file = "/config/vapid_private.pem"`,
   `push.state_file = "/data/state.sqlite3"` and `push.app_url` to
   `https://<DOMAIN>/`.

4. Check the space, then start:

   ```sh
   docker compose run --rm server --config /config/config.toml init
   docker compose up -d
   ```

5. With accounts per person on, print them to hand out:
   `docker compose run --rm server --config /config/config.toml users`.

Back up `.env`, `vapid_private.pem` and the `state` volume together. A second
space is a second `server` service with its own config and a path or domain of
its own in the Caddyfile.
