#!/bin/sh
# Runs headless Anytype for anytype-caldav and prepares its account on every
# start. Each step can run again without harm, so a changed .env is applied by
# starting the kit again:
#
# - the bot account is created only when the volume holds none, since a new
#   account would be a stranger to the space; with ANYTYPE_ACCOUNT_KEY an
#   existing bot logs in instead;
# - joining by ANYTYPE_INVITE_LINK again, or before the owner has let the bot
#   in, does no harm;
# - a new API key is issued on every start and handed to the server through
#   the shared volume; older keys keep working.
set -u

cli() { anytype --no-update-check "$@"; }

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

if cli auth status 2>&1 | grep -q "Not authenticated"; then
    if [ -n "${ANYTYPE_ACCOUNT_KEY:-}" ]; then
        cli auth login --account-key "$ANYTYPE_ACCOUNT_KEY" || exit 1
    else
        echo "No account in the volume yet: creating the bot. Keep the account key" \
            "printed below; it is the only way back into this account."
        cli auth create "${ANYTYPE_BOT_NAME:-anytype-caldav}" || exit 1
    fi
fi

if [ -n "${ANYTYPE_INVITE_LINK:-}" ]; then
    cli space join "$ANYTYPE_INVITE_LINK" ||
        echo "space join did not go through; that is expected when the account is already a member"
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
