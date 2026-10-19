# Build context is the relay crate root:  docker build -f deploy/Dockerfile .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
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
