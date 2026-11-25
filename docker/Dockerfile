# syntax=docker/dockerfile:1
#
# Clean multi-stage image for the megu Alpha release.
#
# The builder compiles the release binary on the pinned Rust toolchain; the
# runtime carries only the binary, the licence texts, the public-facing tools
# and the entrypoint. The image ships no Mojang data: no server jar, no
# config_packets.bin, no datapacks/vanilla tree, no generated *_1211* asset
# file. Every generated table the server reads is loaded at runtime from the
# /data volume, regenerated from the operator's own 1.21.1 server.jar
# (tools/vanilla_data/gen_vanilla_data.sh - see docs/release-checklist.md), so
# no *_1211* input is needed at compile time.
#
# A stripped public export builds this image as-is: no generated or
# Mojang-derived file is part of the build context.

FROM rust:1.98.0-bookworm AS builder
WORKDIR /build

# Everything `cargo build --release --locked` (including `--features mod-suite`
# and the companion CLI crates) resolves, and nothing more.
ARG CARGO_FEATURES="mod-suite,create-offload"
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY core/Cargo.toml core/Cargo.lock core/
COPY core/src core/src
COPY proto/Cargo.toml proto/Cargo.lock proto/
COPY proto/src proto/src
COPY state-store/Cargo.toml state-store/Cargo.lock state-store/
COPY state-store/src state-store/src
COPY server-files/Cargo.toml server-files/Cargo.lock server-files/
COPY server-files/src server-files/src
COPY wasm-mod-runtime/Cargo.toml wasm-mod-runtime/Cargo.lock wasm-mod-runtime/
COPY wasm-mod-runtime/src wasm-mod-runtime/src
COPY mod-adapter-core/Cargo.toml mod-adapter-core/Cargo.lock mod-adapter-core/
COPY mod-adapter-core/src mod-adapter-core/src
COPY mod-offload/Cargo.toml mod-offload/Cargo.lock mod-offload/
COPY mod-offload/src mod-offload/src
COPY create-adapter/Cargo.toml create-adapter/Cargo.lock create-adapter/
COPY create-adapter/src create-adapter/src

RUN if [ -n "$CARGO_FEATURES" ]; then \
      cargo build --release --locked --features "$CARGO_FEATURES"; \
    else \
      cargo build --release --locked; \
    fi \
    && cargo build --manifest-path server-files/Cargo.toml --release --locked --bin megu-init \
    && cargo build --manifest-path state-store/Cargo.toml --release --locked --bin megu-world-migrate \
    && cargo build --manifest-path mod-adapter-core/Cargo.toml --release --locked --bin megu-mod-check

FROM debian:bookworm-slim AS runtime

# python3 runs tools/mod_launcher.py (the MEGU_MODPACK pre-flight) and
# tools/vanilla_data/version_pack.py; ca-certificates keeps TLS fetches sane.
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user. /data is the server's working directory: it holds
# the world save, server.toml and the regenerated vanilla data, so it must be
# writable by this uid before the entrypoint runs.
RUN groupadd --gid 10001 megu \
    && useradd --uid 10001 --gid megu --no-create-home --shell /usr/sbin/nologin megu \
    && mkdir -p /data \
    && chown megu:megu /data

COPY --from=builder /build/target/release/megu /usr/local/bin/megu
COPY --from=builder /build/server-files/target/release/megu-init /usr/local/bin/megu-init
COPY --from=builder /build/state-store/target/release/megu-world-migrate /usr/local/bin/megu-world-migrate
COPY --from=builder /build/mod-adapter-core/target/release/megu-mod-check /usr/local/bin/megu-mod-check

# The same payload the release bundle ships: licence texts plus the tools a
# public user needs (modpack pre-flight, vanilla-data regeneration).
COPY LICENSE docs/COPYING.GPL NOTICE LICENSING.md THIRD_PARTY.md /opt/megu/
COPY tools/mod_launcher.py /opt/megu/tools/mod_launcher.py
COPY tools/vanilla_data/gen_vanilla_data.sh tools/vanilla_data/version_pack.py /opt/megu/tools/vanilla_data/
COPY docker/entrypoint.sh /opt/megu/entrypoint.sh
RUN chmod 0755 /usr/local/bin/megu /usr/local/bin/megu-init \
        /usr/local/bin/megu-world-migrate /usr/local/bin/megu-mod-check \
        /opt/megu/entrypoint.sh \
        /opt/megu/tools/mod_launcher.py \
        /opt/megu/tools/vanilla_data/gen_vanilla_data.sh \
        /opt/megu/tools/vanilla_data/version_pack.py

USER megu
WORKDIR /data
EXPOSE 25565/tcp
ENTRYPOINT ["/opt/megu/entrypoint.sh"]
