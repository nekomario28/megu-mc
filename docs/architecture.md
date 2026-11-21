# Architecture

`mc-spine` is the canonical server package and executable. Its standalone
crates and dependency edges are indexed in
[architecture-crates.md](architecture-crates.md). The capabilities below
refer to modules present in this tree.

## Crate capabilities

- **`mod-adapter-core`** — Zero-JVM JAR and bytecode Mixin inspection
  ([`class_mixin_scanner`](../mod-adapter-core/src/class_mixin_scanner.rs)),
  including safe `@Inject` Tier 2 event lifting; generic Farmer's Delight
  recipes and compatible addon discovery
  ([`culinary_adapter`](../mod-adapter-core/src/culinary_adapter.rs));
  progression barriers, multi-part bosses, `CGD1` chunk-generation and
  `EAD1` entity-AI delegates, AppleSkin saturation and Inventory Sorter
  quality-of-life support, and TerraBlender-style region blending
  ([`world_extensions`](../mod-adapter-core/src/world_extensions.rs));
  validated Server Mod Sync ([`mod_sync`](../mod-adapter-core/src/mod_sync.rs))
  and generic creature behavior primitives
  ([`creature_primitives`](../mod-adapter-core/src/creature_primitives.rs)).
- **`client-port`** — Vulkan mesh shaders, compute-driven multi-draw
  indirect, and `ClassicLowSpec` for legacy Intel HD/UHD devices with
  16 KiB uniform-buffer batches. The GPU pipeline includes Create-style
  kinetic instances and block-entity instancing for chests, item frames,
  and signs ([`gpu_pipeline`](../client-port/src/gpu_pipeline.rs)).
- **`proto`** — `FRL1` friend invite relay
  ([`friend_relay`](../proto/src/friend_relay.rs)) and `VCR1` proximity
  voice relay ([`voice_relay`](../proto/src/voice_relay.rs)).
  `ServerSession` and the version router provide protocol 767/777
  session/codec paths ([`session`](../proto/src/session.rs),
  [`router`](../proto/src/router.rs)).
- **`state-store`** — `AUD1` audit write-ahead log and rollback
  ([`audit_log`](../state-store/src/audit_log.rs)); spiral chunk
  pregeneration and ticket-aware LRU unload
  ([`pregen`](../state-store/src/pregen.rs)); Anvil region and `level.dat`
  inspection/migration ([`anvil_inspect`](../state-store/src/anvil_inspect.rs)).
- **`server-files`** — `SharedChunkWireCache` with `Arc<[u8]>` packet
  storage, `DirectionalChunkQueue`, and the `SBC1` startup binary cache
  ([`perf_cache`](../server-files/src/perf_cache.rs)).

## Clean-room public export

[`export_public_branch.sh`](../tools/export_public_branch.sh) strips
generated Minecraft data from the public tree and can run strict
provenance checks (`--verify`). Production code still embeds generated
block-state facts via `include_bytes!` in
[`client-kernel/src/registry.rs`](../client-kernel/src/registry.rs).
The stripped tree must regenerate those inputs from an operator-supplied
official server JAR before a release build. The export and build
procedure is in [release-checklist.md](release-checklist.md).
