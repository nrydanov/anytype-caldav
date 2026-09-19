FROM rust:1.97-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked && cp target/release/anytype-caldav /anytype-caldav

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data anytype-caldav \
 && mkdir /data && chown anytype-caldav /data
COPY --from=build /anytype-caldav /usr/local/bin/anytype-caldav
# The compose kit's entrypoint. Shipped in the image rather than mounted, so
# the host's file modes do not decide whether this user can read it.
COPY deploy/compose/server.sh /usr/local/bin/anytype-caldav-compose
RUN chmod 755 /usr/local/bin/anytype-caldav-compose
USER anytype-caldav
WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["anytype-caldav"]
CMD []
