# Crate architecture

The dependency map of the twelve standalone crates plus the root `megu`
package, the license model, and the fast-iteration commands. The
manifests are the source of truth — this page is the map, and every edge
below is read out of the `*/Cargo.toml` `[dependencies]` tables.
Eleven crates have no third-party dependencies. The twelfth, `client-gpu`,
isolates `winit`/`wgpu` and their external dependencies in its own
manifest and `client-gpu/Cargo.lock`.

## Packages

| path | package | what it owns |
|---|---|---|
| [../core](../core/Cargo.toml) | `megu-core` | canonical `1.21.1`/`26.3` version constants, validated `ResourceLocation`, Java-exact `LegacyRandom`/`Xoroshiro128++` RNG, vanilla `Mth` math |
| [../proto](../proto/Cargo.toml) | `megu-proto` | canonical protocol IR, versioned packet tables (767 and 777), the protocol-767 codec, the `VCR1` UDP proximity voice relay ([src/voice_relay.rs](../proto/src/voice_relay.rs)) |
| [../state-store](../state-store/Cargo.toml) | `megu-state-store` | 64-shard `DimensionState`/`ServerContext` runtime store, the `STV1` snapshot codec, the declarative `1.21.1 ↔ 26.3` DataFixer pipeline, the pure-`std` vanilla `level.dat`/`.mca` inspector with its own gzip + binary-NBT codecs ([src/anvil_inspect.rs](../state-store/src/anvil_inspect.rs)) |
| [../server-files](../server-files/Cargo.toml) | `megu-server-files` | Vanilla server files (`server.properties`, `eula.txt`, ops/whitelist/bans, `usercache.json`) with atomic persistence; `megu-init` directory bootstrap and `megu-admin` offline commands; OpenPAC/FTB-Chunks-style claims + parties + forceload engine ([src/claims.rs](../server-files/src/claims.rs)) and `CustomPacketPayload` sync codec ([src/claims_wire.rs](../server-files/src/claims_wire.rs)) |
| [../mod-offload](../mod-offload/Cargo.toml) | `mod-offload` | factory simulation kernels and server-authoritative combat/cast timelines, combo windows, four hitbox types, invulnerability frames and `MOT1` wire ([src/combat_motion.rs](../mod-offload/src/combat_motion.rs)); `rlib` + `cdylib` |
| [../mod-adapter-core](../mod-adapter-core/Cargo.toml) | `mod-adapter-core` | zero-JVM mod classification, declarative offload profiles, 4-tier fallback planner, Mixin-to-Event lifter ([src/mixin_lifter.rs](../mod-adapter-core/src/mixin_lifter.rs)), mana/spell casts/statuses/rituals and `MAG1` wire ([src/magic_primitives.rs](../mod-adapter-core/src/magic_primitives.rs)), declarative magic bundles ([src/unified_bundle.rs](../mod-adapter-core/src/unified_bundle.rs)), and automatic bundle/report generation ([src/auto_adapter.rs](../mod-adapter-core/src/auto_adapter.rs)) via `megu-mod-check --adapt-jar`, `--adapt-mods-dir`, `--scaffold-native` |
| [../create-adapter](../create-adapter/Cargo.toml) | `create-adapter` | flagship zero-JVM Create-mod special-case optimizer, 6-DOF `aeronautics` rigid-body sub-worlds and altitude-trigger (`Y >= 560`) space/planet dimension transitions with variable gravity ([src/orbital.rs](../create-adapter/src/orbital.rs)) |
| [../client-kernel](../client-kernel/Cargo.toml) | `megu-kernel` | protocol-767 kernel, versioned C ABI, 128-payload-section multi-word light masks ([src/light.rs](../client-kernel/src/light.rs)), and FK skeletal animation with Step/Linear/CatmullRom interpolation, Bedrock/GeckoLib JSON loading, crossfade layers and `ANM1` wire ([src/skeletal_anim.rs](../client-kernel/src/skeletal_anim.rs)) |
| [../client-port](../client-port/Cargo.toml) | `megu-client-mesh` | protocol-767 voxel mesher (`rlib` + `cdylib`), shaderpack processing ([src/shaderpack.rs](../client-port/src/shaderpack.rs)), shader binding reflection ([src/shaderpack_reflect.rs](../client-port/src/shaderpack_reflect.rs)), RGBA/scalar VFX curves, particles, beams/ribbons, auras, dynamic colored lights, GPU budget tiers and `VFX1` wire ([src/vfx_engine.rs](../client-port/src/vfx_engine.rs)), and `cull_region_bench` benchmark binary |
| [../client-ui](../client-ui/Cargo.toml) | `megu-client-ui` | independent pure-Rust client UI drawing/screens, including shaderpack selection ([src/shader_screen.rs](../client-ui/src/shader_screen.rs)) and mana/cast/cooldown/status HUD overlay ([src/magic_hud.rs](../client-ui/src/magic_hud.rs)) |
| [../client-app](../client-app/Cargo.toml) | `megu-client` | native client executable; `--shader-pack`/`--shader-profile` CLI ([src/cli.rs](../client-app/src/cli.rs)), shaderpack activation and GPU fallback ([src/shaderpack_runtime.rs](../client-app/src/shaderpack_runtime.rs)), RenderGraph integration, `ClientVfxHost` ([src/render.rs](../client-app/src/render.rs)), `WorldAnimationHost` ([src/world.rs](../client-app/src/world.rs)) and `recv_play_with_custom_channels` for `MAG1`/`VFX1`/`ANM1`/`MOT1` ([src/protocol.rs](../client-app/src/protocol.rs)) |
| [../client-gpu](../client-gpu/Cargo.toml) | `megu-client-gpu` | isolated windowed `winit` 0.30.13 + `wgpu` 30.0.1 GPU client, `pollster` and `bytemuck`; `--headless-probe`, real GPU window/surface and offscreen rendering ([src/main.rs](../client-gpu/src/main.rs), [src/renderer.rs](../client-gpu/src/renderer.rs)) |
| [../Cargo.toml](../Cargo.toml) | `megu` | the root server package ([../src/main.rs](../src/main.rs)) |

Six primary operator-facing binaries come out of the tree: `megu` (root),
`megu-init` (server-files), `megu-mod-check` (mod-adapter-core),
`megu-world-migrate` (state-store), `megu-client` (client-app)
and `megu-voice-relay` (proto). Additional executables include
`megu-admin` (server-files) and `cull_region_bench` (client-port).
Operator-facing usage is in
[docs/operator-guide.md](operator-guide.md).

Outside the crates, [../website](../website) is a zero-build static
landing page (`index.html` + `app.js` + `site-config.js`, no bundler, no
CDN) whose in-tab `.mrpack`/`.zip` dropzone mirrors the
`mod-adapter-core` pre-flight classifier entirely client-side;
[website/check_site.py](../website/check_site.py) verifies it offline.

## Dependency DAG

Edges point at what a crate depends on; the graph is acyclic.

```
megu-core            (no in-tree deps)
megu-proto           → megu-core                  [bin: megu-voice-relay]
megu-state-store     → megu-core                  [bin: megu-world-migrate]
megu-server-files    → megu-core                  [bins: megu-init, megu-admin]
mod-offload              (no in-tree deps)
mod-adapter-core         → megu-core, mod-offload     [bin: megu-mod-check]
create-adapter           → mod-adapter-core, mod-offload
megu-kernel          → megu-core
megu-client-mesh     (no in-tree deps)                [bin: cull_region_bench]
megu-client-ui       → megu-core
megu-client          → megu-kernel, megu-client-mesh, megu-proto
megu-client-gpu      → megu-client, megu-client-mesh,
                           megu-kernel, megu-client-ui   [bin: megu-client-gpu]
megu (root)          → mod-adapter-core?, create-adapter?   (optional deps)
```

The root optional edges are enabled by `create-offload`; the default
server build resolves those path dependencies without linking them.

## Standalone crates, not a workspace

There is deliberately no `[workspace]` table. Each crate is an
independent package — its own `Cargo.toml`, version and build directory —
held together by `path` dependencies. `cargo build` in a crate directory
builds that crate and its path deps without touching the root; `cargo
build` at the repository root builds the `megu` package only. Cargo
builds of the eleven dependency-free crates use their own lockfiles;
`client-gpu/Cargo.lock` pins the separate third-party GPU stack. Cargo
profiles are read from the package being built, so
`cargo build --profile fast` refers to the `fast` profile defined in the
root [../Cargo.toml](../Cargo.toml) (inherits `release`, `lto = false`,
`codegen-units = 16`, `incremental = true`).

The root server stays buildable with zero subcrate code linked:
`mod-adapter-core` and `create-adapter` are `optional = true` and only
join the binary through the `create-offload` feature
([../src/mod_offload_bridge.rs](../src/mod_offload_bridge.rs)).

The toolchain is pinned — `channel = "1.98.0"` in
[../rust-toolchain.toml](../rust-toolchain.toml).

## License model

| package | license |
|---|---|
| `megu` (root) | `LGPL-3.0-or-later` |
| all twelve subcrates, including `megu-client-gpu` | `LGPL-3.0-or-later` |
| `client-gpu` direct third-party crates (`wgpu`, `winit`, `pollster`, `bytemuck`) | `MIT OR Apache-2.0` |

Every manifest declares `LGPL-3.0-or-later` in its `[package]` table, and
source files carry an `SPDX-License-Identifier` header (LGPL-only, or the
earlier `LGPL OR GPL OR AGPL` grant). The texts ship in the tree:
[../LICENSE](../LICENSE) (LGPL-3.0, the distribution licence),
[COPYING.GPL](COPYING.GPL) (the GPL-3.0 companion text LGPLv3 asks for)
and [../NOTICE](../NOTICE). The reasoning and the third-party inventory
are [../LICENSING.md](../LICENSING.md) and
[../THIRD_PARTY.md](../THIRD_PARTY.md);
[../deny.toml](../deny.toml) gates dependency licenses, and
[tools/provenance_check.py](../tools/provenance_check.py) fail-closed
verifies that every listed manifest keeps a distributable license.

## Fast iteration

```sh
# What this checkout can run — no build, no downloads, seconds.
python3 tools/dev_check.py

# Optimized but quick-to-link iteration build of the server.
cargo build --profile fast

# Point all the standalone crates at one shared target dir (or isolate
# a run): they each default to their own ./target.
CARGO_TARGET_DIR=$HOME/.cache/megu-target cargo build --profile fast

# Map changed paths to the harnesses that cover them before running
# the whole suite (also: --git-diff REF, --changed-files FILE).
python3 tools/parity_cache.py map --paths src/worldgen.rs core/src/lib.rs
```

The full gates are `bash tests/verify.sh`, `bash tools/layers_check.sh`,
`bash tools/debt_ratchet.sh` and `python3 tools/provenance_check.py` —
[tools/README.md](../tools/README.md) indexes the rest of the tooling,
and [docs/development.md](development.md) walks contributor setup and the
test tiers. Release packaging (`tools/release_bundle.py build|verify`,
deterministic tarball + manifest + SHA256SUMS) follows
[docs/release-checklist.md](release-checklist.md).
