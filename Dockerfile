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
USER anytype-caldav
WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["anytype-caldav"]
CMD []
