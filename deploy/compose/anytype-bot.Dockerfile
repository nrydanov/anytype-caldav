# The kit's anytype service: Anytype's CLI with socat, which relays its gRPC
# port (bound to 127.0.0.1 only) to the kit's network, and anytype.sh, which
# prepares the bot on every start. Published as ghcr.io/nrydanov/anytype-bot.
ARG ANYTYPE_CLI_VERSION=v0.3.7
FROM ghcr.io/anyproto/anytype-cli:${ANYTYPE_CLI_VERSION}
RUN apk add --no-cache socat
COPY anytype.sh /usr/local/bin/anytype-bot
RUN chmod 755 /usr/local/bin/anytype-bot \
 && printf '#!/bin/sh\nexec anytype-bot account-key\n' > /usr/local/bin/account-key \
 && chmod 755 /usr/local/bin/account-key
ENTRYPOINT ["anytype-bot"]
