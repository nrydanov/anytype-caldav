#!/bin/sh
# Runs headless Anytype for anytype-caldav and prepares its account on every
# start. Each step can run again without harm, so a changed .env is applied by
# starting the kit again:
#
# - the bot account is created only when the volume holds none, since a new
#   account would be a stranger to the space; with ANYTYPE_ACCOUNT_KEY an
#   existing bot logs in instead;
# - the space is joined by ANYTYPE_INVITE_LINK, and its id is handed to the
#   server, which serves that space and not the bot's own. A join that does not
#   go through is tried again every minute; a link joined once is not used
#   again, since an endless round of joins would be the only harm, and a new
#   link joins anew;
# - a new API key is issued on every start and handed to the server through
#   the shared volume; older keys keep working;
# - the session token is handed over too, with a relay to Anytype's gRPC port,
#   which listens on localhost only: the server sets the header of a type,
#   which the API cannot, over gRPC.
set -u

# Stale until the account is up again.
rm -f /shared/session-token

cli() { anytype --no-update-check "$@"; }

# A self-hosted network, when network/client.yml is there; Anytype's own
# otherwise. Only creating the account or logging in takes it: the account
# remembers its network from then on.
network=""
if [ -s /setup/network/client.yml ]; then
    network="--network-config /setup/network/client.yml"
    echo "using the self-hosted network of network/client.yml"
fi

anytype --no-update-check serve --listen-address 0.0.0.0:31012 &
serve=$!
trap 'kill "$serve" 2>/dev/null; wait "$serve"; exit 0' TERM INT

# The commands below talk to the server over gRPC.
until nc -z 127.0.0.1 31010; do
    if ! kill -0 "$serve" 2>/dev/null; then
        echo "anytype serve exited" >&2
        exit 1
    fi
    sleep 1
done

# The CLI keeps the account key in its config once an account exists; the
# config is JSON it writes itself, unlike the text of `auth status`.
if ! grep -q '"accountKey": *"[^"]' /root/.anytype/config.json 2>/dev/null; then
    if [ -n "${ANYTYPE_ACCOUNT_KEY:-}" ]; then
        # shellcheck disable=SC2086
        cli auth login --account-key "$ANYTYPE_ACCOUNT_KEY" $network || exit 1
    else
        echo "No account in the volume yet: creating the bot. Keep the account key" \
            "printed below; it is the only way back into this account."
        # shellcheck disable=SC2086
        cli auth create "${ANYTYPE_BOT_NAME:-anytype-caldav}" $network || exit 1
    fi
fi

# After a restart the server logs in with the stored key on its own, a moment
# after its port opens; until then every call is refused. A listing needs a
# live session, so it tells when the account is ready.
ready=0
for _ in $(seq 1 120); do
    if cli space list >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 1
done
if [ "$ready" -ne 1 ]; then
    echo "the account did not come up within two minutes" >&2
    exit 1
fi

socat TCP-LISTEN:31020,fork,reuseaddr TCP:127.0.0.1:31010 &
token=$(sed -n 's/.*"sessionToken": *"\([^"]*\)".*/\1/p' /root/.anytype/config.json)
if [ -n "$token" ]; then
    printf '%s\n' "$token" > /shared/session-token.new &&
        mv /shared/session-token.new /shared/session-token
else
    echo "no session token in the CLI config; init leaves type headers alone" >&2
fi

join_space() {
    out=$(cli space join "$ANYTYPE_INVITE_LINK" 2>&1)
    printf '%s\n' "$out" | grep -v '"level":"DEBUG"'
    id=$(printf '%s\n' "$out" | sed 's/\x1b\[[0-9;]*m//g' |
        sed -n "s/.*join request to space '\([^']*\)'.*/\1/p" | tail -n 1)
    [ -n "$id" ] || return 1
    printf '%s\n' "$id" > /shared/space-id.new && mv /shared/space-id.new /shared/space-id
    printf '%s\n' "$ANYTYPE_INVITE_LINK" > /shared/space-invite
    echo "space $id: join request sent; the server serves this space" \
        "once the owner has let the bot in"
}

if [ -z "${ANYTYPE_INVITE_LINK:-}" ]; then
    echo "ANYTYPE_INVITE_LINK is not set: the bot joins no space" >&2
elif [ -s /shared/space-id ] &&
    [ "$(cat /shared/space-invite 2>/dev/null)" = "$ANYTYPE_INVITE_LINK" ]; then
    echo "space $(cat /shared/space-id): joined by this invite link before"
else
    (
        until join_space; do
            echo "joining the space did not go through (see above); trying again in a minute"
            sleep 60
        done
    ) &
fi

key=$(cli auth apikey create anytype-caldav 2>&1 |
    sed 's/\x1b\[[0-9;]*m//g' |
    sed -n 's/.*Key: *\([^[:space:]]*\).*/\1/p' | tail -n 1)
if [ -n "$key" ]; then
    printf '%s\n' "$key" > /shared/api-key.new && mv /shared/api-key.new /shared/api-key
    echo "API key for the server written to the shared volume"
else
    echo "could not create an API key; the server keeps waiting for one" >&2
fi

wait "$serve"
