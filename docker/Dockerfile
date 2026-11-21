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

# Everything `cargo build --release --locked` resolves, and nothing more: the
# root manifest + lockfile, src/, the manifest+sources of the optional path
# dependencies (Cargo resolves them even with every feature off), and the
# four generated tables embedded via include_bytes!.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY mod-adapter-core/Cargo.toml mod-adapter-core/Cargo.toml
COPY mod-adapter-core/src mod-adapter-core/src
COPY mod-offload/Cargo.toml mod-offload/Cargo.toml
COPY mod-offload/src mod-offload/src
COPY create-adapter/Cargo.toml create-adapter/Cargo.toml
COPY create-adapter/src create-adapter/src
COPY assets/feature_order_1211.json assets/feature_order_1211.json
COPY assets/structure_state_transforms_1211.bin assets/structure_state_transforms_1211.bin
COPY datapacks/vanilla/data/minecraft/dimension_type/overworld.json \
     datapacks/vanilla/data/minecraft/dimension_type/the_nether.json \
     datapacks/vanilla/data/minecraft/dimension_type/

RUN cargo build --release --locked

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

# The same payload the release bundle ships: licence texts plus the tools a
# public user needs (modpack pre-flight, vanilla-data regeneration).
COPY LICENSE COPYING.LESSER COPYING.AGPL NOTICE LICENSING.md THIRD_PARTY.md /opt/mc-spine/
COPY tools/mod_launcher.py /opt/mc-spine/tools/mod_launcher.py
COPY tools/vanilla_data/gen_vanilla_data.sh tools/vanilla_data/version_pack.py /opt/mc-spine/tools/vanilla_data/
COPY docker/entrypoint.sh /opt/mc-spine/entrypoint.sh
RUN chmod 0755 /usr/local/bin/mc-spine /opt/mc-spine/entrypoint.sh \
        /opt/mc-spine/tools/mod_launcher.py \
        /opt/mc-spine/tools/vanilla_data/gen_vanilla_data.sh \
        /opt/mc-spine/tools/vanilla_data/version_pack.py

USER mc-spine
WORKDIR /data
EXPOSE 25565/tcp
ENTRYPOINT ["/opt/mc-spine/entrypoint.sh"]
