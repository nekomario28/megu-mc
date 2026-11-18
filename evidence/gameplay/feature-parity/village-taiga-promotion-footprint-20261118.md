# Village taiga step 4: the (n+4)² promotion footprint, and the fluid arm that was refusing plants

Base `798f645` on `perf/worldgen-parallel`. Raw logs, dumps, the decompiled classes behind every
vanilla claim and the two scan scripts used here:
`mc-spine-local/audit/village-3-20261118T064633/`.

**Result: `3 / 884,736` -> `1 / 884,736` on both captures, zero new cells.** Two of the three open
cells close; the third is attributed below with the evidence that says why it cannot close from the
port side without a new native instrument.

## 1. What landed

| change | file | measured by |
|---|---|---|
| the promotion region is native's `FULL` demand region, `(n+4)²` for an `(n+2)²` plan: a promotion candidate is clipped to the plan window plus one ring | `worldgen.rs::promotion_region_contains`, `feature.rs::promote_full_chunks` | both captures (§2), a mutation (§6) |
| a 3x3 neighbour outside the recorded plan counts as `FULL` instead of blocking the promotion | `worldgen.rs::promotion_window_ready` | both captures, `a_recorded_plan_window_answers_for_the_neighbours_it_does_not_cover`, a mutation (§6) |
| the population plan carries its own window (`PopulationPlan`), installed by `Gen::populate_plan` | `worldgen.rs`, `main.rs` | the two tests above; the default path is untouched (§3) |
| a fluid replaces a cell whose collision **shape is empty** (`short_grass` and the rest of the `canHoldFluid` families), sideways and below | `flowing_fluid.rs::Cell::Passable`, `template_structures.rs::holds_fluid` | the two measured water cells (§2), two fluid tests, a mutation (§6) |

## 2. The two water cells: `(98,19,-512)` and `(99,19,-512)`

| measurement | before | after |
|---|---|---|
| `matrix-v1/nat-4242-village-taiga` 3x3, recorded order | 3 / 884,736 | **1 / 884,736** |
| `t6b/nat-taiga-mlight` 3x3, recorded order | 3 / 884,736 | **1 / 884,736** |
| seed-0 `MC_SPINE_WG_DUMP_N=8` (the identity row) | `670fdfe3…` | **`670fdfe3…` (byte-identical, 25,165,824 B)** |
| `tools/fixture_suite.py …/manifest-promotable.json` | 8 MATCH | **9 MATCH, 0 DIFF, 0 INVALID, 0 NOT_RUN** (the 9th row is the jungle-temple lane's) |
| `cargo test --offline --locked -j 4 -- --test-threads=8` | 694 passed; 1 ignored | **698 passed; 0 failed; 1 ignored** |

The mechanism is two halves, and both were needed:

1. **The footprint.** `ChunkMap` (`aqb`) promotes a chunk on the load path only:
   `getChunkRangeFuture(pos, 1, $ -> ChunkStatus.FULL)` followed by
   `LevelChunk.postProcessGeneration` (`dvi.H` — the jar's **only** call site of it, a
   `duy.e(Ljd;)V`-style reference scan over the 6,148 classes). So a chunk's marks are consumed
   after its whole 3x3 has reached `FULL`. `FULL`'s accumulated dependency array, computed from
   `ChunkPyramid.GENERATION_PYRAMID` (`dvy.a`) and `ChunkStep.Builder`'s radius array (`dwc$a`), is
   `[light@0, initialize_light@1, carvers@2, biomes@3, structure_starts@4..10]`: a chunk needs *its
   own* `FEATURES` (through `light@0`) but only *earlier* stages of its neighbours. The `FULL`
   region is therefore the request plus two rings while the decorated region is the request plus
   three — and the capture agrees: a 3x3 request at `(4,-32)` holds **49 `minecraft:full` chunks in
   `(2..8) x (-34..-28)`** with zero pending `PostProcessing`, while the recorded plan covers the
   5x5 `(3..7) x (-33..-29)`.

   The port's old rule (`shape_marks_ready` = the whole 3x3 is in `pop_done`) is that same timing,
   but it asked for the neighbours outside the recorded plan as well — and the plan is a *narrowed
   input*: native decorated them, this model did not. So chunk `(6,-33)` (the chunk holding the
   marked source at `z = -513`) could never promote. The fix states the two regions the jar states:
   a candidate must lie in the plan window plus one ring, and a 3x3 neighbour outside the plan
   counts as `FULL`. Every window of a promotion-region chunk is inside native's decorated region
   (`promotion window + 1 = (n+6)²` = the request plus three rings), so the assumption is bounded by
   the same proof that produces the region.

   Under the same recorded order this promotes all 25 plan chunks (and the 9 ring chunks of the
   demand region that have data), where the port promoted 9: `MARKPASS 6 -33 marks=477` against the
   lane-3 hand-written 12-chunk order's 471, the difference being the marks filed by the two chunk
   passes that order did not run.

2. **The fluid arm.** With `(6,-33)` promoted, the tick at the marked source `(98,19,-513)` still
   aborted: `FlowingFluid.getNewLiquid` reads every horizontal neighbour's fluid state, and
   `(100,19,-513)` is `minecraft:short_grass` in this port (measured with `MC_SPINE_WG_DUMP_CANON`;
   water in native). `flowing_fluid` refused any non-solid, non-air cell. Vanilla's gate is
   `canPassThrough` (the two **collision shapes** do not intersect — an empty shape cannot
   intersect) plus `canHoldFluid` (`epc.a(dcc,jd,dtc,epd)`: `SimpleWaterloggedBlock` first, then
   `DoorBlock`, `#minecraft:signs`, `ladder`, `sugar_cane`, `bubble_column`, `nether_portal`,
   `end_portal`, `end_gateway`, `structure_void` are refused by name, everything else answers
   `!state.blocksMotion()`), and `spreadTo` then destroys the block and writes the fluid. So a cell
   with an empty collision shape is one a fluid may enter; a cell with a *partial* shape stays
   refused, because its answer depends on the two shapes' geometry.

   That is the arm, and it closes both cells: the source at `(98,19,-513)` now writes
   `water[level=1]` into `(98,19,-512)` and the one at `(99,19,-513)` into `(99,19,-512)`.

## 3. What the footprint change does *not* touch

The default population discipline (`MC_SPINE_POP_ORDER` unset) decorates native's whole decorated
region, so `pop_done` alone already answers the promotion question there — `promotion_window_ready`
has no window to narrow and `promotion_region_contains` has no region to clip. That is why the
seed-0 N=8 identity dump stays byte-identical: it runs the default path. The window exists only for
a recorded plan, and only a plan's boundary can be a narrow window.

## 4. The one remaining cell: `(82,69,-500)`, `air` in native against `fern` here

Native's own trace says the cell was **marked**: the write is
`RUNTIME 82 69 -500 state=0 wrote=true` with
`stack = dcw.a(267) | dcw.a(217) | dvi.H(539) | aqb.a(692) | …`, i.e. `LevelChunk.postProcessGeneration`'s
`setBlock(pos, state, 20)` — `H` only ever walks marked positions. (The two water cells show the
same frame at `dvi.H(534)`, one branch up: `epc`/`epe` = `WaterFluid`/`FluidState` under the fluid
branch.) The substrate at pass time is `dirt_path` (state 12513, `RUNTIME 82 68 -500 … wrote=false`),
which is outside `#minecraft:dirt`, so `BushBlock.updateShape` (`dgh.java`) answers air — the rule
the step-3 lane measured and reverted from the *shared* `update_shape` (§5 of
`village-taiga-promotion-pass-20261118.md`).

What is missing is the **mark**, not the rule:

| instrument | result at `(82,69,-500)` |
|---|---|
| `MARK_TRACE=all` on the village 3x3 (8,551 marks, 32 promoted chunks) | no line for the position |
| `FWRITE 82 * -500` (feature writes, whole column) | `69` = the fern (state 2006, by a `patch_taiga_grass`-class feature of chunk `(4,-32)`), `68` = `dirt_path` (piece), `67` = stone (carver top material). **No disk write anywhere in the column.** |
| the jar's nine `markPosForPostprocessing` owners | port files six (`write`, `piece`, `fill`, `surface`, `carver`, `multiface`); the two it does not are `Feature.markAboveForPostProcessing` (`ece.a(dds,jd)`, whose only callers are `DiskFeature` and `LakeFeature` — two call sites, so the old "no caller in the pinned jar" comment was wrong and is corrected) and `Blender` (needs blending data this version never writes) |

`Feature.markAboveForPostProcessing` marks `pos.above()` and `pos.above(2)`, stopping before an air
cell, so it would need a disk write at `y = 67` or `68` of that column to reach `69` — and there is
none in the port. Two sources therefore remain possible and the existing capture cannot separate
them: (a) native's disk placement differed at that column (a placement divergence upstream of
marks), or (b) a mark source neither the port's six nor the two named here. Deciding between them
needs a native run with a `ChunkAccess.markPosForPostprocessing` hook logging `(position, caller)`,
which is a new capture — out of scope for this lane, and the reason the cell is left standing rather
than guessed at.

The removal rule itself is a separate, smaller task for whoever lands that instrument: `dgh`'s
`!canSurvive -> air` with `mayPlaceOn = #minecraft:dirt || farmland`, scoped to
`postprocess_shape_marks` and **not** to the shared `update_shape` (landing it on the shared one
costs 232 diffs; measured in step 3 and not repeated here).

## 5. Gates

| gate | result |
|---|---|
| `tools/port_triage.py` `matrix-v1/nat-4242-village-taiga` 3x3, `--pop-order … recorded` | **1 / 884,736** |
| `tools/port_triage.py` `t6b/nat-taiga-mlight` 3x3, `--pop-order … recorded` | **1 / 884,736** |
| seed-0 N=8 dump (`670fdfe3472812af…`, 25,165,824 B) | **byte-identical** |
| `cargo test --offline --locked -j 4 -- --test-threads=8` | **698 passed; 0 failed; 1 ignored** |
| `tools/fixture_suite.py …/manifest-promotable.json` | **9 MATCH, 0 DIFF, 0 INVALID, 0 NOT_RUN**, exit 0 |
| `bash tools/layers_check.sh` | **PASS** (44 modules, 111 edges, 0 named errors) |
| `bash tools/debt_ratchet.sh` | **0 over limit** |
| `MUTATION_STABILITY=1 bash tools/mutation_check.sh --offline` (filtered, 2 entries) | **2 proved, 0 not a gate** (registry 480 -> 482) |

Mutations registered: "a recorded plan's window is native's decoration, not this model's"
(`!in_window(c) || pop_done.contains(&c)` -> `pop_done.contains(&c)`) and "a fluid replaces a cell
whose collision shape is empty" (`Cell::Passable => put(fluid)` -> `Ok(())`). The classification
itself (`holds_fluid`) is pinned by `the_pass_sees_an_empty_collision_shape_as_a_cell_a_fluid_can_hold`
but has no registered entry: two entries was the lane's budget, and the machine arm is the one the
measured cells exercise.

## 6. Reproduce

```sh
CAP=/home/yuu/mc-spine-local/captures/matrix-v1/nat-4242-village-taiga
tools/port_triage.py "$CAP/world" 4 -32 --n 3 --pop-order "$CAP/order.tsv" \
  --pop-order-scope recorded --binary target/debug/mc-spine          # 1 / 884736

# which chunks promote, and with how many marks
MC_SPINE_SEED=4242 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/v.bin MC_SPINE_WG_DUMP_N=3 \
MC_SPINE_WG_DUMP_X=4 MC_SPINE_WG_DUMP_Z=-32 MC_SPINE_POP_ORDER="$CAP/order.tsv" \
MC_SPINE_POP_ORDER_SCOPE=recorded MARK_TRACE=all ./target/debug/mc-spine | grep MARKPASS

# the cell the fluid machine used to refuse
MC_SPINE_SEED=4242 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/c.bin MC_SPINE_WG_DUMP_N=1 \
MC_SPINE_WG_DUMP_X=6 MC_SPINE_WG_DUMP_Z=-33 MC_SPINE_WG_DUMP_CANON=/tmp/c.canon \
MC_SPINE_POP_ORDER=/home/yuu/mc-spine-local/audit/village-taiga-closure-20261118T114736/order-c6m32-ring.tsv \
MC_SPINE_POP_ORDER_SCOPE=recorded ./target/debug/mc-spine
awk -F'\t' '$2==19 && $3==-513' /tmp/c.canon     # 100 = short_grass, 98/99 = water
```

## 7. Fern verdict: mechanism B, `Feature.markAboveForPostProcessing`

§4 could not separate "native's disk placement differed" (A) from "a mark source we do not model"
(B), because a mark leaves no trace in the saved world. This section is the measurement that
separates them: a `markPosForPostprocessing` hook in the carve-agent (`3fd3be2`) and one native
capture of the same window. Base `9a6169b` + the instrument. Raw logs, the instrument diff, the
decoded stacks and `analyse_marks.py`: `mc-spine-local/audit/fern-verdict-20261118T130409/`.

**Verdict: B. The cell is marked, by native's `Feature.markAboveForPostProcessing`
(`ece.a(dds,jd)`), called from `LakeFeature`'s barrier-shell loop. The port's disk placement at
that column does *not* differ** — both sides record the same three writes, in the same feature
identities, so the block the mark was filed from was already placed by the port.

### 7.1 The capture, and what it proves about capture stability

`captures/matrix-v2/nat-4242-village-marktrace`, seed 4242, 3x3 at `(4,-32)`, `frozen`,
`stage structures`, `-Dmarktrace=all`, `boundary.json`: 9 chunks, 25 decorated, `runtime_writes 5`,
`height_window noise`. The instrument is the tree's, so `inputs.sha256` verifies (§5's F5 gap does
not apply to this capture).

| comparison against `matrix-v1/nat-4242-village-taiga` | result |
|---|---|
| `native.bin` (884,736 cells) | **bit-identical**, `0dd5a9d8abb350c3…d11654e8` on both |
| `boundary.json` mode/stage/chunks/decorated/runtime_writes | identical |
| `game_times` | `[2,2]` here, `[3,3]` there — the pre-freeze tick count, not a cell quantity |
| `order.tsv` | same **set**; entries 22 and 23 of 25 transposed (`(7,-31)` ↔ `(6,-29)`) |

So the fresh capture does not reproduce the recorded decoration *order* exactly, while it
reproduces every cell of the world exactly. The order difference is a trace-ordering artifact (two
of the last decoration slots reported in the other order), and it is worth knowing before anyone
treats a recorded `order.tsv` as stable input: **the decorated set is stable, the position of the
tail is not**.

### 7.2 (a) The cell is in native's mark set

```
MARK filed 82 69 -500 chunk=5,-32 n=1 s=4216
  stack = dvs.e | ece.a(211) | ect.a(139) | ece.a(168) | ebq.a(28) | eiv.a(54) | ForEachOps...
```

Decoded against the pinned jar's `LineNumberTable` (`tools/vanilla_src.sh`, jar sha1
`59353fb4c36d304f2035d51e7d6e6baa98dc05c`):

| frame | body | what it is |
|---|---|---|
| `dvs.e(Ljd;)V` | `ProtoChunk.markPosForPostprocessing` | appends to the section bucket `dvi.H` walks — the only body that *manufactures* a pass entry |
| `ece.a(211)` | `Feature.markAboveForPostProcessing` | offset 36–46 of `a(dds,jd)`: `level.getChunk(p).markPosForPostprocessing(p)` (`invokeinterface duy.e`) |
| `ect.a(139)` | `LakeFeature.place` | offset 1182: `this.a(level, pos)` immediately after `level.setBlock(pos, state, 2)` (offset 1170, line 138) |
| `ece.a(168)` | `Feature.place(FC, level, gen, random, origin)` | the 4-arg entry that wraps the context |
| `ebq.a(28)`, `eiv.a(54)` | `ConfiguredFeature.place`, `PlacedFeature` stream | the placement pass |

`markAboveForPostProcessing` is `p = pos.mutable(); for (i = 0; i < 2; i++) { p.move(UP); if
(level.getBlockState(p).isAir()) return; level.getChunk(p).markPosForPostprocessing(p); }`, so the
marked cells are `pos+1` and `pos+2` and both must have been non-air. The capture marks **both**
`(82,68,-500)` and `(82,69,-500)` from the same site at the same window serial `s=4216`, so
`pos = (82,67,-500)` — exactly the cell `FCELL 82 67 -500 state=1 wrote=true feature=1,1` records
the lake writing (state 1 = `stone`: the lake's barrier).

### 7.3 (b) The column's disk state, and (c) the discriminator

| y | native saved world | native `FCELL` write | port `FWRITE` write |
|---|---|---|---|
| 70 | air | — | — |
| 69 | **air** (fern removed by the pass) | `2006` `feature=52,9` (fern) | `2006` `feature=52,9` |
| 68 | `dirt_path` `12513` | `12513` `feature=25,4` (piece) | `12513` `feature=25,4` |
| 67 | `stone` `1` | `1` `feature=1,1` (**the lake's barrier**) | `1` `feature=1,1` |

The port's own feature trace holds the same write, at the same cell, with the same state and the
same feature identity — so the port's LakeFeature already places what native's mark was filed
from. What it does not do is file the mark: `MarkSrc` has `write`/`piece`/`fill`/`surface`/
`carver`/`multiface` and no `above` variant. Filing it would reach `(82,69,-500)`: the mark's
owning chunk is `(5,-32)`, inside the plan `(3..7) x (-33..-29)`, and the pass runs there
(`RUNTIME 82 69 -500 state=0 wrote=true`, stack `dvi.H(539)`).

One correction to §4 falls out of this table, and it is the reason §4 read the column as
unreachable: §4 listed `67` as "stone (carver top material)", but the port's own `FWRITE` line for
that cell is `FWRITE 82 67 -500 state=1 feature=1,1` — the **lake**, not the carver. The lake's
barrier write is the only *feature* write at `y=67` on either side; it is invisible in the saved
world only because `stone` is also what the terrain already had there, so "no disk write in the
column can reach `y=69`" was a misreading of a write that was present and state-identical. A
feature trace names the last writer of a cell; a saved world cannot, when the state does not
change.

Rejected alternatives, each by measurement:

| # | hypothesis | measurement | verdict |
|---|---|---|---|
| A | native's disk placement at/above the column differed | both sides: `67 = 1 (1,1)`, `68 = 12513 (25,4)`, `69 = 2006 (52,9)` | refuted |
| B1 | marked but swallowed by `ImposterProtoChunk` | `MARK dropped = 0`, `MARK unsupported = 0` in 71,518 mark calls | refuted |
| B2 | marked on a chunk the pass never promotes | owning chunk in the plan; `RUNTIME` lines exist there | refuted |
| B3 | one of the six modelled sources, gated differently | the column's marks are `ece` (68, 69) and `dzb` (y=34, 35) only | refuted |
| B4 | `Feature.markAboveForPostProcessing`, unmodelled | caller `ece.a(211)` via `ect.a(139)`; no `MarkSrc` for it | **confirmed** |

Scale, for the next lane's budget: `ece` files **9,975 of 71,518** marks in this run — 9,273 from
`DiskFeature` (`ebx.a(53)`, all outside the plan window), 443 from `LakeFeature` line 139 (138
inside the plan), 259 from `LakeFeature` line 111 (1 in the plan). Inside the 5x5 plan the port
therefore holds **139 of 8,019** marks, and the fern cell is one of them. `dzq$1`
(`SurfaceSystem`) and `eaa` (`Blender`) filed nothing here.

### 7.4 The completeness gate — the hook saw what the pass consumed

A hook that never fires and a hook that fires but filters everything out both leave an empty
trace, so the trace is checked against the pass itself:

| gate | result |
|---|---|
| `MARK` lines / distinct `(cell+chunk+stack)` / repeats | 71,518 / 67,291 / 4,227, all `filed` |
| caller census (frame above the mark body) | 6 owners present, `dzq$1`/`eaa` absent, `arb` present as `arb.f(308)` |
| cells the pass called `setBlock` on (`RUNTIME`) | 106 |
| … that carry a `MARK` line | **104** |
| … that do not | 2, `(98,19,-512)`/`(99,19,-512)`, both with the **fluid-tick** frame `dvi.H(534)` — written by their marked neighbours `(98/99,19,-513)` (`dzb`), not by marks of their own |
| `MARKERR` | 0 |

So every cell the pass touched is either marked or explained by a marked cell's fluid tick. One
instrument caveat is recorded rather than papered over: this capture's `run.log` carries `VINESUM`
but not `MARKSUM`, the second line of the same shutdown hook. The hook body is proven (the build
probe prints both lines; a standalone JVM with 70,000 `mark()` calls prints `MARKSUM
calls=70000`), so the second write is lost in the server's own log shutdown — the 71,518 `MARK`
lines are the stronger evidence that it ran.

### 7.5 What this hands the next lane

Two independent gaps, in dependency order:

1. **The mark source** — `Feature.markAboveForPostProcessing` for `LakeFeature`'s two marking
   sites (lines 111 and 139) and `DiskFeature`'s. 139 marks in this window, 9,975 in this run.
   This is the cell's mechanism: without it the fern is never visited.
2. **The removal rule** (§4's `dgh` arm, `!canSurvive -> air` under `mayPlaceOn = #minecraft:dirt`,
   scoped to `postprocess_shape_marks` and not to the shared `update_shape`). With the mark filed
   and the rule absent, the pass visits the cell and keeps the fern; the cell only closes when
   both are in.

The `feature=index,step` numbering matches between `FCELL` and `FWRITE` (`52,9`, `25,4`, `1,1`), so
the two traces are comparable cell by cell — that identity is what makes "the port placed the same
block" a measurement rather than a plausibility argument.

Reproduce:

```sh
AUD=mc-spine-local/audit/fern-verdict-20261118T130409
CAP=mc-spine-local/captures/matrix-v2/nat-4242-village-marktrace
python3 $AUD/analyse_marks.py "$CAP" \
  --port-fwrite mc-spine-local/audit/village-taiga-closure-20261118T114736/fwrite-all.txt
grep -F "MARK filed 82 69 -500 " "$CAP/run.log"        # the fern cell, with its stack
grep -c  "MARK "               "$CAP/run.log"          # 71,518
cmp "$CAP/native.bin" mc-spine-local/captures/matrix-v1/nat-4242-village-taiga/native.bin
```

The capture itself: `CARVER_FIXTURE_BASE=<persistent base> MC_SPINE_SEED=4242
MC_FIXTURE_STAGE=structures MC_FIXTURE_JAVA_EXTRA='-Dmarktrace=all' bash
tools/carve-agent/native_fixture.sh <dir> 4 -32 3 frozen`, under a heavy lease → `systemd-run`
`--slice=devin-compute` (`MemoryMax=3G`, `CPUQuota=250%`, `RuntimeMaxSec=900`) → bind → release.
