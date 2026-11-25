# Mod compatibility

How existing Minecraft 1.21.1 mods and modpacks are handled without starting a
JVM: what is read out of the JARs, how the loaders' tag namespaces line up, what
a server may push to a client, and what happens to a world when a mod goes
missing.

Compatibility is **pre-flight classification plus per-mod adaptation**, not a
promise that any given mod runs. The tier model (`Native` / `Data+Bundle` /
`Lifted Shim` / `Fallback`) and the `megu-mod-check` flags live in
[mod-author-guide.md](mod-author-guide.md); this page covers the mechanics
underneath them.

## Compatibility tier taxonomy (L0–L5)

The six rungs below are the **capability ladder** this project classifies,
reports and reasons about; they sit underneath the per-mod verdicts that
`megu-mod-check` emits. Every subsystem in this tree occupies exactly one
rung **today** — the status column is the honest current state, not a
roadmap promise.

| Tier | Name | What it means | Status today |
|---|---|---|---|
| `L0` | Metadata Discovery | Reading a mod without executing anything: loader manifests, dependency and version ranges, Jar-in-Jar nesting, Mixin site inventory, modpack files (`.mrpack` / CurseForge manifest). | **Shipped.** [`class_mixin_scanner`](../mod-adapter-core/src/class_mixin_scanner.rs), [`loader_extensions`](../mod-adapter-core/src/loader_extensions.rs), [`dep_resolver`](../mod-adapter-core/src/dep_resolver.rs) and [`modpack`](../mod-adapter-core/src/modpack.rs) parse JAR/modpack metadata with fail-closed bounds (§1). |
| `L1` | Data / Asset / Recipe / Tag Parity | Declarative documents a mod ships are read and applied natively: recipes, tags, loot modifiers, dimensions, worldgen JSON on the server; blockstates, models, textures, recipe docs on the client. | **Shipped for standard data forms.** [`unified_tags`](../mod-adapter-core/src/unified_tags.rs) (§2), [`data_loader`](../mod-adapter-core/src/data_loader.rs), [`recipe_conflict_unify`](../mod-adapter-core/src/recipe_conflict_unify.rs), and the client-side [`recipe_ingest`](../client-gpu/src/recipe_ingest.rs) / [`vanilla_jar`](../client-gpu/src/vanilla_jar.rs) asset layers. Unknown data documents are preserved (§5) but not interpreted. |
| `L2` | Native Rust Adapter Parity | A dedicated Rust adapter reproduces a mod's runtime behaviour without any JVM — the flagship is Create (kinetics, contraptions, addon profiles, Aeronautics sub-worlds). | **Shipped for the Create family and authored adapters.** [`create-adapter`](../create-adapter/src/lib.rs) + [`addon_profile`](../create-adapter/src/addon_profile.rs), declarative `megu-mod-bundle/1` bundles, and the culinary/Farmer's-Delight-style adapters. Arbitrary mods need an authored adapter or bundle; nothing is automatic. |
| `L3` | Java/NeoForge Event & Capability API Parity | The event and capability surface a JVM mod codes against is re-implemented in Rust: item/fluid/energy transfer capabilities, NeoForge event batches, lifting supported `@Inject` sites to native events. | **Partial.** [`capability_bus`](../mod-adapter-core/src/capability_bus.rs) (§3) models the transfer APIs end to end; [`neoforge_wire`](../mod-adapter-core/src/neoforge_wire.rs) and [`mixin_lifter`](../mod-adapter-core/src/mixin_lifter.rs) lift the supported injection subset. Everything else classifies down — never emulated. |
| `L4` | Bytecode / Mixin Runtime Compatibility | Executing mod bytecode itself: clean-room `.class` parsing, an allowlisted subset VM, and a quarantined WASM/native mod runtime with rollback. | **Partial by design.** [`class_mixin_scanner`](../mod-adapter-core/src/class_mixin_scanner.rs) parses JVMS §4, [`bytecode_subset_vm`](../mod-adapter-core/src/bytecode_subset_vm.rs) executes only the statically verified allowlist with a fuel cap, [`mod_isolation`](../mod-adapter-core/src/mod_isolation.rs) quarantines faults per mod. `@Redirect` / `@Overwrite`, reflection, access transformers and arbitrary bytecode are hard rejections. |
| `L5` | Multi-Modpack Composition & Save Round-Trip | Many mods and whole modpacks at once, and a world that safely survives adding / removing / updating them: namespace tombstones, verbatim NBT preservation, verified snapshots before mutation, hashed mod sync, per-server isolation. | **Shipped for preservation** (§4, §5); full behavioural composition of an arbitrary modpack is not promised — each mod still has to satisfy its own tier. |

Tier mapping to `megu-mod-check`: `L0`–`L1` are the input analysis behind
Tier 0/1 verdicts, `L2` is what a Tier 0 adapter or Tier 1 bundle runs on,
`L3` is the Tier 2 lifted-shim surface, `L4` is the Tier 2 ↔ Tier 3
boundary (what the subset VM may execute vs. what must be delegated), and
`L5` is the cross-tier guarantee every path above leans on.

## Fail-closed unsupported-mod contract

When a mod needs something this tree does not implement — an un-liftable
Mixin, `@Redirect` / `@Overwrite`, an access transformer, an unknown
capability type, arbitrary bytecode — Megu **refuses or quarantines; it
never silently skips.**

Why skipping is worse than refusing: the world format is shared with real
NeoForge/Fabric servers. If Megu pretended an unsupported mod loaded while
not running its hooks, every write to that mod's blocks, entities and
attachments would silently drop or mis-handle state — items would behave
wrong, block entities would save garbage, and a world handed back to a real
server would be inconsistent. A crash is recoverable; a world that is
silently corrupt but looks fine is not. So the contract is:

1. **Classify first.** Pre-flight reports the exact reason a mod cannot run
   (`megu-mod-check` exit `2` = `tier3_fallback_required`), never a vague
   failure.
2. **Refuse to start without it.** The server / modpack pre-flight fails
   with the reason instead of booting with the mod missing.
3. **No partial execution.** A mod either runs inside a tier it fully
   satisfies, or its whole activation is blocked — no half-applied Mixin
   sets.
4. **Quarantine at runtime.** A running mod that traps, exhausts its WASM
   fuel, exceeds its memory cap, makes a disallowed host call, or misses
   its tick deadline is quarantined alone
   ([`ModQuarantineSupervisor`](../mod-adapter-core/src/mod_isolation.rs))
   while the world and every healthy mod keep running, with a
   content-addressed rollback snapshot
   ([`WorldTimeMachine`](../mod-adapter-core/src/mod_isolation.rs)).
5. **Mutations stay reversible.** The world's bytes are never touched by an
   un-verified mod: preservation primitives (§5) plus verified snapshots
   before every mutation make each change undoable.

The trade-off is explicit: fewer mods run than a loader that skips and
hopes, but none of them can corrupt a world.

## 1. Recursive Jar-in-Jar

Mods that embed other mods (Create addons, libraries, loader shims) declare
them in `META-INF/jarjar/metadata.json` (NeoForge) or ship them under
`META-INF/jars/*.jar` (Fabric). Both loaders nest, and a wrapper JAR can carry
no assets of its own at all.

**Server side** — [`inspect_jar_extensions`](../mod-adapter-core/src/loader_extensions.rs)
parses the metadata and recursively walks the declared nested JARs. The
`JarExtensionReport::jar_in_jar` (`JarInJarScan`) reports every declared entry
in declaration order, how many nested members were opened and verified, their
decompressed byte total, and the deepest level reached. Every bound fails
closed:

| bound | value |
|---|---|
| `MAX_JAR_IN_JAR_DEPTH` | 3 levels of metadata |
| `MAX_JAR_IN_JAR_NESTED_JARS` | 64 declared entries per outer archive |
| `MAX_JAR_IN_JAR_TOTAL_BYTES` | 64 MiB aggregate decompressed |

Path traversal and non-`.jar` member names are rejected outright. An archive
that declares nested JARs sets `JarExtensionReport::requires_jvm_bridge()`,
which is what pushes it out of the pure-native tier — embedded code is
**reported and bounded, never executed**.

**Client side** — [`client-gpu/src/vanilla_jar.rs`](../client-gpu/src/vanilla_jar.rs)
walks `mods/*.jar` and follows `META-INF/jarjar/*.jar` / `META-INF/jars/*.jar`
recursively to `MAX_JIJ_DEPTH` (2), capped at `MAX_JIJ_JARS_PER_MOD` (64) and
`MAX_JIJ_JAR_BYTES` (64 MiB) per nested payload. Each nested JAR emits its own
resource layers in sorted entry-name order, so a bundled addon contributes every
nested namespace's blockstates, models, textures and animations. Later JARs
override earlier ones and user resource packs override every mod layer.
[`client-gpu/src/recipe_ingest.rs`](../client-gpu/src/recipe_ingest.rs) reads
the same nesting for recipe/data documents **without extracting a single byte to
disk** — the client ingests data, it does not unpack mods.

**Create-family addons.** Built-in profiles ship for `createaeronautics`,
`createaddition`, `create_new_age` and `railways`
([`create-adapter/src/addon_profile.rs`](../create-adapter/src/addon_profile.rs)),
and `discover_create_addon_profile` classifies arbitrary Create addon JARs
without a JVM. Aeronautics sub-worlds are simulated as 6-DOF rigid bodies
([`create-adapter/src/aeronautics.rs`](../create-adapter/src/aeronautics.rs))
and wired into the live server by
[`src/aeronautics_and_dim_bridge.rs`](../src/aeronautics_and_dim_bridge.rs):
each attached passenger receives its exact carried-motion delta (no rubber
banding, no NoFall false positives), and multi-airship state round-trips through
a deterministic Save v2 blob.

## 2. Cross-mod tag interoperability

The same concept has three names in 1.21.1: `#c:ingots` (conventional),
`#forge:ingots` (legacy Forge) and `#neoforge:ingots`.

[`UnifiedTagIndex`](../mod-adapter-core/src/unified_tags.rs) merges every loaded
mod JAR (including bounded Jar-in-Jar inner JARs), every datapack and the
Vanilla 1.21.1 baseline into one index:

- **Three domains** — item, block and fluid tags stay separate, so `c:ores` the
  block tag never aliases `c:ores` the item tag.
- **Bidirectional aliasing** between `#c:*` and legacy `#forge:*` / `#neoforge:*`
  spellings, canonicalizing at both merge and query time, for **arbitrary
  paths** — not only the ten conventional families. A mod that only registered
  `forge:plates/brass` still answers `#c:plates/brass` queries and vice versa.
- **Recursive nested tags** (`#c:ingots` containing `#c:ingots/iron`) with
  deterministic sorted resolution, a `MAX_UNIFIED_TAG_DEPTH` of 16 and
  fail-closed cycle detection: a cyclic graph errors instead of looping, and
  `matches` reports `false`.
- **Spec-faithful entries** — `"replace": true` and
  `{"id": ..., "required": false}`, shared with
  [`loader_extensions::parse_tag_file_json`](../mod-adapter-core/src/loader_extensions.rs),
  plus the legacy `data/<ns>/tags/fluids/*.json` layout.
- Scan budget: `MAX_UNIFIED_TAG_SCAN_BYTES` (256 MiB decompressed) per
  `ingest_jar_bytes` call.

`ConventionalTagRegistry` keeps the ten `c:` families seeded with the vanilla
baseline; the unified index exports a flattened
[`culinary_adapter::TagIndex`](../mod-adapter-core/src/culinary_adapter.rs) so
older APIs keep working while gaining nested expansion and aliasing.

## 3. Cross-mod block capabilities

[`BlockCapabilityBus`](../mod-adapter-core/src/capability_bus.rs) is a pure-Rust
model of NeoForge 1.21.1 `BlockCapabilities` / the Fabric Transfer API, so any
loaded mod — and vanilla hoppers, chests and furnaces — can exchange items,
fluids and energy through one shared registry **with zero hardcoded mod-to-mod
special cases**.

- **Dimension-generic coordinates** — `BlockCoord` carries the canonical
  dimension id, so endpoints in different dimensions can never alias.
- **Lossless item stacks** — `CapabilityItemStack` keeps item id, count and
  opaque `raw_components` (1.21.1 data-component bytes) byte-for-byte across
  every hop; stacks merge only when id **and** components match.
- **Faithful handler contracts** — `ItemTransferHandler`, `FluidTransferHandler`
  and `EnergyTransferHandler` mirror `IItemHandler` / `IFluidHandler` /
  `IEnergyStorage`, including `simulate: bool` probes that never mutate state.
- **Atomic simulate-then-commit** — `transfer_item_between`,
  `transfer_fluid_between` and `transfer_energy_between` cannot duplicate or
  vanish cargo when the destination is full or rejects a slot.
- **Ready-made endpoints** — `SimpleSlotInventory`, `SimpleFluidTank`,
  `SimpleEnergyBuffer`, and `CookingPotItemAdapter` (top/side inserts inputs,
  bottom extracts the finished meal) as living cross-mod proof.

## 4. Server → client mod sync

Two independent paths, both hash-verified and consent-gated.

**Protocol path** — a server advertises the exact mods a client needs on the
Protocol 767 channel `megu:mod_sync_v1` as an `MSY1` frame
([`mod-adapter-core/src/mod_sync.rs`](../mod-adapter-core/src/mod_sync.rs)):

- strict origin allowlist: `https://` on exactly `cdn.modrinth.com`,
  `edge.forgecdn.net` or `mediafilez.forgecdn.net`, or an operator-pinned
  mirror. Scheme tricks, credentials in the URL, raw IP literals, `localhost`,
  non-443 ports, control characters and non-ASCII hosts are hard errors;
- every entry carries a 64-hex SHA-256 digest and an exact size, checked before
  the write, with the installed file re-hashed after the atomic rename
  (`MAX_SYNC_MODS` 512, `MAX_MOD_JAR_BYTES` 256 MiB);
- a `.jar` filename jail (plain ASCII basename, no separators, no `..`, no
  hidden files, no shell metacharacters, no pre-existing symlink);
- nothing is written without a `ConsentToken` with `user_approved == true`;
- ids in `fallback::BUILTIN_SUPERSEDED_PERF_MODS` (Sodium, Embeddium, Lithium,
  Starlight, FerriteCore, ModernFix, Distant Horizons, Iris/Oculus, …) are
  skipped automatically — Megu's own implementation serves them.

Accepted bytes land in a `ContentAddressedModCache` keyed by SHA-256
(`<root>/<sha256>.jar`), stored read-only and published with an exclusive temp
write plus `hard_link`; the profile's `mods/` directory receives a hardlink
(atomic copy across filesystems), and removal is a consent-gated `prune_removed`
on that profile only.

**Client auto-sync path** — [`client-gpu/src/mod_sync_auto.rs`](../client-gpu/src/mod_sync_auto.rs)
handles the "server requires mods you do not have" case end to end:

1. the host's disconnect reason names the required mods, versions and digests;
   `compute_missing_mods` turns that into a `Missing` / `VersionMismatch` list;
2. the client shows one **Mod mismatch** screen listing mod id, required and
   installed versions, then the **Server Mod Sync** consent screen with a short
   SHA-256 per added / updated / removed file — nothing installs until Accept;
3. downloads resolve through `https://api.modrinth.com/v2` and come only from
   `cdn.modrinth.com` (`host_allowed` is an exact-host match; a non-443 port,
   `user@host`, or any other host is rejected). Each file is verified against
   its Modrinth **SHA-512** digest, and with SHA-256 as well when supplied
   (`MAX_MODS` 64, `MAX_MOD_BYTES` 128 MiB, `MAX_BATCH_BYTES` 512 MiB);
4. files land in a **per-server isolated directory**,
   `<gameDir>/server-mods/<sha256(server_key)[..16]>/`, so two servers that need
   different versions of the same mod never collide and a player who never joins
   a server downloads nothing from it. The job runs on a worker with progress
   and cancellation.

## 5. Modded saves: preservation instead of silent deletion

Vanilla deletes registry entries it no longer knows on load, so removing a mod
is normally one-way data loss. Megu keeps the bytes:

- **Unknown NBT is preserved verbatim.** The Anvil codec
  ([`src/anvil_io.rs`](../src/anvil_io.rs)) imports and exports both first-class
  targets — 1.21.1 (`DataVersion` 3955, the native write format) and 26.3
  (`DataVersion` 5023). Per chunk, a palette cell whose block the registry
  cannot interpret is kept as an exact NBT tombstone and merged back on
  re-encode; unknown root members (`Heightmaps`, `structures`,
  `neoforge:attachments`, `ForgeData`, `ForgeCaps`, `fabric:attachments`,
  `cardinal_components`, …) pass straight back through.
- **Custom dimensions round-trip.** `dimensions/<namespace>/<path>/region/*.mca`
  is recognised on import and written back to the same layout, with a depth
  bound and per-segment validation (`src/anvil_io.rs`).
- **Cross-loader discovery is read-only.**
  [`state-store/src/world_interop.rs`](../state-store/src/world_interop.rs)
  never rewrites region files; unknown NBT members retain their original binary
  tag representation even where the `DataValue` codec projects numeric widths
  and array types, and `roundtrip_playerdata_preserving_attachments` /
  `roundtrip_level_dat_preserving_attachments` re-encode `playerdata` and
  `level.dat` while carrying preserved members through, then re-decode to prove
  the document did not change. `PRESERVED_MOD_ATTACHMENT_KEYS` covers
  `neoforge:attachments`, `fabric:attachments`, `ForgeCaps`, `ForgeData`, `fml`,
  `cardinal_components`, `DataPacks`, `WorldGenSettings`, `CustomBossEvents` and
  `DragonFight`.
- **Unknown registry objects become tombstones, not deletions.**
  `NamespaceTombstoneSidecar`
  ([`state-store/src/namespace_guard.rs`](../state-store/src/namespace_guard.rs))
  parks each unknown `modid:block|item|entity` entry with its full original
  payload — block-state properties, block-entity NBT, item components, entity
  NBT — keyed by location, and substitutes a safe vanilla placeholder so the
  chunk loads. `restore_for_reloaded_namespaces` hands every entry back untouched
  when the mod returns. Entry/size caps fail closed rather than dropping data.
- **A verified snapshot precedes any mutation.** `prepare_cross_loader_handoff`
  (and the Anvil import path, under `mod-suite`) takes a content-addressed
  `WorldSnapshotStore` backup and re-verifies every blob before the first
  placeholder substitution, so a sidecar bug cannot lose the world.

## 6. Known limits

- Pre-flight exit code `0` means the JARs were classified, **not** that the
  modpack runs. Unsupported Mixins, `@Redirect`/`@Overwrite` and access
  transformers are reported as Tier 3, not emulated.
- Embedded JARs are reported and bounded; they are not executed natively.
- Tag aliasing covers `c:` ↔ `forge:` / `neoforge:`; an arbitrary third-party
  namespace is only resolved when a datapack declares the alias.
- The capability bus models item/fluid/energy transfer. Mods that depend on
  other Forge capability types need those reached through data or a dedicated
  adapter.

## Build features (root `Cargo.toml`)

The root `megu` package gates mod interop behind opt-in Cargo features; the
default build keeps the vanilla-only dependency graph. Every feature is
listed here because `tools/docs_sync_audit.py --strict` proves this section
never goes stale: each `[features]` entry in the root
[`Cargo.toml`](../Cargo.toml) must appear on this page.

| feature | enables | pulls in |
|---|---|---|
| `native-oracle` | CI-only Java fixtures: the pinned clean-room probes that run the 1.21.1 jar (`tools/structure-oracle/`, the `p1-native-oracle` workflow). Tests hard-fail without the native files. | nothing extra |
| `java-compat` | JNI bridge to one pinned JVM owner ([`src/java_compat.rs`](../src/java_compat.rs)) for the guarded Fabric/NeoForge probes (`src/landing.rs`). | optional `jni` dependency |
| `fabric-probe` | Fabric-flavoured Java-side probe set used by compat validation (`cargo check --locked --features fabric-probe` in CI). | `java-compat` |
| `neoforge-events` | NeoForge event-surface batches (S3/S4 payload codecs in [`src/play_protocol.rs`](../src/play_protocol.rs) and [`src/java_compat.rs`](../src/java_compat.rs)) that back the Tier-2 lift validation. | `java-compat` |
| `create-offload` | Production bridge to the mod adapters ([`src/mod_offload_bridge.rs`](../src/mod_offload_bridge.rs), `include!`d behind this cfg). | optional path deps `mod-adapter-core`, `create-adapter` |
| `mod-suite` | Full live root integration ([`src/live_mod_bridge.rs`](../src/live_mod_bridge.rs), every symbol inside `#[cfg]`-gated). | all four optional path deps: `mod-adapter-core`, `create-adapter`, `mod-offload`, `megu-state-store` |

`java-compat`, `fabric-probe`, `neoforge-events` and `native-oracle` are
opt-in Java-side paths; production servers ship without a JVM unless the
operator enables one of them.

## See also

- [mod-author-guide.md](mod-author-guide.md) — tiers, `megu-mod-check` flags,
  exit codes, Bundle authoring.
- [operator-guide.md](operator-guide.md) — modpack pre-flight on a server.
- [architecture-crates.md](architecture-crates.md) — where `mod-adapter-core`,
  `create-adapter`, `mod-offload` and `state-store` sit in the crate graph.
