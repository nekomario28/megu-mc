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
run reports, in `tests/data/template_init_1211.tsv` (4 cells, two rotations):

| case | cell | state | entity |
|---|---|---|---|
| none | 10,80,10 | `chest[facing=north,type=single,waterlogged=false]` | `d8d35ce8…` |
| none | 11,80,10 | `stone` | absent |
| clockwise_90 | 10,80,10 | `chest[facing=east,…]` | `d8d35ce8…` |
| clockwise_90 | 10,80,11 | `stone` | absent |

Three facts a port has to reproduce, none of which are visible from the template
bytes alone:

1. The tag is decoded **through the live entity**, not copied: `CustomName` arrives as
   the string `{"text":"Crate"}` and is saved back as the component `'"Crate"'`.
2. A `LootTable` without a seed gets one generated from the placement's
   `RandomSource` (`LootTableSeed`), so placement is not a pure copy and the seed must
   come from the same draw order.
3. The state is written **rotated** (north → east) and the tag is loaded after the
   air clear and the state write, so the entity sees the final state.

Boundary: the entity is created by `LevelChunk.setBlockState` in a real server; a
`ProtoChunk` does not, so the oracle registers one at the target and only claims the
*load* path. Entities in the template, packet NBT, and save/reload remain unobserved.

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
