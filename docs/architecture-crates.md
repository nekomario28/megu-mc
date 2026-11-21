# Crate architecture

The dependency map of the ten standalone crates plus the root `mc-spine`
package, the license model, and the fast-iteration commands. The
manifests are the source of truth — this page is the map, and every edge
below is read out of the `*/Cargo.toml` `[dependencies]` tables.

## Packages

| path | package | what it owns |
|---|---|---|
| [../core](../core/Cargo.toml) | `mc-spine-core` | canonical `1.21.1`/`26.3` version constants, validated `ResourceLocation`, Java-exact `LegacyRandom`/`Xoroshiro128++` RNG, vanilla `Mth` math |
| [../proto](../proto/Cargo.toml) | `mc-spine-proto` | canonical protocol IR, versioned packet tables (767 and 777), the protocol-767 codec, the `VCR1` UDP proximity voice relay ([src/voice_relay.rs](../proto/src/voice_relay.rs)) |
| [../state-store](../state-store/Cargo.toml) | `mc-spine-state-store` | 64-shard `DimensionState`/`ServerContext` runtime store, the `STV1` snapshot codec, the declarative `1.21.1 ↔ 26.3` DataFixer pipeline, the pure-`std` vanilla `level.dat`/`.mca` inspector with its own gzip + binary-NBT codecs ([src/anvil_inspect.rs](../state-store/src/anvil_inspect.rs)) |
| [../server-files](../server-files/Cargo.toml) | `mc-spine-server-files` | Vanilla server files (`server.properties`, `eula.txt`, ops/whitelist/bans, `usercache.json`) with atomic persistence, the OpenPAC/FTB-Chunks-style claims + parties + forceload engine ([src/claims.rs](../server-files/src/claims.rs)) and its `CustomPacketPayload` wire sync codec ([src/claims_wire.rs](../server-files/src/claims_wire.rs)) |
| [../mod-offload](../mod-offload/Cargo.toml) | `mod-offload` | factory simulation kernel prototypes (`rlib` + `cdylib`) |
| [../mod-adapter-core](../mod-adapter-core/Cargo.toml) | `mod-adapter-core` | universal zero-JVM mod adapter, declarative offload profile engine, 4-tier fallback planner on `mod-offload`, the Mixin-to-Event static lifter that promotes a fully liftable mixin set from Tier 3 to Tier 2 ([src/mixin_lifter.rs](../mod-adapter-core/src/mixin_lifter.rs)) |
| [../create-adapter](../create-adapter/Cargo.toml) | `create-adapter` | flagship zero-JVM Create-mod special-case optimizer, 6-DOF `aeronautics` rigid-body sub-worlds and altitude-trigger (`Y >= 560`) space/planet dimension transitions with variable gravity ([src/orbital.rs](../create-adapter/src/orbital.rs)) |
| [../client-kernel](../client-kernel/Cargo.toml) | `mc-spine-kernel` | protocol-767 kernel and versioned C ABI |
| [../client-port](../client-port/Cargo.toml) | `mc-spine-client-mesh` | protocol-767 voxel section mesher (`rlib` + `cdylib`) |
| [../client-app](../client-app/Cargo.toml) | `mc-spine-client` | native client executable |
| [../Cargo.toml](../Cargo.toml) | `mc-spine` | the root server package ([../src/main.rs](../src/main.rs)) |

Six binaries come out of the tree: `mc-spine` (root),
`mc-spine-init` (server-files), `mc-spine-mod-check` (mod-adapter-core),
`mc-spine-world-migrate` (state-store), `mc-spine-client` (client-app)
and `mc-spine-voice-relay` (proto). Operator-facing usage is in
[docs/operator-guide.md](operator-guide.md).

Outside the crates, [../website](../website) is a zero-build static
landing page (`index.html` + `app.js` + `site-config.js`, no bundler, no
CDN) whose in-tab `.mrpack`/`.zip` dropzone mirrors the
`mod-adapter-core` pre-flight classifier entirely client-side;
[website/check_site.py](../website/check_site.py) verifies it offline.

## Dependency DAG

Edges point at what a crate depends on; the graph is acyclic.

```
                       ┌─→ mc-spine-proto ──────────────┐
                       │                                │
 mc-spine-core ────────┼─→ mc-spine-state-store          │    mc-spine-client
   (leaf)              ├─→ mc-spine-server-files         │      → mc-spine-kernel
                       ├─→ mc-spine-kernel ──┐           │      → mc-spine-client-mesh
                       └─→ mod-adapter-core  │           │      → mc-spine-proto
                              │    ▲         │           │
                              ▼    │         ▼           ▼
 mod-offload ◄─────────────────────┤    mc-spine-client-mesh
   (leaf)                          │       (leaf)
                                   │
 create-adapter ──→ mod-adapter-core, mod-offload

 mc-spine (root) ── optional ──→ mod-adapter-core, create-adapter
                  (feature `create-offload`; default builds resolve
                   the path deps but link neither)
```

Flattened, as the manifests declare it:

```
mc-spine-core            (no in-tree deps)
mc-spine-proto           → mc-spine-core                  [bin: mc-spine-voice-relay]
mc-spine-state-store     → mc-spine-core                  [bin: mc-spine-world-migrate]
mc-spine-server-files    → mc-spine-core                  [bin: mc-spine-init]
mod-offload              (no in-tree deps)
mod-adapter-core         → mc-spine-core, mod-offload     [bin: mc-spine-mod-check]
create-adapter           → mod-adapter-core, mod-offload
mc-spine-kernel          → mc-spine-core
mc-spine-client-mesh     (no in-tree deps)
mc-spine-client          → mc-spine-kernel, mc-spine-client-mesh, mc-spine-proto
mc-spine (root)          → mod-adapter-core?, create-adapter?   (optional deps)
```

## Standalone crates, not a workspace

There is deliberately no `[workspace]` table. Each crate is an
independent package — its own `Cargo.toml`, version and build directory —
held together by `path` dependencies. `cargo build` in a crate directory
builds that crate and its path deps without touching the root; `cargo
build` at the repository root builds the `mc-spine` package only. Cargo
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
| `mc-spine` (root) | `GPL-3.0-or-later` |
| all ten subcrates | `LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later` |

Every subcrate manifest declares the triple license in its `[package]`
table and every source file carries the matching SPDX header. The texts
ship in the tree: [../LICENSE](../LICENSE),
[../COPYING.LESSER](../COPYING.LESSER), [../COPYING.AGPL](../COPYING.AGPL)
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
CARGO_TARGET_DIR=$HOME/.cache/mc-spine-target cargo build --profile fast

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
