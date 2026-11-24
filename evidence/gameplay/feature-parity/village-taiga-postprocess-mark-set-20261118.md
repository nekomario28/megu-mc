# Village Taiga `(4,-32)+3×3`: the four cells are the chunk-promotion pass, and the port's mark set is two of nine

Base: `018f8dc` on `perf/worldgen-parallel`, tree clean. Port binary `target/debug/mc-spine`
sha256 `e02b916bb8d9086b1e6ade3877f46f6ba2ff241e2767d7f7442a7be5a20b741f` (`cargo build --locked`,
heavy lease unit; the lane lands **no source change**, see §7).
Raw logs: `mc-spine-local/audit/village-taiga-20261117T232939/`.

**No code is landed by this page.** What it does is replace the two retracted attributions
(mushroom brightness, `BasaltColumnsFeature`) with a measured mechanism, name the part of vanilla
the residual actually lives in, and record the one in-lane change that closes a cell but was not
landed because it moves cells outside the measured window.

## 1. The residual, re-measured on both captures

Both captures are seed 4242, Overworld, `stage: structures`, frozen, `min_y -64` / height 384,
3×3 at `(4,-32)`, 25 decorated chunks, `runtime_writes 5`:

| capture | command | grid diff |
|---|---|---|
| `matrix-v1/nat-4242-village-taiga` | `port_triage <world> 4 -32 --n 3 --pop-order <cap>/order.tsv --pop-order-scope recorded` | **4 / 884,736** (`dumps/village/dump.bin` sha256 `542dcd63…`, identical to the acceptance-rows lane's dump) |
| `t6b/nat-taiga-mlight` | same, with that capture's own `order.tsv` | **4 / 884,736** |

The four cells, both canonical states, in both captures:

| cell | reference (saved chunk) | port | the capture's own RUNTIME line |
|---|---|---|---|
| `(98,19,-512)` | `minecraft:water[level=1]` | `minecraft:air` | `state=81 wrote=true` (declared write) |
| `(99,19,-512)` | `minecraft:water[level=1]` | `minecraft:air` | `state=81 wrote=true` (declared write) |
| `(82,69,-500)` | `minecraft:air` | `minecraft:fern` | `state=0 wrote=true` (declared write) |
| `(99,68,-502)` | `minecraft:brown_mushroom` | `minecraft:air` | `state=2089 wrote=false` |

The fourth cell is the one the earlier lane's report did not quote; it is a *refused* RUNTIME write,
not a declared one, and §5 explains why it is in the same record.

## 2. All 106 RUNTIME lines are one method: `LevelChunk.postProcessGeneration`

Every one of the 106 RUNTIME lines carries the same frame chain below `Level.setBlock`:

```
dcw.a(SourceFile:267) | dcw.a(SourceFile:217) | dvi.H(SourceFile:539) | aqb.a(692) | aqc$b.a(60) | aqb.a(691) | CompletableFuture$UniApply.tryFire | CompletableFuture$Completion.run
```

read against the pinned jar (SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`) and the official
mappings (SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`):

* `dcw` = `net.minecraft.world.level.Level`; `a(BlockPos,BlockState,int,int)` line 267 is the
  `chunk.setBlockState(…)` call site, line 270 the "nothing changed" return — which is exactly the
  `wrote=true` / `wrote=false` split in the log.
* `dvi` = `net.minecraft.world.level.chunk.LevelChunk` (mappings, class block at
  `server.txt:80854`); its `postProcessGeneration() -> H` is `526:551` and the two `Level.setBlock`
  calls are line 534 (the fluid branch) and line 539 (the shape branch):

  ```java
  for (int i = 0; i < this.postProcessing.length; ++i) {          // 527  ShortList[] per section
      for (short s : this.postProcessing[i]) {                    // 529
          BlockPos p = …;                                         // 530
          BlockState st = this.getBlockState(p);                  // 531
          FluidState fs = st.getFluidState();                     // 532
          if (!fs.isEmpty()) fs.tick(this.level, p);              // 534  -> FlowingFluid
          if (!(st.getBlock() instanceof LiquidBlock)) {          // 537
              this.level.setBlock(p,                              // 539
                  Block.updateFromNeighbourShapes(st, this.level, p), 20);
          }
      }
      this.postProcessing[i].clear();                             // 542
  }
  ```

* `aqb` = `net.minecraft.server.level.ChunkMap`, `aqc$b` a chunk-status completion: the pass runs
  from the promotion of a proto chunk, not from a tick.

So the five writes the row declares (3 `air`, 2 `water[level=1]`) and the fourth differing cell are
all produced by **`postProcessGeneration`**, i.e. by the mark set that chunk promotion consumes —
not by the chunk-generation task's feature writes. The acceptance-rows lane's *verdict* (deterministic,
pre-save, inside the frozen artifact, identical across two independent runs) stands; its mechanism
label ("the chunk-gen task's completion path") is the status-task chain that *runs* the promotion,
and is corrected here.

## 3. What fills the mark set: nine call sites, and the port implements two

`ChunkAccess.markPosForPostprocessing` is `duy.e(BlockPos)` (`server.txt:80568`, line 288). Scanning
the whole nested jar for calls to it finds exactly these owners:

| # | caller | what it marks | port |
|---|---|---|---|
| 1 | `arb` = `WorldGenRegion.setBlock` (`setBlock` ends `if (state.hasPostProcess(this, pos)) mark(pos)`) | the three `hasPostProcess` blocks | **yes** (`FC::set`) |
| 2 | `ejv` = `StructurePiece.placeBlock` | `SHAPE_CHECK_BLOCKS` (11 blocks: fences, panes, walls, bars, torches…) | partial: mineshaft/stronghold/fortress/single-piece writers yes, **the jigsaw/template piece path never calls it** |
| 3 | `dzb` = `NoiseBasedChunkGenerator` | the fill/surface state when it carries a fluid | no |
| 4 | `dzq$1` = `SurfaceSystem`'s `BlockUpdater` | `setBlockState` then `if (!state.getFluidState().isEmpty()) mark(pos)` | no |
| 5 | `ebe` = `WorldCarver` (two sites, one shared helper) | every fluid the carver writes (aquifer water) | no |
| 6 | `ecw` = `MultifaceGrowthFeature` | the `glow_lichen` cell it placed | no |
| 7 | `dkz$b` = `MultifaceSpreader` | the multiface cell it spread into | no |
| 8 | `eaa` = `Blender` | `state.is(<some tag>)` or a fluid | no |
| 9 | `ece` = `Feature.markAboveForPostProcessing` | the two cells above a placed block | no (and unused in this jar: zero call sites) |

`hasPostProcess` itself is not a block method but a cached `StatePredicate`:
`Properties.hasPostProcess(StatePredicate)` is `dtb$d.d`, stored in `Properties.B`, copied to
`BlockStateBase.t`, and read by `BlockStateBase.hasPostProcess(BlockGetter,BlockPos)` =
`dtb$a.q` (`server.txt:1213`) — the method `arb` calls. Its **default is the constant predicate
`dtb$d.d(BlockState,BlockGetter,BlockPos) { return false; }`**, and a scan of the jar finds exactly
three `Properties.hasPostProcess(…)` setter call sites, all in `dga` (`Blocks.<clinit>`). So the
three-block set the oracle exports is right, and §4's marked cells are *not* explained by it.

Measured on the port: with `MARK_TRACE` armed (a diagnostic build, not landed), the whole 3×3 window
files **14** marks — the 14 `magma_block` cells `underwater_magma` wrote. The reference's pass
visits **≥106** cells in the same window (every non-liquid mark produces exactly one
`Level.setBlock`, which is what the log records; a liquid mark is only visible when its tick writes).
The 106 are, by state: 54 `glow_lichen` (#6/#7; ten distinct face combinations), 18 `dirt_path`, 14
`magma_block` (#1), 7 `grass_block[snowy=false]`, 6 `andesite`, 3 `air`, 2 `water[level=1]`, 1
`cave_air`, 1 `brown_mushroom`, 1 `fern`. The non-plant states are cells a mark was filed on and a
later write overwrote — the mark outlives the state, which is why a `dirt_path` cell is in the list at
all.

## 4. Per-cell mechanism, stated from the vanilla bodies

**`(99,68,-502)` — a write the port refuses.** `patch_brown_mushroom` (`feature=69,9`, i.e. step 9
index 69 = `minecraft:brown_mushroom_taiga`) writes it in the reference
(`FCELL 99 68 -502 state=2089 wrote=true`); the port's write-time gate
(`generation_providers::can_place`) refuses because `uninitialized_raw_brightness` answered `15`
for a sky dimension, and `MushroomBlock.canSurvive` requires `< 13`. The reference's own read at that
cell is `0` (`-Dmushroomchunk=6,-32`, 403 reads: 401×0, 1×1, 1×2). The promotion pass then keeps it
(`updateFromNeighbourShapes` → `MushroomBlock.canSurvive` → support `isSolidRender` and lit
brightness `< 13`: three spruce-leaf layers above give the port's sky model `15-3 = 12`), which is
why the reference's RUNTIME line for it is `wrote=false` — the re-derivation concluded "no change".

**`(82,69,-500)` — a mark the port cannot file.** The reference's state at pass time is a `fern`
(`FCELL 82 69 -500 state=2006 wrote=true feature=52,9` = `patch_grass_taiga_2`), and
`BushBlock.updateShape` (`!state.canSurvive → air`; `FernBlock`'s `mayPlaceOn` is `#minecraft:dirt`)
removes it because the support below is the village's `dirt_path` (`12513`), which is not in
`#dirt`. Two things are missing in the port: the mark (no `hasPostProcess`, no piece
`SHAPE_CHECK_BLOCKS`, no fluid — one of #3–#8 above; the artifacts on disk cannot separate them,
because every one of those sources writes outside `arb` and the capture's `featurebox` is the 3×3),
and the bush shape rule (`template_structures::update_shape` has no `BushBlock` arm at all).

**`(98,19,-512)` and `(99,19,-512)` — the fluid branch.** The reference's saved chunk has a water
body at `y=19` whose edge is `z=-513` (source water, `level=0`); the two differing cells are its
`z=-512` neighbours. The RUNTIME stack for both is the fluid branch (`dvi.H(534)` →
`FluidState.tick` → `FlowingFluid`), i.e. a marked fluid cell at `(98,19,-513)`/`(99,19,-513)`
ticked and `spreadToSides` wrote `water[level=1]` into the neighbour. The port both misses the mark
(carver/surface sites #3–#5) and, in `postprocess_shape_marks`, **skips fluid states entirely**
where vanilla ticks them. That skip is also the reason the pass is inert on the port's 14 magma
marks (magma holds no fluid).

## 5. The one in-lane half, measured: `uninitialized_raw_brightness` → the measured `0`

`generation_providers::uninitialized_raw_brightness` is the port's `getRawBrightness(pos, 0)` at
`FEATURES` time. It answered `15` for a sky dimension while every measured read is `0`. Landing the
measured value (a one-function change, plus the comment that used to argue for `15`):

| measurement | before | after |
|---|---|---|
| `matrix-v1` village 3×3 (`--pop-order … recorded`) | 4 / 884,736 | **3 / 884,736** |
| `t6b/nat-taiga-mlight` village 3×3 | 4 / 884,736 | **3 / 884,736** |
| cells closed | — | `(99,68,-502)` correct; the two open-sky mushrooms the port now places (`(97,68,-508)`, `(102,68,-509)`) are removed again by the promotion pass (sky model 15 and 14 ≥ 13), so no new cells appear |
| 64-chunk `MC_SPINE_WG_DUMP_N=8` block dump | `c8f28da9…` | **`210c4d81…` — 34 cells move** |

The 34 moved cells are all one shape: 17 × `grass_block[snowy=true] → [snowy=false]` plus 17 ×
`snow[layers=1] → air`, at `y≈80-86` in the `(0,0)` window's snowy peaks. Mechanism, verified with
`POSTPROC_TRACE=6,0` on the changed build:

```
POSTPROC 106 81 0 from=minecraft:brown_mushroom to=minecraft:air light=Some(15)
POSTPROC 102 81 2 from=minecraft:brown_mushroom to=minecraft:air light=Some(15)
…
```

`brown_mushroom_taiga` is step 9 index 69, `freeze_top_layer` is step **10**: with the measured
write-time brightness the patch places a mushroom on the top cell (it is air at that moment), the
freeze pass then declines to put snow on a non-air cell, and the promotion pass removes the mushroom
at `light=15`. So every moved cell is the same three-step sequence, and each is *plausibly* the
reference's own answer — but there is no native capture for that window, and this lane's acceptance
condition is that the 64-chunk identity dump stays byte-identical. **The change was reverted**
(`git diff` empty; the binary above is the clean build) and is recorded here only as a measured,
one-line decision the user can take: accept the 34 cells as corrections and land it, or keep the
identity and leave `(99,68,-502)` open.

## 6. What the write-class verdict got right, and the one word to fix

`runtime-writes-class-verdict.md` (acceptance-rows lane) concludes: the five writes are
deterministic, in the frozen artifact, entered through the same level wrapper, and the row's
`expected_runtime_writes` declaration is legitimate. All of that is re-confirmed here (both captures,
identical payload hash `62fc3ddc…`, 5 writes / 101 refusals, every written cell in the saved chunk).
Only the mechanism name changes: they are the **chunk-promotion pass**
(`LevelChunk.postProcessGeneration` over `ChunkAccess.postProcessing`), not the chunk-generation
task's feature writes. The declaration stays correct because the pass runs before the save and its
output is in the compared artifact.

## 7. What this lane did not do, and why

* **No manifest row promotion.** The village row stays `DIFF` (3 of the 4 cells remain), so
  `manifest-promotable.json` keeps 8 rows and `finite-acceptance-set.md` §6 keeps its current
  statement. Promoting a row that is 3 cells short, or narrowing its window, would be the
  result-shaped edit that document forbids.
* **No mark plumbing from the carver/surface/fill stages.** Those writes are owned by
  `src/worldgen.rs` (a parallel lane owns its terrain/carver/dispatch regions), and the pass itself
  would then need `flowing_fluid` (a runtime module) reachable from the worldgen layer — a layer
  edge this lane may not create on its own.
* **No bush shape rule either**, because without the mark it cannot fire on any cell the port
  visits today: adding it would be code no measurement exercises.

## 8. What a follow-up needs, in order

1. Mark plumbing for the fluid-writing stages (`WorldCarver`'s two sites, `SurfaceSystem`'s updater,
   `NoiseBasedChunkGenerator`'s fill) — the three that can put a fluid cell in the mark set. This is
   the blocker for `(98,19,-512)`/`(99,19,-512)` **and** for `(82,69,-500)` unless a native
   carvers-stage capture shows the fern cell was never marked.
2. The pass's fluid branch: `FluidState.tick` → `FlowingFluid.tick` at every marked fluid cell.
   `src/flowing_fluid.rs` already implements that machine with 28 native fixtures
   (`tests/data/fluid_tick_1211.tsv`); it needs a `Reader` over the feature-facing world view and a
   layer edge worldgen → `flowing_fluid`.
3. `BushBlock.updateShape` in `template_structures::update_shape` (`!canSurvive → air`,
   `mayPlaceOn` per family), once a mark can reach a plant cell.
4. The write-time brightness decision of §5, taken together with whatever window the identity dump is
   allowed to move.

## 9. Gates run in this lane

| gate | result |
|---|---|
| `cargo test --offline --locked --bin mc-spine -- --test-threads=1` at `018f8dc` (no source change) | **660 passed; 0 failed; 1 ignored** |
| village 3×3, both captures, clean binary | 4 / 884,736 each (baseline reproduced) |
| 64-chunk `N=8` dump, clean binary | `c8f28da9…` (the identity the §5 experiment was measured against) |

## Reproduce

```sh
CAP=<local>/captures/matrix-v1/nat-4242-village-taiga
CAPB=<local>/captures/t6b/nat-taiga-mlight
tools/port_triage.py "$CAP/world"  4 -32 --n 3 --pop-order "$CAP/order.tsv"  --pop-order-scope recorded --binary <bin>
tools/port_triage.py "$CAPB/world" 4 -32 --n 3 --pop-order "$CAPB/order.tsv" --pop-order-scope recorded --binary <bin>

# the reference's own record for the four cells
grep -E 'RUNTIME (98 19 -512|99 19 -512|82 69 -500|99 68 -502) ' "$CAP/run.log"

# the pass, per cell, with the brightness it read (port side)
MC_SPINE_SEED=4242 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/v.bin MC_SPINE_WG_DUMP_N=3 \
MC_SPINE_WG_DUMP_X=4 MC_SPINE_WG_DUMP_Z=-32 MC_SPINE_POP_ORDER="$CAP/order.tsv" \
MC_SPINE_POP_ORDER_SCOPE=recorded POSTPROC_TRACE=all ./target/debug/mc-spine
```
