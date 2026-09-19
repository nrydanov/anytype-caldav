#!/bin/sh
# Runs anytype-caldav with what the anytype service hands over through the
# shared volume. Without arguments it brings the space's schema up to date
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

if [ -n "${ACCOUNTS_SECRET:-}" ]; then
    export ANYTYPE_CALDAV__PROPERTIES__ASSIGNEE="${ANYTYPE_CALDAV__PROPERTIES__ASSIGNEE:-key:assignee}"
fi

if [ "$#" -eq 0 ]; then
    anytype-caldav init --apply
fi
exec anytype-caldav "$@"
