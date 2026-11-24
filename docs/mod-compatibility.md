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

## See also

- [mod-author-guide.md](mod-author-guide.md) — tiers, `megu-mod-check` flags,
  exit codes, Bundle authoring.
- [operator-guide.md](operator-guide.md) — modpack pre-flight on a server.
- [architecture-crates.md](architecture-crates.md) — where `mod-adapter-core`,
  `create-adapter`, `mod-offload` and `state-store` sit in the crate graph.
