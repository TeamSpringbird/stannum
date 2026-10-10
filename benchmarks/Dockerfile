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
COPY engine engine
# A workspace member: cargo reads its manifest; nothing here builds it.
COPY bench bench
# An x86-64 build for a CPU level (x86-64-v3, x86-64-v4); empty, the baseline
# PGDG packages target (the kernels pick AVX2 or AVX-512 at runtime either
# way). Set per target, not as RUSTFLAGS, which would replace
# .cargo/config.toml's link flags.
ARG STANNUM_TARGET_CPU=
# Profile-guided optimization (docs/architecture/pgo.md). Empty: none. 1: use
# the committed profile for this architecture, benchmarks/pgo/<arch>.profdata.
# generate: an instrumented build whose backends write raw profiles under
# /var/lib/postgresql/pgo-raw (script/pgo-build trains one).
ARG STANNUM_PGO=
COPY benchmarks/pgo benchmarks/pgo
# Keep optimized code and line tables for the owned perf diagnostics.
RUN case "$STANNUM_PGO" in \
      "") pgo= ;; \
      generate) pgo="-C profile-generate=/var/lib/postgresql/pgo-raw" ;; \
      1) pgo="-C profile-use=/src/benchmarks/pgo/$(uname -m).profdata -C llvm-args=-no-pgo-warn-mismatch" \
         && test -s "benchmarks/pgo/$(uname -m).profdata" ;; \
      *) echo "STANNUM_PGO is empty, 1 or generate" >&2; exit 2 ;; \
    esac \
    && CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="${STANNUM_TARGET_CPU:+-C target-cpu=$STANNUM_TARGET_CPU} $pgo" \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="$pgo" \
    CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none cargo pgrx install --package stannum --no-default-features --features pg18 --release

FROM runtime
USER root
ARG STANNUM_SOURCE_SHA256=unknown
ARG STANNUM_COMMIT=unknown
ARG RECIPE_SHA256=unknown
ARG STANNUM_TARGET_CPU=
ARG STANNUM_PGO=
LABEL benchmark.stannum_source_sha256=${STANNUM_SOURCE_SHA256} benchmark.stannum_commit=${STANNUM_COMMIT} benchmark.recipe_sha256=${RECIPE_SHA256} benchmark.stannum_target_cpu=${STANNUM_TARGET_CPU} benchmark.stannum_pgo=${STANNUM_PGO}
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
