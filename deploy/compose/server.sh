#!/bin/sh
# The entrypoint of the server in the compose kit, shipped in the image as
# anytype-caldav-compose. Runs anytype-caldav with what the anytype service
# hands over through the shared volume. Without arguments it brings the space's schema up to date
# (init adds only what is missing) and serves; with arguments it runs that
# command instead, as in `docker compose run --rm server users`.
#
# Made once and kept in the state volume unless .env gives them: the CalDAV
# password, printed when made, and a random path for the iCalendar feed, which
# Caddy does not route. With ACCOUNTS_SECRET, people are read from Anytype's
# own assignee property.
set -eu

until [ -s /shared/api-key ] && [ -s /shared/session-token ]; do sleep 2; done

if [ -z "${ANYTYPE_CALDAV__ANYTYPE__SPACE_ID:-}" ]; then
    until [ -s /shared/space-id ]; do sleep 2; done
    ANYTYPE_CALDAV__ANYTYPE__SPACE_ID="$(cat /shared/space-id)"
    export ANYTYPE_CALDAV__ANYTYPE__SPACE_ID
fi

random() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }

if [ -z "${ANYTYPE_CALDAV__SERVER__FEED_PATH:-}" ]; then
    [ -s /data/feed-path ] || echo "/f/$(random 16)/todos.ics" > /data/feed-path
    ANYTYPE_CALDAV__SERVER__FEED_PATH="$(cat /data/feed-path)"
    export ANYTYPE_CALDAV__SERVER__FEED_PATH
fi

if [ -z "${CALDAV_PASSWORD:-}" ]; then
    if [ ! -s /data/caldav-password ]; then
        random 18 > /data/caldav-password
        echo "CalDAV login anytype, password $(cat /data/caldav-password)." \
            "Shown again by docker compose exec server cat /data/caldav-password"
    fi
    export CALDAV_PASSWORD_FILE=/data/caldav-password
fi

# With Calino beside the server, a tapped reminder opens it, and Safari on iOS,
# which shows a reminder without a service worker, needs that address.
case ",${COMPOSE_PROFILES:-}," in
*,calino,*)
    export ANYTYPE_CALDAV__PUSH__APP_URL="${ANYTYPE_CALDAV__PUSH__APP_URL:-https://${ANYTYPE_CALDAV_DOMAIN}/}"
    ;;
esac

if [ -n "${ACCOUNTS_SECRET:-}" ]; then
    export ANYTYPE_CALDAV__PROPERTIES__ASSIGNEE="${ANYTYPE_CALDAV__PROPERTIES__ASSIGNEE:-key:assignee}"
fi

# Until the bot is in the space and has loaded it, init fails; it is tried
# again rather than the container restarted.
if [ "$#" -eq 0 ]; then
    until anytype-caldav init --apply; do
        echo "init failed (see above); trying again in 30 s. This passes by itself" \
            "while the owner has yet to let the bot in or the space is still" \
            "loading. If it does not, check that the host reaches the sync network" \
            "(docker compose logs anytype), fix what the error names in .env and" \
            "run docker compose up -d."
        sleep 30
    done
fi
exec anytype-caldav "$@"
