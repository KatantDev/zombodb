# syntax=docker/dockerfile:1

# PostgreSQL + ZomboDB compiled from this repository's sources.
#
#   docker build -t postgres-zombodb .
#
# Published automatically to ghcr.io/katantdev/postgres-zombodb by
# .github/workflows/docker-publish.yml

ARG PG_MAJOR=15

FROM postgres:${PG_MAJOR}-alpine AS builder
ARG PG_MAJOR
ARG PGRX_VERSION=0.13.1
ARG RUST_VERSION=1.96.0
ARG TARGETARCH

# dynamically link against musl instead of producing a static binary --
# a cdylib extension cannot be crt-static
ENV RUSTFLAGS="-C target-feature=-crt-static"

RUN apk add --no-cache \
    bash \
    clang-dev \
    clang-libs \
    coreutils \
    curl \
    gcc \
    git \
    make \
    musl-dev \
    openssl-dev \
    tar \
    util-linux-dev

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

WORKDIR /root
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}"
ENV PATH="/root/.cargo/bin:${PATH}"

# source-independent layers: stay cached between builds.
# the cache mounts additionally survive Dockerfile changes on local rebuilds
RUN --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/root/.cargo/registry,sharing=locked \
    cargo install cargo-pgrx --version "${PGRX_VERSION}" --locked

RUN cargo pgrx init --pg${PG_MAJOR}="$(which pg_config)"

WORKDIR /zombodb
COPY . .

# the 'artifacts' profile is upstream's release-artifact profile:
# fat LTO + a single codegen unit = the fastest runtime binary
RUN --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,id=zombodb-target-${TARGETARCH},target=/zombodb/target,sharing=locked \
    cargo pgrx install --profile artifacts

# ---------------------------------------------------------------------------
# final stage: stock postgres + just the compiled extension
# ---------------------------------------------------------------------------
FROM postgres:${PG_MAJOR}-alpine

LABEL org.opencontainers.image.source="https://github.com/KatantDev/zombodb" \
      org.opencontainers.image.description="PostgreSQL with the ZomboDB Elasticsearch index access method" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY --from=builder /usr/local/lib/postgresql/zombodb.so /usr/local/lib/postgresql/
COPY --from=builder /usr/local/share/postgresql/extension/zombodb* /usr/local/share/postgresql/extension/

# libgcc is needed at runtime by the Rust-built extension;
# the initdb script creates the extension in $POSTGRES_DB on first start
RUN apk add --no-cache libgcc \
    && echo 'CREATE EXTENSION IF NOT EXISTS zombodb;' > /docker-entrypoint-initdb.d/10-zombodb.sql

# no USER/EXPOSE/VOLUME/HEALTHCHECK overrides: the official postgres
# entrypoint already drops to the postgres user, exposes 5432 and manages
# PGDATA; healthchecks are defined by the orchestrator (docker-compose)
