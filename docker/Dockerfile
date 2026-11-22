# syntax=docker/dockerfile:1
#
# Clean multi-stage image for the mc-spine Alpha release.
#
# The builder compiles the release binary on the pinned Rust toolchain; the
# runtime carries only the binary, the licence texts, the public-facing tools
# and the entrypoint. The image ships no Mojang data: no server jar, no
# config_packets.bin, no datapacks/vanilla tree, no generated *_1211* asset
# file. The four generated tables the binary needs are compiled into it by
# include_bytes! in the builder; everything the server reads at runtime is
# regenerated into the /data volume from the operator's own 1.21.1 server.jar
# (tools/vanilla_data/gen_vanilla_data.sh - see docs/release-checklist.md).
#
# Build from a checkout that carries the compile-time tables: the dev tree,
# or the public export after one gen_vanilla_data.sh run. The export strips
# those inputs, so a bare export context fails the build - by design.

FROM rust:1.98.0-bookworm AS builder
WORKDIR /build

# Everything `cargo build --release --locked` (including `--features mod-suite`
# and the companion CLI crates) resolves, and nothing more.
ARG CARGO_FEATURES=""
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY core/Cargo.toml core/Cargo.lock core/
COPY core/src core/src
COPY proto/Cargo.toml proto/Cargo.lock proto/
COPY proto/src proto/src
COPY state-store/Cargo.toml state-store/Cargo.lock state-store/
COPY state-store/src state-store/src
COPY server-files/Cargo.toml server-files/Cargo.lock server-files/
COPY server-files/src server-files/src
COPY mod-adapter-core/Cargo.toml mod-adapter-core/Cargo.lock mod-adapter-core/
COPY mod-adapter-core/src mod-adapter-core/src
COPY mod-offload/Cargo.toml mod-offload/Cargo.lock mod-offload/
COPY mod-offload/src mod-offload/src
COPY create-adapter/Cargo.toml create-adapter/Cargo.lock create-adapter/
COPY create-adapter/src create-adapter/src
COPY assets/feature_order_1211.json assets/feature_order_1211.json
COPY assets/structure_state_transforms_1211.bin assets/structure_state_transforms_1211.bin
COPY datapacks/vanilla/data/minecraft/dimension_type/overworld.json \
     datapacks/vanilla/data/minecraft/dimension_type/the_nether.json \
     datapacks/vanilla/data/minecraft/dimension_type/

RUN if [ -n "$CARGO_FEATURES" ]; then \
      cargo build --release --locked --features "$CARGO_FEATURES"; \
    else \
      cargo build --release --locked; \
    fi \
    && cargo build --manifest-path server-files/Cargo.toml --release --locked --bin mc-spine-init \
    && cargo build --manifest-path state-store/Cargo.toml --release --locked --bin mc-spine-world-migrate \
    && cargo build --manifest-path mod-adapter-core/Cargo.toml --release --locked --bin mc-spine-mod-check

FROM debian:bookworm-slim AS runtime

# python3 runs tools/mod_launcher.py (the MC_SPINE_MODPACK pre-flight) and
# tools/vanilla_data/version_pack.py; ca-certificates keeps TLS fetches sane.
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root runtime user. /data is the server's working directory: it holds
# the world save, server.toml and the regenerated vanilla data, so it must be
# writable by this uid before the entrypoint runs.
RUN groupadd --gid 10001 mc-spine \
    && useradd --uid 10001 --gid mc-spine --no-create-home --shell /usr/sbin/nologin mc-spine \
    && mkdir -p /data \
    && chown mc-spine:mc-spine /data

COPY --from=builder /build/target/release/mc-spine /usr/local/bin/mc-spine
COPY --from=builder /build/server-files/target/release/mc-spine-init /usr/local/bin/mc-spine-init
COPY --from=builder /build/state-store/target/release/mc-spine-world-migrate /usr/local/bin/mc-spine-world-migrate
COPY --from=builder /build/mod-adapter-core/target/release/mc-spine-mod-check /usr/local/bin/mc-spine-mod-check

# The same payload the release bundle ships: licence texts plus the tools a
# public user needs (modpack pre-flight, vanilla-data regeneration).
COPY COPYING.LESSER NOTICE LICENSING.md THIRD_PARTY.md /opt/mc-spine/
COPY tools/mod_launcher.py /opt/mc-spine/tools/mod_launcher.py
COPY tools/vanilla_data/gen_vanilla_data.sh tools/vanilla_data/version_pack.py /opt/mc-spine/tools/vanilla_data/
COPY docker/entrypoint.sh /opt/mc-spine/entrypoint.sh
RUN chmod 0755 /usr/local/bin/mc-spine /usr/local/bin/mc-spine-init \
        /usr/local/bin/mc-spine-world-migrate /usr/local/bin/mc-spine-mod-check \
        /opt/mc-spine/entrypoint.sh \
        /opt/mc-spine/tools/mod_launcher.py \
        /opt/mc-spine/tools/vanilla_data/gen_vanilla_data.sh \
        /opt/mc-spine/tools/vanilla_data/version_pack.py

USER mc-spine
WORKDIR /data
EXPOSE 25565/tcp
ENTRYPOINT ["/opt/mc-spine/entrypoint.sh"]
