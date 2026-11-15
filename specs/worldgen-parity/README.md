# Minecraft 1.21.1 world generation parity

Status: active implementation. User scope: all vanilla biomes and structures,
including the Overworld, Nether and End. No partial fixture is whole-world
acceptance. Source authority is the authenticated 1.21.1 server bundler, SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`, its own datapack, bytecode and executed
native classes. Existing forest full-state runtime and restart parity remain a
regression checkpoint.

## Measured starting boundary

- The datapack contains 64 biomes, 34 structure definitions (16 algorithm types),
  20 placement sets, 186 template pools, 40 processor lists and 7 noise settings.
- At the starting revision, generation selected only Overworld noise settings;
  End island density was a placeholder. Neither other dimensions nor all
  Overworld biomes had whole-chunk parity proof.
- The jar contains 1,180 compressed structure templates, absent from the installed
  datapack. Structure biome tags also need export. Neither definitions present
  on disk nor candidate positions mean generated structures work.
- REA static ZIP inspection: `ev_4e26d90e460629d0e74fa23c59c4690317440f7e4d8b7aeda32306bb9f74791e`,
  complete local evidence `/data/lane-scratch/mc-worldgen-rea-1027.json`.
  Static inventory is not execution evidence. Focused native bytecode recovered
  with official mappings and Vineflower is in
  `/data/lane-scratch/mc-worldgen-re-1027/`.

## Implementation and verification order

1. Structure placement: candidate RNG, frequency, exclusion and concentric rings,
   including preferred-biome relocation. Native execution must fix draw order.
2. Shared named NBT, modified UTF-8, template palettes/block entities/entities,
   rotation/mirror, bounding boxes and placement. Load every native template;
   compare typed values and placement against actual StructureTemplate methods.
3. Template pools, jigsaw connections and processors, then each of the 34
   structure definitions. Non-template algorithms have separate piece builders.
   A fixture must contain positive structure cells and block-entity comparisons.
4. Dimension-specific climate and noise, Nether/End terrain and carvers; remove
   placeholders only after independent native tests. Track every biome and
   reachable feature/config/provider, rather than treating registry coverage as
   behavior coverage.
5. Chunk stage order, structure starts/references and terrain adaptation, dynamic
   heights, blending, generation ticks, save/reload and protocol chunk output.
6. Full-state captures by biome/structure and representative mixed boundaries,
   multiple signed seeds and negative coordinates; all mismatches stay counted.

Each implementation batch records source identity, inputs, native oracle result,
port result and its limits. Existing local/native/hosted/runtime evidence remains
separate. No guessed zero/default substitutions or fixture filtering may close a
missing behavior. Work stays on the current branch; no new external publication
is authorized by this implementation request.

Nested parser acceptance is a separate gate from dispatch-arm coverage. The
[feature codec contract](bulk-codecs.md) explains required-child rejection,
predicate offsets and the shared native column scenes used to check them.
The [sculk contract](sculk-patch.md) distinguishes the post-spread state snapshot
from later live reads: callback arguments retain their native read boundary.

## Current native contracts

`tools/structure_placement_oracle.py` reads all 20 sets from the pinned jar and
executes the 19 random-spread sets through native RandomSpreadStructurePlacement
and StructurePlacement frequency methods. The checked 17,024 vectors cover four
reducers, four rates, both spread types, seven signed seeds, negative and extreme
chunk coordinates. `src/structure_placement.rs` matches their candidate and
frequency results. Ring geometry and native fixed-biome relocation match 84
additional cases. Exclusion zones, real multi-noise biome acceptance and structure
blocks remain open. A diagnostic command reports candidates without claiming starts:

```sh
cargo run --offline --locked -- --worldgen-structure-candidates 0 -64 32
TMPDIR=/data/lane-scratch python3 tools/structure_placement_oracle.py \
  --jar /tmp/carverfresh/server.jar
```

The native 1,180 compressed templates and 83 biome/structure tag files are now
exported byte-for-byte with an authenticated manifest. All templates match the
native typed NBT canonical digest and the actual StructureTemplate.load palette
block order/state/block-entity digest. NBT also matches 41 synthetic native cases
including malformed strings, lone surrogates, NaNs and depth boundaries.
Block/entity transforms and bounding boxes match 192 native cases; all 26,684
states have captured mirror/rotation mappings. Those checks establish loading
and transform preview. The separate [pre-placement processing contract](template-processing.md)
fixes the source/candidate/world read boundaries and RNG ownership for supported
processor shapes. The [fossil placement contract](fossil-placement.md) connects
the bounded full-cube path through production feature dispatch and actual native
feature comparisons on scripted worlds. Tree and template share native voxel
face order, including per-chunk fluid reservation order. Generic neighbor state
changes, initialization, runtime delivery and whole-chunk acceptance are tracked
in the [finalization plan](../template-finalization/README.md), not claimed by
these bounded contracts.

The native-wide state count test exposed a phantom extra slot in the existing
registry; its allocation now has exactly the asset-defined state count. The
Python region reader also interpreted IEEE float/double bits as integers/2;
its numeric decoder is corrected, with a guard observed to fail on the old code.

End island density now matches 560 native float/double vectors. End biome
selection matches 2,352 native cases covering all five End biomes; Nether biome
selection matches 5,460 cases covering all five Nether biomes. Shared BlendedNoise
matches 1,800 double-bit vectors using the actual three dimension configurations
and their modern or legacy RNG sources; the approximate Overworld-only sampler
has been removed.
Nether surface depth and named positional random values match 29,120 native
cases. The existing modern constructor over a legacy RNG is distinct from the
old Nether biome constructor; those paths are not interchangeable.

The offline `MC_SPINE_WG_PRESET=end|nether` terrain path uses the selected noise
settings, height, interpolation cell dimensions and biome source. It required
`MC_SPINE_NOFEAT=1` while the feature layer was gated on the Overworld; the gate
is gone, so those presets now run the carver and feature passes (their own
feature order, their own structure sets by biome) and `MC_SPINE_NOFEAT=1` remains
the layer-attribution switch it is for the Overworld. Ordinary server dimensions
and realm-specific save/tick/wire output are still outside that switch.

Four frozen, feature-free and carver-free native captures now supply all
655,360 states from 20 chunks. Central seed-zero End/Nether chunks and negative
seed outer 3x3 regions match all states. The committed compressed references and
their native input hashes are in `tests/data/dimension_terrain_1211/`. Source
captures are `/data/lane-scratch/mc-{end-terrain-native2,end-outer-native,nether-terrain-native,nether-outer-native}-1027`.
These are selected terrain fixtures, not coverage of all biome terrain or all
dimensions' runtime. No unequal states are excluded.

The expanded tests exposed three existing assumptions: JSON `false` was read
as null and could inherit a true default, the surface scan's preliminary-height
window skipped Nether cave surfaces, and surface biome conditions used raw
quart biomes. False now remains distinct from null; Nether scans the full
column and uses actual block-position BiomeManager selection. The Overworld
surface-biome boundary path still needs its own native closure.

The Nether cave subclass shares the existing tunnel walker and implements the
native cave-count bound, thickness, vertical ratio and fixed lava boundary.
All 294,912 states of a separate seed-zero 3x3 native carver capture match;
its compressed reference includes positive cave-air writes. The native
`VerticalAnchor.BelowTop` contract also exposed and fixed an off-by-one height.
End island feature writes and the following RNG value match the actual feature
in seven signed-seed cases; this is independent placement, not full decoration.
Its normal decoration path additionally matches all states of a 3x3 small-island
capture with 509 actual native feature writes. A targeted mutant that selects
Overworld's feature order for End is rejected by this full-chunk test.
FeatureSorter indices now match native for all three possible-biome sources;
the source order is exported by `tools/feature_order_oracle.py`. The same sorter
still reads the installed biome feature lists. Chorus plant generation matches
42 native controlled worlds, including blocked/invalid origins, final connection
states and the post-generation RNG value.

Reading the remaining feature bodies needs three name tables that are read out of
the pinned jar rather than guessed from a decompiler dump:
`tests/data/feature_classes_1211.tsv` (`tools/feature_class_map.py`) maps all 59
configured-feature types the pack names — inline ones included — to the actual
registered class, and every one of them is registered (a type that is named but
not registered would be a data bug, not a missing port);
`tests/data/block_fields_1211.tsv` maps the bytecode's `dga.*` block fields to
block ids (`dga.H` is lava, `dga.dY` basalt — guessing netherrack there would move
every column by one block); and `tools/structure-oracle/EnumFieldsMap.java` reads
the `ji` direction constants (`ji.a` down, `ji.b` up, `ji.c..f` north/south/west/east).
A wrong direction read silently mirrors a column.

`basalt_pillar` (`ebj`) matches native on 49 cases: seven controlled scenes (short
column, deep column, air above the origin, solid origin, side ledges, no floor at
all, stepped base) times seven signed seeds, comparing the placed flag, every
written cell, and the RNG value after the call. Two properties are measured rather
than assumed. The pillar hangs: the native check reads the cell *above* the origin
(`jd.d()`), and the oracle asserts that the predicate it serves is the height
accessor's `isOutsideBuildHeight` and the other one the block reader's, by the
declared interface shapes. And a sideways attempt that draws `nextInt(10) == 0` is
dropped for the rest of the column — the native `flag = flag && place(...)`
short-circuits, so both the write and the draw disappear. Two mutants are gates:
keeping the direction alive, and reading below the origin instead of above both
fail this fixture.

Two nether vine features are ported the same way. `twisting_vines` (`edr`) matches
42 native cases and `weeping_vines` (`edx`) 48 more: six scenes times eight seeds
(including `Long.MAX_VALUE`), covering the ground/support rules (netherrack, warped
nylium, warped wart block and the refusal cases), the height draws (`nextInt(6)`
doubling, `nextInt(5)` collapsing to one) and the age property.

`minecraft:bonus_chest` (`ebo`, `BonusChestFeature`) closes the configured-feature
identity list: the parser inventory test now asserts that no configured feature the
pack names parses as `Cf::Unknown`. Three scripted worlds pin it. The first is the draw
order and the four effects — `Util.toShuffledList` over `rangeClosed(minBlockX,
maxBlockX)` then the same over Z (30 `nextInt` draws before any read), the shuffled
cross product with X outermost, `MOTION_BLOCKING_NO_LEAVES` for the surface, the
`isEmptyBlock || collision shape isEmpty` cell test, one `nextLong` for
`minecraft:chests/spawn_bonus_chest`, and a torch on each horizontal neighbour whose
support cell passes the face test. The second shows the two halves of the cell test
disagree with the obvious guesses: a leaf block sits *at* the no-leaves heightmap cell
(leaves are excluded from that heightmap) and has a collision shape, so the walk skips
it, while a grass plant also sits at its column's heightmap cell but has no collision
shape and is replaced by the chest. The third is a column filled to the build limit: the
first free cell is outside the world, where the native `setBlock` is refused, so the chest
and its `nextLong` both disappear while the walk still ends. Six mutants are gates:
swapping the two shuffles, reading `MOTION_BLOCKING` instead of
`MOTION_BLOCKING_NO_LEAVES`, inverting the empty collision test, reading the support cell
two below the torch, dropping the `nextLong`, and removing the out-of-world guard. The loot-seed draw is native-verified rather than assumed: `WorldGenRegion
.setBlock` leaves a `DUMMY` block-entity tag for a new block-entity state
(`arb.a(jd,dtc,int,int)`), so the `instanceof RandomizableContainer` guard in
`bqz.a(dcc,ayw,jd,akq)` passes and the seed is spent instead of skipped. Its limits:
the scenes are scripted, not native-oracle fixtures (`tools/feature_oracles.py` names a
`bonus_chest` fixture, but `BonusChestOracle` and `tests/data/bonus_chest_1211.tsv` are
not written yet), the torch support is the pinned full UP face while
vanilla asks with `SupportType.CENTER` — they differ only for a support whose 2x2
centre column covers the face, which no generated bonus-chest surface is — and the
`chests/spawn_bonus_chest` loot table is not in the exported pack, so the payload
records the table and seed without its contents.

Configured-feature identity is now complete, which is not the same as behavioural
coverage: run the actual recursive parsers as well as `tools/feature_type_coverage.py`,
because an outer dispatch arm cannot prove that its children, providers or placement
modifiers execute.

Next consumers: general template placement and neighbor effects, remaining processors, jigsaw and structure-start pipelines; all reachable
feature implementations and their full-chunk interactions;
then whole-chunk multi-biome, normal-runtime, save/reload and wire comparisons.
