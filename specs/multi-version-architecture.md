# Multi-Version Architecture Blueprint

**Status:** proposal — Lane N1a (design + tooling foundation only; no `src/` changes in this lane).
**Scope:** two first-class Minecraft Java targets, aligned end to end:

- **`1.21.1`** — the native/mod line: protocol `767`, `DataVersion 3955`,
  data `pack_format (48, 0)`, Java 21 LTS for mod runtimes (NeoForge/Fabric).
- **`26.3` (latest vanilla)** — the newest release line: protocol `777`,
  `DataVersion 5023`, data `pack_format (121, 0)` / resource `(97, 1)`
  (`version.json` spells these as flat `pack_version.{data,resource}_{major,minor}`
  ints and `pack.mcmeta` as `min_format`/`max_format` `[major, minor]` tuples),
  Java 25.

Intermediate snapshots/releases stay *compatible-but-not-primary*: they ride the same
VersionPack + IdRemapper machinery, but tests, gates and docs pin exactly the two
targets above. Everything is designed so a third target is a data pack, not a code fork.

**Reference implementation:** the architecture borrows patterns from the MIT-licensed
Horizon rewrite (`megu-local/reference/horizon-survey/`): a single pinned version
module (`horizon-core`'s `version.rs` — here, `mod-adapter-core`'s
`McVersion`/`version_compat` tables and the pack manifests), a name→id table plus
resolver split as in `horizon-protocol`'s `ids.rs`/`resolver.rs` (here:
`packet_ids.json`/`registry_ids.json` + `IdRemapper`), a versioned-storage datafix
pipeline as in `horizon-world`'s `datafix/` (here: `DataVersion` classification +
migration in `src/anvil_io.rs`), the dedicated-server file contract of
`horizon-server-files`, and Horizon's `[profile.fast]` quick-link build profile for
iteration.

**Companion tool:** [`tools/vanilla_data/version_pack.py`](../tools/vanilla_data/version_pack.py) —
builds, verifies and diffs the VersionPack data contract specified in §2.

All file/line claims below were measured on the base commit of this lane and are cited so they
can be re-checked after later syncs ([ROADMAP](ROADMAP.md) records the live head).

---

## 0. Where the version lives today (measured facts)

| Concern | Current location | Why it blocks a second version |
|---|---|---|
| Protocol number | Literal `767` in the handshake gate `if protocol != 767` and the status/login JSON ([`src/play_protocol.rs`](../src/play_protocol.rs) ~L4139/L4154/L8346; [`src/main.rs`](../src/main.rs) ~L14072) and `PROTOCOL_767` ([`client-app/src/protocol.rs`](../client-app/src/protocol.rs) L10) | A second protocol means forking every literal by hand. |
| Packet ids | Literal hex on send/recv: `0x2b` Join Game, `0x47` Respawn, `0x27` chunk data, `0x74` Update Advancements, `0x07` registry data, `0x12`/`0x19` custom payload (all in [`src/play_protocol.rs`](../src/play_protocol.rs)) | Vanilla renumbers packets between releases; a name→id table is data, not code. |
| Configuration-phase registries | [`config_packets.bin`](../config_packets.bin): a verbatim capture of one vanilla 1.21.1 server's configuration packets, replayed by `load_config_packets()` and parsed as `0x07` registry packets (`captured_config_packets`, `merge_registry_overlays`, `parse_dim_type_wire_ids`) | Opaque, version-pinned, requires booting a vanilla server under EULA to regenerate ([`tools/vanilla_data/capture_config_packets.py`](../tools/vanilla_data/capture_config_packets.py)). |
| Entity ids | `ENTITY_REGISTRY_1211: [EntitySpec; 130]` static table with `id` = protocol id ([`src/entity_registry.rs`](../src/entity_registry.rs) L87) | Ids are baked next to gameplay attributes (width, health, tracking range). |
| Block/state ids | `BREG: OnceLock<Option<BReg>>` loads `assets/blocks_1211.json` + `assets/block_states_1211.json` ([`src/block_registry.rs`](../src/block_registry.rs) L1489, [`assets/`](../assets/block_states_1211.json)) | Already data-driven, but the files are one-version-only by name and there is no remap story for saved data. |
| DataVersion | `ANVIL_DATA_VERSION = 3955`, `ANVIL_LEVEL_VERSION = 19133` constants; every chunk/level write stamps them ([`src/anvil_io.rs`](../src/anvil_io.rs) L16-18) | Reading a world written by another version has no migration path. |
| Save format versioning | `save_v2.rs` already versions its own format (MET2/MET3/MET4 + legacy tick conversion) | Proves the in-tree idiom for versioned storage; protocol DataVersion is not wired to it. |
| Process-global state | `type Registry<K, V> = OnceLock<Mutex<HashMap<K, V>>>` ([`src/main.rs`](../src/main.rs) L99) plus ~40 `static` registries (`BREG`, `ANVIL_BLOCK_ENTITIES`, `ARROWS`/`PRIMED`/`BOBBERS`/`PEARLS`/`SNOWBALLS`/`XPS`, `SESSIONS`, `FORCELOADED`, `PACKS`, `RULES`, `SCORES`, `BORDER`, loot/advancement/bossbar tables, `INV_STORE`/`POS_STORE`, `DIM_WIRE_IDS`, the mod-bridge singletons) | Globals are one-version-one-world; parallel tests and multi-world are structurally blocked. |
| Build shape | One `megu` crate: 230,969 lines across `src/*.rs`; [`src/main.rs`](../src/main.rs) is 15,776 lines of module text pulling in **32 `include!()` directives** (50 `include!()` total once nested includes in `feature.rs`, `natural_spawner.rs`, `mineshaft_piece.rs`, `structure_template.rs`, `redstone.rs` are counted) | One compilation unit: server, client-shared codec, worldgen, storage and sim all rebuild together; no place to put a shared `megu-proto`. |
| Sibling crates | `client-app` (megu-client), `client-kernel` (megu-kernel), `client-port` (megu-client-mesh), `mod-adapter-core`, `create-adapter`, `mod-offload` each have an independent `Cargo.toml`/`Cargo.lock`; the root [`Cargo.toml`](../Cargo.toml) has no `[workspace]` | Client and server already duplicate the protocol (`client-app/src/protocol.rs` re-implements the wire). A workspace lets `megu-proto` be shared. |
| Vanilla data pipeline | [`tools/vanilla_data/gen_vanilla_data.sh`](../tools/vanilla_data/gen_vanilla_data.sh) pins the 1.21.1 bundler jar by sha1, runs `net.minecraft.data.Main --reports --server`, and [`verify_pack.py`](../tools/vanilla_data/verify_pack.py) fail-closes on the datapack inventory + member hashes | The jar already emits `reports/packets.json` + `reports/registries.json` + `reports/blocks.json` on 1.20.5+ — the raw material for version packs exists and is unused today. |
| License layout | Root crate `GPL-3.0-or-later` ([`Cargo.toml`](../Cargo.toml), [`LICENSE`](../LICENSE)); all subcrates and contributions `LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later` ([`COPYING.LESSER`](../COPYING.LESSER), [`COPYING.AGPL`](../COPYING.AGPL), [LICENSING.md](../LICENSING.md), [CONTRIBUTING.md](../CONTRIBUTING.md)) | The workspace split must preserve exactly this boundary. |

The single-version assumptions are therefore concentrated, not diffuse: ~a dozen literal
sites plus one binary capture plus two static tables plus one DataVersion constant. That is a
cheap migration surface — the design below keeps it that way.

---

## 1. Four-layer version separation

The rule: a version fact may live in exactly one layer, and higher layers ask lower layers
questions through typed APIs. Nobody outside layer 1 ever sees a protocol number.

```
  Layer 4  Gameplay/Worldgen   VersionCapabilities     "can this version do X?"
  Layer 3  Storage             DataVersion + migration  "what wrote this file?"
  Layer 2  Registry/Data       VersionPack + IdRemapper "what is this id called?"
  Layer 1  Wire                ProtocolCodec + PacketIdTable "how is it framed?"
```

### 1.1 Wire layer — `ProtocolCodec` + `PacketIdTable`

- `PacketIdTable` is loaded from `packet_ids.json` (§2) and keyed
  `(phase, direction, packet_name) -> protocol_id`. Senders stop writing hex literals:
  `0x2b` becomes `table.clientbound_play("minecraft:login")`. Missing name → fail closed
  at pack load, not at first send.
- `ProtocolCodec` owns the serializations that differ between versions: varint, string and
  NBT framing are stable from 767 through 770+, but slot/item-stack, component-patch,
  particle and entity-metadata encodings changed across 1.20.5→1.21.x. Each encoding family
  is a codec revision selected once at handshake, keyed off the pack manifest.
- The version gate (`protocol != 767` today) becomes a lookup into the set of packs under
  `assets/versions/`; the status response's `"version":{"name","protocol"}` is rendered
  from the active pack manifest instead of a format literal.
- `client-app` consumes the same `megu-proto` crate (§3) — it already keeps its own
  `PROTOCOL_767` codec and is the second consumer that justifies sharing the layer.

### 1.2 Registry/data layer — `VersionPack` + `IdRemapper`

- `VersionPack` (§2) is the normalized id space of one release: packet ids, registry ids,
  block-state runs. Loaded once, immutable, shared by `Arc`.
- `IdRemapper` is built from `version_pack.py diff --json` output (`id_remap` section):
  `forward: old_id -> new_id`, `reverse: new_id -> old_id`, plus `added_ids`/`removed_ids`
  per registry and per phase/direction. It is used by storage migration (§1.3), by any
  cross-version view (e.g. an overlay written under a different version), and by datapack
  registry merges so appended entries keep stable ids (the policy
  `merge_registry_overlays` already implements: baseline ids stable, new entries appended
  in builder order).
- **Attributes stay code; ids become lookups.** `EntitySpec` keeps width/health/tracking
  range — those are gameplay data `registries.json` does not carry — but its `id` field is
  resolved at load by name through `VersionPack::entity_type_id("minecraft:zombie")`,
  failing closed on an unknown name. Same for menu ids, particle/sound id subsets and the
  hardcoded constants like `PLAYER_ENTITY_TYPE` (128) and the air/void-air/cave-air block
  ids. `BREG` keeps its fast compiled tables but *validates* its state numbering against
  `block_states.json` (which it already effectively does via `assets/block_states_1211.json`
  — the pack adds provenance + versioning to that same claim).

### 1.3 Storage layer — `DataVersion` + migration

- Region/level writes stamp the *active* pack's `data_version` (manifest), not a file-local
  constant: `ANVIL_DATA_VERSION` becomes `pack.manifest.data_version` (3955 for 1.21.1,
  5023 for 26.3 — both classified in `AnvilSchemaVersion` since the relevant chunk
  section schema (`sections[].Y`, `block_states`, `biomes`) is unchanged between them).
- Reads accept `DataVersion <= supported` and dispatch on it: equal → direct load; older →
  apply the `IdRemapper` chain across the intermediate packs (block-state ids, entity ids,
  item ids) before decoding palettes/entities; newer than the newest known milestone
  (currently `> 5023`) → **fail closed** with the same
  idiom [`worldgen::VANILLA_DATA_MISSING`](../src/worldgen.rs) uses today — a named
  command to run, not a silent `Option`.
- `save_v2`'s own MET-versioning stays an orthogonal axis: it tracks *our* container
  revision; `DataVersion` tracks *Mojang's* id space. A loader record stores both.

### 1.4 Gameplay/worldgen layer — `VersionCapabilities`

- One struct, built once from the pack manifest, e.g.
  `VersionCapabilities { data_components: bool, known_packs_negotiation: bool,
  minecart_movement_packets: bool, registry_entry_optionals: bool, .. }`,
  threaded through `ServerContext` (§4).
- Any gameplay/worldgen branch reads `caps.has_x` — never `protocol >= 769`. A capability
  name says *what changed* (which the diff in §2 measures); a raw number says nothing and
  invites scattered drift. `if version == ...` outside layer 1 is a review defect.

---

## 2. VersionPack contract — `assets/versions/<mc_version>/`

One directory per supported release, generated by
[`tools/vanilla_data/version_pack.py`](../tools/vanilla_data/version_pack.py) from the
vanilla server jar's own `DataGenerator --reports` output (`--version-json` reads the
jar's `version.json` for `id`/`protocol_version`/`world_version`/`pack_version`).
Generation is a pure function of the reports: sorted keys, no timestamps, byte-identical
across runs; `verify` re-checks schema, per-file SHA-256 and id invariants fail-closed —
the same trust model [`verify_pack.py`](../tools/vanilla_data/verify_pack.py) already
applies to `datapacks/vanilla`.

### `manifest.json`

```json
{
  "format": 1,
  "generated_by": "tools/vanilla_data/version_pack.py",
  "mc_version": "1.21.1",
  "protocol_version": 767,
  "data_version": 3955,
  "pack_format": {"data": 48, "resource": 34},
  "jar_sha1": "59353fb40c36d304f2035d51e7d6e6baa98dc05c",
  "files": {
    "packet_ids.json":    {"sha256": "<64-hex>", "bytes": 0},
    "registry_ids.json":  {"sha256": "<64-hex>", "bytes": 0},
    "block_states.json":  {"sha256": "<64-hex>", "bytes": 0}
  },
  "counts": {
    "packets_total": 0,
    "packets": {"play/clientbound": 0, "play/serverbound": 0},
    "registries": 0,
    "registry_entries": 0,
    "block_states": 0
  }
}
```

`pack_format`/`data_version`/`jar_sha1` may be `null` only when the source did not provide
them; `mc_version` and `protocol_version` are mandatory. `block_states.json` is listed
only when `reports/blocks.json` was present. `pack_format` records the source's shape:
legacy `{"data": 48, "resource": 34}` on the 1.x line, or the flat
`{"data_major": 121, "data_minor": 0, "resource_major": 97, "resource_minor": 1}`
the 26.x `version.json` emits (tuple `[a, b]` values in `pack_version` normalize to the
same `<key>_major`/`<key>_minor` keys).

### `packet_ids.json`

```json
{"format": 1,
 "packets": {
   "configuration": {"clientbound": {"minecraft:registry_data": 7},
                     "serverbound": {}},
   "handshake": {"serverbound": {"minecraft:intention": 0}},
   "login": {"clientbound": {}, "serverbound": {}},
   "play":  {"clientbound": {"minecraft:login": 43},
             "serverbound": {}},
   "status": {"clientbound": {}, "serverbound": {}}}}
```

Vanilla's `packets.json` `{phase: {direction: {packet_id: {protocol_id}}}}`, flattened to
`packet_id -> int`. Duplicate ids within one phase/direction are a build error.

### `registry_ids.json`

```json
{"format": 1,
 "registries": {
   "minecraft:block": {"protocol_id": 0,
                        "entries": {"minecraft:air": 0, "minecraft:stone": 1}},
   "minecraft:data_component_type": {"protocol_id": 0, "entries": {}},
   "minecraft:entity_type": {"protocol_id": 0, "entries": {}},
   "minecraft:item": {"protocol_id": 0, "entries": {}},
   "minecraft:menu": {"protocol_id": 0, "entries": {}}}}
```

Fail-closed minimums (all enforced by both `build` and `verify`): the five core
registries `minecraft:block`, `minecraft:item`, `minecraft:entity_type`,
`minecraft:menu`, `minecraft:data_component_type` must be present; ids are non-negative
ints unique within their registry.

### `block_states.json`

```json
{"format": 1, "block_states_total": 0,
 "blocks": {"minecraft:stone": {"first_state_id": 1, "state_count": 1,
                                "default_state_id": 1}}}
```

Per-block contiguous state runs tiling `0..block_states_total` globally — the invariant
`build_vanilla_data.py` already asserts against `reports/blocks.json`, now part of the
pack so a loader can prove its own table agrees with vanilla numbering.

### `diff` output → `IdRemapper`

`version_pack.py diff --from A --to B` emits, per `phase/direction` group and per
registry (plus `block_states`): `added`, `removed`, `renumbered`
(`name -> {from_id, to_id}`), `unchanged_count`, `registry_renumbered`,
`added_registry`/`removed_registry`, and an `id_remap` section with `forward`/`reverse`
maps and `added_ids`/`removed_ids` — the ready-to-load input for §1.2/§1.3. The self-test
pins this against synthetic deltas for the two real targets: 1.21.1 (767) → 1.21.4 (769)
and 1.21.1 (767) → 26.3 (777), each including an inserted id that shifts subsequent
numbering and forward/reverse `id_remap` verification.

### Retiring `config_packets.bin`

Today the server replays a captured byte blob and then *parses it back* to learn registry
names, dimension-type ids and to merge datapack overlays. With a VersionPack the flow
inverts: `0x07` registry-data packets are **generated** from `registry_ids.json` +
the datapack overlay builder that already exists (`load_registry_overlays` /
`build_registry_data_payloads` in [`src/play_protocol.rs`](../src/play_protocol.rs)),
in the vanilla packet order. Benefits: no opaque binary in the tree (helps the
public-fork export in [`tools/export_public_branch.sh`](../tools/export_public_branch.sh)
and [`tools/provenance_check.py`](../tools/provenance_check.py)), no vanilla boot under
EULA to onboard a version, and the wire ids are checked against the pack instead of
trusted from a capture. `config_packets.bin` remains the *oracle* for parity tests until
the generated stream is proven byte-equal per packet, then it is dropped.

---

## 3. Incremental Cargo workspace DAG

Goal: replace `include!()` text inclusion with acyclic crates so the protocol/data layers
are shared (`megu` server and `megu-client`), compile incrementally, and version
facts have exactly one owner. Target shape:

```
megu-types      DimId/DimChunkKey/DimBlockPos, AABB, VarInt, NBT Tag, json::J,
                    block/entity name types. Deps: none.
megu-data       VersionPack loader, RegistryTable, IdRemapper, BREG/state facts,
                    assets/* readers.   Deps: types.
megu-proto      ProtocolCodec, PacketIdTable, handshake/status/login, packet
                    framing, 0x07 registry-packet generation.
                    Deps: types, data.  Consumers: megu AND client-app.
megu-storage    anvil_io, save_v2, paletted, tick_persistence, DataVersion
                    migration.          Deps: types, data.
megu-worldgen   worldgen.rs + feature/tree/mineshaft/structure/jigsaw/aquifer
                    (the currently-moduled worldgen cluster + generation_*).
                    Deps: types, data (+ storage's Section type at most).
megu-sim        entity_*, mob_*, natural_spawner, redstone, lighting, commands_*,
                    menus_and_items, combat_and_health, player_mechanisms,
                    recipes_and_stats, workstations_and_mounts, villager_raid,
                    anticheat, save hooks.   Deps: proto, worldgen, storage.
megu (bin)      thin main.rs: config, listener, ServerContext assembly, tick loop.
                    Deps: all of the above.
```

Existing members join one root `[workspace]`: `client-kernel`, `client-port`,
`client-app`, `mod-adapter-core`, `create-adapter`, `mod-offload`. Shared
`[workspace.dependencies]` pins the versions currently duplicated in each `Cargo.lock`
(flate2, signal-hook, ureq, optional jni).

Rules:

- **Acyclic by construction.** `cargo build` enforces the DAG; the forbidden edges are
  proto→sim, data→proto, worldgen→sim. [`tools/rust_module_graph.py`](../tools/rust_module_graph.py)
  already maps the include-graph for the extraction order.
- **License boundary preserved:** root/binary stays `GPL-3.0-or-later`; every library
  subcrate is `LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later` — identical to
  what the five sibling crates already declare ([LICENSING.md](../LICENSING.md) §3).
- **Extraction order is leaf-first, not top-down.** The files already wired as `mod`
  (`json.rs`, `nbt.rs`, `block_registry.rs`, `entity_registry.rs`, `cached_lookup.rs`,
  `landing*`, `connection_limits.rs`, `tick_stats.rs`, `trace_env.rs`, `tree.rs`,
  `world_rng.rs`, `generation_*`) have crate-shaped boundaries and move first; the
  `include!()` files are converted bottom-up as their leaf dependencies externalize.
  Each wave must keep `cargo test` and [`tests/verify.sh`](../tests/verify.sh) green —
  the split is mechanical, never a rewrite.
- **API style on seams:** `pub(crate)` becomes `pub` only across the new crate edges;
  nothing is widened for convenience. A crate that needs a sim concept gets it as a
  parameter, not a dependency (e.g. `megu-data` never learns what a `Player` is).

---

## 4. Runtime state sharding — `ServerContext` / `DimensionState`

Replace the `OnceLock<Mutex<…>>` process globals with owned state:

```
ServerContext            one per process-in-fact (tests may hold several)
├── version_pack: Arc<VersionPack>, capabilities: VersionCapabilities
├── registries: RegistryTables (immutable after load → no lock)
├── players, scoreboard, gamerules, border, bossbars, loot/advancement tables
└── dimensions: DimId -> Arc<DimensionState>

DimensionState           one per dimension
├── chunk_shards: [RwLock<ChunkShard>; 64]   keyed hash(cx, cz) & 63
├── entity sets (ARROWS..XPS, mobs), spawner/census state
├── forceloaded, block entities (ANVIL_BLOCK_ENTITIES), pending blocks/crops/signs
├── scheduled ticks, fluid backlog, light queues
└── save overlay handle
```

- **Why sharding now:** the chunk map is the hottest contended lock (worldgen workers,
  tick, fanout and 6-DOF/sub-world views all hit it; the Aeronautics bridge in
  [`src/aeronautics_and_dim_bridge.rs`](../src/aeronautics_and_dim_bridge.rs) already
  multiplies dimension-like state). 64 shards ⇒ readers stay parallel; a writer holds one
  shard for one chunk-column operation.
- **Lock order, stated once and enforced:** `ServerContext` internals → `DimensionState`
  fields → chunk shards in *ascending shard index*; never shard→context. Multi-chunk
  writes (pistons across chunk edges, `/clone`, structure placement) sort their shard set
  before acquiring. Anything needing a second dimension's state goes through the
  `ServerContext` read path, not a nested write lock.
- **What it buys:** parallel `cargo test` without global bleed (today `Registry<K,V>`
  statics make `cargo test` inherently serial for state), multi-world/multi-dimension
  hosting, 6-DOF sub-worlds as ordinary `DimensionState`s, and — for this spec — a
  `version_pack`/`capabilities` field that is the single runtime owner of the version.
- **`get_or_init` laziness becomes explicit boot:** `breg()`, `dim_type_wire_ids()`,
  `DIM_WIRE_IDS`, the allowlist/idle-timeout statics are all loaded in `ServerContext::new`
  with fail-closed errors; there is no hidden first-call init inside the tick loop.

---

## 5. Phased migration plan (Waves N1–N4)

Follows the repo's Wave/lane convention ([ROADMAP](ROADMAP.md),
[implementation-checklist.md](implementation-checklist.md)). Each wave ends green on
`tests/verify.sh`; none changes wire behaviour alone.

| Wave | Deliverable | Exit gate |
|---|---|---|
| **N1** | *This lane (N1a):* this blueprint + `version_pack.py` (build/verify/diff/self-test). *N1b:* regenerate 1.21.1 datagen `--reports` from the pinned jar ([`gen_vanilla_data.sh`](../tools/vanilla_data/gen_vanilla_data.sh) already runs the generator; add a step exporting `reports/` + `version.json`), `version_pack.py build assets/versions/1.21.1`, pack committed with provenance notes in [THIRD_PARTY.md](../THIRD_PARTY.md) (data is jar-derived, same provenance class as `assets/block_states_1211.json`). | `--self-test` green; `verify` green on the 1.21.1 pack; `tools/provenance_check.py` green. |
| **N2** | VersionPack loader + `PacketIdTable` inside `src/`; packet/registry literals replaced by table lookups; `0x07` registry packets generated from the pack + datapack overlays and proven **byte-equal per packet** against `config_packets.bin`; `ANVIL_DATA_VERSION` reads the manifest. | Wire-parity probes (`tests/verify.sh`, dimension wire, container wire) unchanged; generated config stream ≡ captured stream; `config_packets.bin` demoted to test oracle. |
| **N3** | Workspace split per §3 (types → data → proto first, worldgen/storage/sim next, thin binary last); `ServerContext`/`DimensionState` + 64-shard chunk store per §4; `client-app` switched to `megu-proto`. | `cargo test` in each new crate + full suite green; no `include!()` left on the converted paths; lock-order documented in [docs/development.md](../docs/development.md). |
| **N4** | Second version onboarded end-to-end (checklist §6); `IdRemapper` applied to storage reads (old pack → current pack); `VersionCapabilities` wired through `ServerContext`; parity probes run against both versions. | Both packs serve clients; a 1.21.1-written world loads on the 1.21.4 pack via remap; diff artifact checked in under `assets/versions/diff-1.21.1-1.21.4.json`. |

Sequencing: N2 needs N1's pack; N3 is independent of N2 and can interleave (it only moves
code); N4 needs both. The risky part — the include→crate conversion — is deliberately
mechanical and leaf-first so a wave can land partially without a flag day.

---

## 6. Onboarding checklist: 1.21.4 / protocol 769

1. Download the pinned 1.21.4 server jar; record its sha1 (do not commit the jar —
   [`THIRD_PARTY.md`](../THIRD_PARTY.md) policy).
2. `java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar --reports --server --output gen`
   → `gen/reports/{packets,registries,blocks}.json`; extract `version.json` from the jar.
3. `python3 tools/vanilla_data/version_pack.py build --reports gen/reports \
     --version-json version.json --jar-sha1 <sha1> --out assets/versions/1.21.4`
   then `verify --pack assets/versions/1.21.4`.
4. `version_pack.py diff --from assets/versions/1.21.1 --to assets/versions/1.21.4 --json \
     > assets/versions/diff-1.21.1-1.21.4.json` — review `renumbered`/`added`/`removed`
   per layer; every `renumbered` entry is a site where a literal would have broken.
5. Regenerate the per-version data siblings the repo already owns: `blocks_*.json` /
   `block_states_*.json` equivalents via the [`gen_vanilla_data.sh`](../tools/vanilla_data/gen_vanilla_data.sh)
   Extract.java path, and `datapacks/vanilla` for 1.21.4 worldgen data.
6. Extend `EntitySpec` tables for added entity types (e.g. `minecraft:creaking`); delete
   removed rows; ids now resolve by name, so renumber-only diffs need **no code change**.
7. Declare `VersionCapabilities` deltas discovered in step 4 (new packets, component
   changes, codec revisions) — one field each, one owner.
8. Wire gates: handshake/status/login for 769, chunk+entity wire parity probes against a
   real 1.21.4 client, storage round-trip both directions through `IdRemapper`.
9. Update [ROADMAP](ROADMAP.md) / [implementation-checklist.md](implementation-checklist.md)
   status lines and add the pack to `tools/provenance_check.py`'s tracked-data accounting
   if it is committed (it is generated data, same class as `assets/block_states_1211.json`).

---

## Appendix — failure modes this design rejects

- **Scattered literals:** any `if protocol == …` or packet-id literal outside layer 1 is a
  build/review failure once `PacketIdTable` exists; until then the known sites are the
  table in §0.
- **Silent id drift:** `verify` fails on duplicate/negative ids and missing core
  registries; `build` fails on the same plus non-contiguous block states — a wrong jar or
  truncated report can never produce a usable pack (same philosophy as
  [`verify_pack.py`](../tools/vanilla_data/verify_pack.py)).
- **Opaque blobs:** `config_packets.bin` retires to oracle status in N2; packs are JSON
  with sha256 digests, diffable in review.
- **Compat shims by default:** no long-lived dual-path code; a capability field or a
  remap table carries the difference, matching the repo's hard-cutover convention.
- **License drift in the split:** subcrate manifests copy the sibling crates' triple
  license verbatim; `deny.toml` / [LICENSING.md](../LICENSING.md) boundaries unchanged.
