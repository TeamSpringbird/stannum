# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Native x86-64 published-reference baseline; every engine shares this runtime.
ARG BASE=paradedb/paradedb@sha256:9b7e2540dcb9e6e67825b9a2fa665125af1f0c4e488d50f1afa15ae7dd274359
FROM ${BASE} AS runtime
USER root
# Pin both compilation and measurement to the published PostgreSQL minor version.
ARG PG_PACKAGE_VERSION=18.6-1.pgdg13+2
RUN apt-get update && apt-get install -y --no-install-recommends \
    postgresql-18="${PG_PACKAGE_VERSION}" postgresql-client-18="${PG_PACKAGE_VERSION}" \
    && rm -rf /var/lib/apt/lists/* \
    && /usr/lib/postgresql/18/bin/postgres --version | grep -F 'PostgreSQL) 18.6'

FROM runtime AS builder
USER root
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential clang libclang-dev pkg-config libssl-dev curl ca-certificates git \
    postgresql-server-dev-18="$(dpkg-query -W -f='${Version}' postgresql-18)" \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain 1.96.0
RUN cargo install cargo-pgrx --version 0.19.1 --locked --jobs 4 \
    && cargo pgrx init --pg18=/usr/lib/postgresql/18/bin/pg_config
ARG PGTEXT_COMMIT=d7f04d59fa870902ced5f28689cad307884c0787
RUN git clone https://github.com/timescale/pg_textsearch.git /tmp/pg-textsearch \
    && git -C /tmp/pg-textsearch checkout ${PGTEXT_COMMIT} \
    && make -C /tmp/pg-textsearch -j4 PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config \
    && make -C /tmp/pg-textsearch install PG_CONFIG=/usr/lib/postgresql/18/bin/pg_config
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY postgres postgres
COPY tinql tinql
COPY tokenizer tokenizer
COPY boldi-vigna boldi-vigna
COPY segment segment
# Keep optimized code and line tables for the owned perf diagnostics.
RUN CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none cargo pgrx install --package stannum --no-default-features --features pg18 --release

FROM runtime
USER root
ARG STANNUM_SOURCE_SHA256=unknown
ARG STANNUM_COMMIT=unknown
ARG RECIPE_SHA256=unknown
LABEL benchmark.stannum_source_sha256=${STANNUM_SOURCE_SHA256} benchmark.stannum_commit=${STANNUM_COMMIT} benchmark.recipe_sha256=${RECIPE_SHA256}
COPY --from=builder /usr/lib/postgresql/18/lib/stannum.so /usr/lib/postgresql/18/lib/
COPY --from=builder /usr/share/postgresql/18/extension/stannum* /usr/share/postgresql/18/extension/
COPY --from=builder /usr/lib/postgresql/18/lib/pg_textsearch.so /usr/lib/postgresql/18/lib/
COPY --from=builder /usr/share/postgresql/18/extension/pg_textsearch* /usr/share/postgresql/18/extension/
# The campaign owns settings/extension creation; omit the distribution's auto-tuner
# and unrelated extension bootstrap (which otherwise requires pg_cron).
# The script exists only in the ParadeDB base; another BASE, such as a
# multi-arch postgres image for native ARM rehearsals, has nothing to remove.
RUN rm -f /docker-entrypoint-initdb.d/10_bootstrap_paradedb.sh \
    && mkdir /benchmark && dpkg-query -W > /benchmark/packages.txt
