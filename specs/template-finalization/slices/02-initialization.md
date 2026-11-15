# Block/entity initialization

Observe actual StructureTemplate placement with NBT-bearing blocks and entities,
including rejection, rotation, position/UUID handling, loot seeds and finalizers.
The native level must perform initialization, not merely record a requested NBT
blob. Existing no-NBT admission remains until its replacement has a passing
contract. Reuse typed NBT and runtime state stores; do not introduce a competing
live block-entity map. Test handoff, packet NBT and save/reload independently.
No data migration is authorized by this plan; stop for a concrete decision if an
existing persistence contract cannot be preserved. Acceptance requires positive
initialized instances and a compiling omission mutant, not empty entity lists.

## Native contract for a block with NBT (bytecode, 2026-11-11)

Read from the pinned `StructureTemplate.placeInWorld` (`ent.a(ddl,jd,jd,enp,ayw,int)`,
official mappings 1.21.1). Offsets are from `javap -c`:

| Offset | Call | Meaning |
|---:|---|---|
| 306 | `ent$c.c:Lub` | the palette entry carries a `CompoundTag` |
| 315 | `ddl.c_(jd)Ldqh` + `bqi.a_` | `getBlockEntity(pos)` and **assert non-null** |
| 330-338 | `dga.hW.o()` then `ddl.a(jd,dtc,int)` | write `Blocks.AIR` default state with flags `20` |
| 344-351 | `ddl.a(jd,dtc,int)` | write the transformed state |
| 460-511 | `ddl.c_(jd)`, `ddl.H_()`, `dqh.c(Lub,Ljo$a)` | re-fetch the block entity and `loadWithComponents(nbt, registryAccess)` |

So initialization is not "record the NBT blob". The native level must (a) already
answer `getBlockEntity` at that position, (b) be cleared to air first, (c) take the
transformed state, and (d) load the tag *through the live block entity* with the
level's `RegistryAccess`. A port that stores the tag on the template and skips the
entity has nothing to load it into, and a port that loads it without a registry
provider cannot resolve components.

## What blocks a runnable witness

The scripted-level harness answers reads and records writes; it does not implement
`getBlockEntity` (`c_`) or `registryAccess` (`H_`). `loadWithComponents` takes a
`HolderLookup.Provider`, and this harness deliberately has no `RegistryAccess`
(the same boundary that stopped the sculk path). A real witness therefore needs one
of:

1. a level proxy that serves a real `ProtoChunk`'s block entities *and* a
   `RegistryAccess` built from the built-in registries; or
2. a captured run inside a real server context (the path that previously produced
   the save-incident, so it needs an isolated worktree and a captured input).

### Attempt 1 (2026-11-11): scripted ProtoChunk — failed at chunk construction

`NativeHarness.scratchChunk` cannot build the chunk at all in this context:

```
java.lang.IllegalStateException: Missing key in ResourceKey[minecraft:root / minecraft:block]:
  ResourceKey[minecraft:worldgen/biome / minecraft:plains]
  at dvj.<init> / duy.a / duy.<init> / dvs.<init>
```

The chunk resolves its default biome through a `RegistryAccess`, and 1.21 has no
static biome registry to satisfy that — the same wall the sculk path hit. So the
route is **not** "more harness": it needs either a registry access loaded from the
vanilla data half of a real startup, or a captured run inside a running server.

Log: `~/.cache/mc-spine-audit/save-booking-20261110/init-probe-attempt.log`.

### Attempt 2 (2026-11-11): datapack registry access — chunk constructs, entity does not

Loading the vanilla datapack by hand gets past the biome wall. The recipe, each
step measured:

```java
// inner jar extracted: pack.mcmeta + data/, 64 worldgen/biome/*.json
Object location = PackLocationInfo("vanilla", literal("vanilla"), PackSource.DEFAULT, Optional.empty());
Object pack     = new PathPackResources(location, extractedDir);
Object rm       = new MultiPackResourceManager(PackType.SERVER_DATA, List.of(pack));
Object base     = RegistryAccess.fromRegistryOfRegistries(BuiltInRegistries.WRITABLE_REGISTRY); // private: getDeclaredField + setAccessible
Object worldgen = RegistryDataLoader.load(rm, base, RegistryDataLoader.WORLDGEN_REGISTRIES);
Object biomes   = worldgen.registryOrThrow(Registries.BIOME);
Object chunk    = new ProtoChunk(chunkPos, UpgradeData.EMPTY, heightAccessor, biomes, null);
```

`RegistryAccess.EMPTY` as the base fails: the loaders cannot resolve `minecraft:block`
or `minecraft:item`. The writable root registry works, and the chunk then constructs
(step 1 below is no longer the blocker).

But a `ProtoChunk` does not create block entities. After
`setBlockState(pos, chest, false)` the state is `minecraft:chest` and
`getBlockEntity(pos)` is **null**, so `placeInWorld`'s own assertion at offset 315
cannot pass. Vanilla creates the entity in `LevelChunk.setBlockState`, which needs a
`Level`, which needs a server. The two honest continuations are:

1. supply the entity ourselves and observe only `loadWithComponents` (real decoding,
   synthetic creation — must be labelled as such), or
2. capture the whole path in a running server.

Log: `init-probe-attempt3.log` (probe source in the audit directory).

### Observed contract (2026-11-11, `tools/template_init_oracle.py`)

The datapack recipe above plus one supplied entity reaches the native load. What the
run reports, in `tests/data/template_init_1211.tsv` (6 cells, two rotations, three
palette entries: a loot-table chest, an explicit-contents chest, and a plain stone
control):

| case | cell | state | entity |
|---|---|---|---|
| none | 10,80,10 | `chest[facing=north,type=single,waterlogged=false]` | `d8d35ce8…` |
| none | 11,80,10 | `stone` | absent |
| none | 10,80,11 | `chest[facing=north,…]` | `583036aa…` (`0=3 redstone`, `2=1 diamond`) |
| clockwise_90 | 10,80,10 | `chest[facing=east,…]` | `d8d35ce8…` |
| clockwise_90 | 10,80,11 | `stone` | absent |
| clockwise_90 | 9,80,10 | `chest[facing=east,…]` | `2ee5a92e…` (same stacks) |

Three facts a port has to reproduce, none of which are visible from the template
bytes alone:

1. The tag is decoded **through the live entity**, not copied: `CustomName` arrives as
   the string `{"text":"Crate"}` and is saved back as the component `'"Crate"'`.
2. A `LootTable` without a seed gets one generated from the placement's
   `RandomSource` (`LootTableSeed`), so placement is not a pure copy and the seed must
   come from the same draw order.
3. The state is written **rotated** (north → east) and the tag is loaded after the
   air clear and the state write, so the entity sees the final state.

4. **A loot table shadows the items.** A container that still holds a loot table
   saves the table *instead of* its contents, so one cell cannot show both: the
   first attempt to observe `Items` failed for exactly this reason (the tag carried
   `LootTable` and the stacks were silently absent from the saved entity). They are
   separate palette entries for that reason, and a port that stores both in one place
   would be wrong in the same way.

Boundary: the entity is created by `LevelChunk.setBlockState` in a real server; a
`ProtoChunk` does not, so the oracle registers one at the target and only claims the
*load* path. Entities in the template, packet NBT, and save/reload remain unobserved.


### General placement owner + container staging (2026-11-11)

The seam above was closed by splitting the owner instead of loosening the fossil rule:

- `Template::place_blocks` is the shared placement owner: processors, coordinate
  transform, clipping after processing, writes, the bounded edge pass and container
  staging. `place_solid_blocks` keeps the fossil contract (full cubes, no NBT) and then
  delegates, so a future caller can place a chest without changing what fossils accept.
- `Template::validate_placement` is the general admission: template entities refused,
  block NBT accepted only as a container `Items` list of `Slot`/`id`/`count`.
  `container_items` decodes it to `(slot, item, count)` and refuses every other key,
  because a loot table would shadow the items natively.
- Accepted contents are staged in `FC::containers` — generation-owned, not a live store.

Tested against the captured native rows (`template_container_items_match_native_placement`):
both rotations, every written cell, the rotated `facing` property, and the staged
contents match `tests/data/template_init_1211.tsv`. `the_fossil_contract_still_refuses_a_container`
pins that a chest is still refused on the fossil path, and both behaviours have their
own compiling mutant.

**Handoff (same day).** `Gen::stage_containers` holds the staged cells per generator, and
`worldgen::block_state` — the one place that owns the generator lock — takes them out
*after* releasing it and calls `materialize_containers`, which inserts into `chests()`
under the save guard. The order matters: the block write path takes save → generator, so
doing this the other way round would be an ABBA deadlock. A second call for the same
position rewrites identical contents, and `take_staged_containers` empties the holding
area so nothing is applied twice.

`staged_container_contents_reach_the_store_and_survive_a_save` proves the whole chain:
placement stages, the generator holds it once, the live store receives it, and a
save/reload brings the contents back. Both the materialiser and the take have their own
compiling mutant.

**Packet (same day).** The open-window packet reads `chests()` directly, so a placed
chest's staged contents are what the client receives — by construction, not by a second
copy. `a_placed_chest_shows_its_staged_contents_in_the_open_window` decodes the `0x13`
payload and checks slot 0 (count, item id, component count), then empties the store and
checks the same window reports an empty slot, so a stale copy would fail. The mutant that
replaces the store read with an empty map is caught.

Still open, and deliberately not claimed: template entities, block entity kinds other than
a container with explicit `Items`, and the native comparison for the window bytes (the
fixture pins placement, not the packet layout).

### Sign text: probed, not captured (2026-11-11)

A sign was added to the oracle to pin the second block-entity family. Three things were
measured before stopping:

1. A standing sign placed **without support is removed by the shape pass** — the write
   log shows `oak_sign` then `minecraft:air`, so a fixture must give it ground.
2. With stone under it the sign survives and the state rotates as expected
   (`rotation=0` at none, `rotation=4` at 90 degrees).
3. The NBT did **not** take effect: the saved entity reports four empty components
   (`'""'` each) instead of the `{"text":"Hi"}` written into the template tag. The cause
   is not established. The leading candidate is that the state write replaces the
   pre-registered entity with a fresh one, so `loadWithComponents` applies to an
   instance that is then discarded; that is not proven.

Follow-up probes narrowed it down and refuted the first hypothesis:

- The entity is the **same instance** before and after placement (`same=true`), so no
  replacement happens; and calling `loadWithComponents` **directly** on the pre-registered
  sign also yields four empty components. So the tag, not the placement, is what fails.
- Four message encodings all load as empty: `'{"text":"Hi"}'`, `'Hi'`, `'"Hi"'`, and
  `[{text:"Hi"},{text:""},…]`. A vanilla-format JSON string is among them, so "the port's
  encoder is wrong" is not supported either.
- One notable asymmetry: the same `loadWithComponents` call applies a chest's `Items`
  correctly in this harness, so the harness can host a real load; signs specifically do
  not take one.

**Resolved by encoding instead of guessing.** Asking the game's own codec
(`SignText.DIRECT_CODEC.encodeStart` with the level's ops) what a sign text looks like:

```
{color:"white", filtered_messages:['""','""','""','""'], has_glowing_text:0b,
 messages:['"Hi"','""','""','""']}
```

So the earlier hand-written tag was wrong in two ways: each message is a **JSON string
component** (`'"Hi"'`, not `{"text":"Hi"}`) and a **`filtered_messages` list is always
present**. The codec also defaulted `color` to `white`. That is why every hand-built
variant loaded as empty text; no level is involved (the bytecode shows
`loadAdditional` uses only the decode provider and logs failures through
`resultOrPartial`).

Consequence for the port: `sign_side_nbt` wrote `color:"black"`, a `{"text":...}` message
and no `filtered_messages`, so a real client would have shown blank lines. It now writes
the game's shape, and `sign_side_nbt_matches_the_shape_the_game_writes` pins it with two
compiling mutants (message form, filtered list). Log: `sign-probe6.log`.

**Captured.** With that tag, the oracle places a sign on stone and the saved entity now
reports the text, so the fixture carries both families (8 rows, two rotations):

| case | cell | state | entity |
|---|---|---|---|
| none | 11,80,11 | `stone` | absent |
| none | 11,81,11 | `oak_sign[rotation=0,…]` | `745749e0…`, text `'"Hi"'` then three empty |
| clockwise_90 | 9,80,11 | `stone` | absent |
| clockwise_90 | 9,81,11 | `oak_sign[rotation=4,…]` | `a2de47a0…`, same text |

The sign needs the stone under it (the shape pass removes an unsupported one) and its
state rotates while the text does not.

**Port side (same day).** One staging channel now carries both families:
`StagedBlockEntity::{Container, Sign}` in the generation context, decoded by
`Template::staged_payload` (a tag with `front_text` is a sign, otherwise container
`Items`; anything else is refused). `Template::sign_lines` accepts the game's JSON string
components and a plain text object, and refuses every other shape rather than showing
blank lines. The handoff writes each variant into the store that owns it — `chests()` or
`signs()` keyed `front = 1` — so the existing chunk-packet block-entity path carries the
text without a second owner.

`template_sign_text_matches_native_and_reaches_the_sign_store` starts from the fixture's
captured message, stages `["Hi","","",""]`, and checks the sign store; two compiling
mutants cover the decode and the store routing. The container comparison test also runs
against the same fixture.

### A second container store, and a refusal for the rest (2026-11-11)

The port keeps one store per menu, so staging a furnace's `Items` into `chests()` would be
invisible: the furnace window reads `furnaces()`. `StagedBlockEntity::Container` now carries
`ContainerKind` decided from the *placed state* at admission — chest/trapped_chest/barrel to
`chests()`, furnace/blast_furnace/smoker to `furnaces()` (slots 0/1/2 → input/fuel/output,
`dirty` set) — and any other container is **refused** rather than staged where nothing reads
it. Two compiling mutants cover the kind decision and the slot mapping.

This is the shape the next kinds take: decide the destination at admission from the state,
write into the store that owns it, and refuse what has no store.

The staging type is now named `GenerationPayload`; its template container/sign
variants retain these contracts. The [mineshaft extension](../../../evidence/gameplay/feature-parity/mineshaft-entities-20261113.md)
adds procedural payloads to the same channel, not general template-entity creation.

### The rest of the container family, and the archaeology that has no store (2026-11-15)

The refusal above was decided from the two *menus* the port had. It was too narrow: `chests()`
is position-keyed and read by more than a window — the hopper tick draws from a hopper,
`fire_dispenser` draws from a dispenser, and a broken block spills its entry at that position
(`set_block_if_inner`) — so dispenser, dropper, crafter, hopper and decorated pot have a store
and their loot was being dropped instead. Measured over the pinned 1.21.1 template corpus, the
states that actually carry a `LootTable` are chest (139), dispenser (4), decorated pot (4) and
barrel (3); the states that carry container `Items` are chest (98), barrel (18), furnace (28),
blast furnace (11), smoker (9), trapped chest (1) and dispenser (1).

`ContainerKind::for_block` is now the one admission all three pipelines read (template NBT,
template-structure block-entity events, jigsaw pieces), with `BlockContainer(n)` carrying the
native menu size so the loot roll cannot fill a slot the block does not have. The jigsaw path
also stages container `Items`, which it previously ignored (`LootTable` still wins, because a
container that still holds a table saves the table instead of its items).

What still has **no** store is archaeology: a suspicious-sand/gravel block's table is recorded
as `LootKind::Brushable` and the block is placed, but nothing is staged for it. Staging it in
the item store would make a broken suspicious-sand block spill loot native never yields — the
same named gap `feature.rs` carries for `desert_well`. Closing it needs a brushing interaction
and the store that owns it, not another variant of this payload.

### Entity geometry captured; creation still refused (2026-11-11)

`StructureTemplate.transform(Vec3, Mirror, Rotation, BlockPos)` is the placement-visible
half of template entities and needs no level, so it is captured in
`tests/data/template_entity_1211.tsv`: 216 rows over two pivots, three mirrors, four
rotations and nine offsets. `structure_transform::entity` already existed but had **no
native comparison**; it now passes all 216 rows, and two compiling mutants cover the
mirror axis and the pivot term.

The capture also documented a trap: the port's `Mirror` variant names are **swapped**
relative to native (`LeftRight` flips `z` like native `FRONT_BACK`). `from_index` keeps the
composition correct, so a caller must map the stored *index*, never the name; the doc
comment now says so.

Creation is a different matter and stays refused: native `placeEntities` calls
`createEntityIgnoreException` → `EntityType.create(tag, level)`, and this harness has no
`Level` (the same wall as sign `setText` and as the chunk's biome). A template carrying
entities is therefore rejected by `validate_placement` — correct-by-failure rather than
silent loss — and no port entity placement is claimed.

### Earlier note: the port has no general placement path (2026-11-11)

Tried to wire initialization into the existing placement and stopped at a real seam
rather than loosening it:

- `Template::place_solid_blocks` is the **fossil** path. Its admission is
  "full-cube states only" (`is_template_solid == Some(true)`, the 26,684-state asset),
  and a chest is not one: `validate_solid_placement` refuses it with
  `solid template placement requires full cubes`. Native `placeInWorld` writes any
  state, so the restriction is the port's, not the game's.
- There is no general template placement path to put initialization into: the
  structure-start / jigsaw pipeline that would call it does not exist yet.

So initialization cannot be delivered by relaxing the fossil rule — that rule exists
for fossils and every fossil palette entry is a full cube. The decoding contract is
nevertheless settled and captured: `container_items` shape (Items list of
Slot/id/count), the loot-table shadowing rule, the rotated cells, and the generated
`LootTableSeed` all come from `tests/data/template_init_1211.tsv`.

Stopped here with the WIP patch stored outside the repository
(`~/.cache/mc-spine-audit/save-booking-20261110/init-port-wip.patch`): it adds a
`Tag` decoder plus generation-owned container staging, and it is only useful once a
general placement path exists to stage into. The next slice is that path (or an
explicit decision to let non-fossil templates reuse the solid placement with a
named non-full-cube exception).

**No native initialization observation exists.** The contract above is bytecode
evidence, not an executed comparison, and it must be confirmed by a run before any
port work is accepted. Choosing the server route is a decision, not a next step:
the last unplanned runtime launch in this work left a save it could not restore, so
that route needs an isolated worktree, a fixed input and an explicit go-ahead.

## Order for the implementation slice

1. Confirm the contract with one executed witness (chest with items, sign with
   text, rotated 90°, plus a template that stores no NBT as the negative control).
2. Add initialization to the shared placement path *after* `validate_solid_placement`
   admits NBT, reusing the existing `chests()`/`signs()` stores; never a new map.
3. Pin, as separate evidence: handoff into those stores, the packet NBT, and
   save/reload. A template that places a chest with contents and reloads empty is
   the failure this slice exists to prevent.
