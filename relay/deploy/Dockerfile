# Build context is the relay crate root:  docker build -f deploy/Dockerfile .
# Pinned to a release (a floating tag changes what is built between two deploys). Bump on purpose.
FROM rust:1.93.0-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# A point release, not "bookworm-slim": security refreshes of the base are taken by bumping this line on purpose.
FROM debian:12.11-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin relay \
 && mkdir -p /var/lib/desklink-relay /etc/desklink-relay \
 && chown relay:relay /var/lib/desklink-relay
COPY --from=build /src/target/release/desklink-relay /usr/local/bin/desklink-relay
USER relay
VOLUME /var/lib/desklink-relay
ENTRYPOINT ["/usr/local/bin/desklink-relay", "--config", "/etc/desklink-relay/config.toml"]
