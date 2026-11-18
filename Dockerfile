# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS chef
WORKDIR /app
RUN cargo install cargo-chef --locked

# ------------------------------------------------------------------------------
# Planner: extract the dependency recipe from the manifests and the source
# layout. Does not compile; runs in seconds.
# ------------------------------------------------------------------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ------------------------------------------------------------------------------
# Builder: compile the dependency tree from the recipe, then the real source. The
# expensive layer is only invalidated when Cargo.toml or Cargo.lock changes.
# ------------------------------------------------------------------------------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --bin bgpviews --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin bgpviews && \
    cp /app/target/release/bgpviews /usr/local/bin/bgpviews

# ------------------------------------------------------------------------------
# Runtime image
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim
# ca-certificates only: the archive is written to local disk, and nothing in the
# binary calls out to the network except the BMP listener it serves.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/bgpviews /usr/local/bin/bgpviews

# The archive is the operator's data, so the container writes it as an
# unprivileged user and never as root.
RUN groupadd --system bgpviews \
    && useradd --system --gid bgpviews --no-create-home --shell /usr/sbin/nologin bgpviews \
    && mkdir -p /var/lib/bgpviews /etc/bgpviews \
    && chown -R bgpviews:bgpviews /var/lib/bgpviews

USER bgpviews

# Mount the archive here for persistence, and the configuration at
# /etc/bgpviews/bgpviews.toml. The archive root inside that file has to live under
# this path, because it is the only directory the unprivileged user can write.
#
# Prefer a named volume (`-v bgpviews-archive:/var/lib/bgpviews`): Docker initialises
# its ownership from this image, so uid 999 can write immediately. A host bind mount
# keeps the host's ownership instead, so pass `--user "$(id -u):$(id -g)"` or
# `chown -R 999:999` the directory first.
VOLUME ["/var/lib/bgpviews"]

# BMP: routers connect to this port, so it has to be published and allowed by the
# client's `allow_from`.
EXPOSE 17901

# A bare `docker run` starts archiving with the mounted configuration; any other
# subcommand runs instead (`docker run IMAGE check --config ...`,
# `docker run IMAGE verify --config ...`).
ENTRYPOINT ["bgpviews"]
CMD ["run", "--config", "/etc/bgpviews/bgpviews.toml"]
