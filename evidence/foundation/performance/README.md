# Chunk generation performance

Measured 2026-10-22 on this host (AMD, 32 vCPU shared; numbers are medians of 3
runs unless stated). Reproduce with the commands below.

## Baseline

| workload | time | per chunk |
|---|---|---|
| `chunk_cols` + `surface(0,0)`, 16x16 chunks | 3.44 s | **13.4 ms** |
| full chunk (24 sections x 4096 blocks), 64 chunks, features off | 2.61 s | **40.8 ms** |
| full chunk, 64 chunks, features on | 4.38 s | 68.4 ms |
| full chunk, 64 chunks, carvers off (`MC_SPINE_NOCARVE=1`) | 3.19 s | 49.9 ms |

So carvers cost ~20 ms/chunk on top of the 40 ms terrain, and features ~28 ms.

Work per chunk: **~1.75M `eval_c` calls** (`WG_COUNT=1`, measured 2026-11-18).
That is ~18 density-function node visits per block.

**The count is a visit count, not a wall-clock claim** (corrected 2026-11-18,
see "Compiling the density DAG" below). Making the evaluator three times
cheaper at the same step count moved the 64-chunk workload by 4.7%, so the
visits were cheap per visit and they are not what the chunk time is mostly made
of. Read the stage deltas, not the count.

```sh
MC_SPINE_NOFEAT=1 MC_SPINE_WG_PROFILE=16 ./target/release/mc-spine          # cols+surface
WG_COUNT=1 MC_SPINE_NOFEAT=1 MC_SPINE_WG_PROFILE=4 ./target/release/mc-spine # + eval count
MC_SPINE_NOFEAT=1 MC_SPINE_WG_PROFILE=8 MC_SPINE_WG_DUMP=/tmp/p.bin \
  MC_SPINE_WG_DUMP_N=8 MC_SPINE_WG_DUMP_X=0 MC_SPINE_WG_DUMP_Z=0 ./target/release/mc-spine
# the compiled evaluator against the interpreter, same binary (the A/B below)
MC_SPINE_DF_INTERP=1 MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/i.bin MC_SPINE_WG_DUMP_N=8 ./target/release/mc-spine
MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/tmp/c.bin MC_SPINE_WG_DUMP_N=8 ./target/release/mc-spine
cmp /tmp/i.bin /tmp/c.bin
```

## Measurement discipline

Single runs are not usable here: the same binary measured 10.4 ms and 13.3 ms
per chunk within one session (thermal state / host load). Every A/B below is
three interleaved runs of the full 64-chunk dump, and only a difference larger
than the spread is reported.

## Optimisations tried, with results

| change | result |
|---|---|
| LRU `Vec::remove(0)` -> `VecDeque::pop_front()` (7 caches, O(n) memmove per insertion) | 13.2 -> 12.7 ms/chunk, inside the noise band; kept (O(1) is right, cost is correctness-of-shape not speed) |
| `HashMap` -> open-addressing memo keyed by `(id,x,y,z)` | **3x slower**; reverted |
| reuse one memo (scratch) + `clear()` per block instead of a fresh map | 12.0 ms vs 10.4 ms (slower: `clear()` + `mem::replace` costs more than the allocation); reverted |
| SipHash -> multiply-xor hasher for the density memo (1.6M lookups/chunk) | 2.617/2.606/2.610 s vs 2.600/2.599/2.581 s: **-0.4%, inside the noise**; not kept |
| compile the density-function DAG into a flat post-order instruction list, evaluated by a stack machine | 8.201 -> 7.816 s (64 chunks, interleaved 3+3, same binary, one flag): **-4.7%**, -6.0 ms/chunk; 8.065 -> 7.804 s against the pre-change binary (**-3.2%**). Kept. See the section below |
| parallelise terrain across chunks (one `Gen` per worker) | **landed 2026-11-18, see "Parallelising the terrain stage" below**. Terrain-only dump 2.633 -> 0.557 s at 8 workers (**4.73x**), full chunk 8.273 -> 7.164 s at 5 (**1.15x**). The win is wall clock; CPU rises (terrain-only +13%, full +88% at 5 workers, because the worker-side `cols`/beardifier work is per worker) |

Conclusion (2026-11-18): the container choices are exhausted, and the number of
density-function evaluations is the right thing to attack *within* the
evaluator - but it is not the whole chunk. Compiling the DAG (lever 1) is
measured at 4.7% of the full chunk; the per-block work *around* the density
function is the larger share of the terrain stage (see "What the 4.7% says
about where the terrain time goes"). Lever 2 is still open:

1. **Compiled / done (2026-11-18)**: the DAG is compiled into a flat post-order
   instruction list and evaluated with a stack (what the Java JIT sees and what
   hyperchunk's `df_compile.c` does), eliminating the per-node `match` + memo
   hash. -4.7% on the full chunk (same binary, one flag).
2. **Parallelise across chunks**, which is what vanilla itself does.

## Compiling the density DAG (2026-11-18)

`final_density` is a DAG of 290 nodes and `block_density` walks it once per
block. The evaluator was a recursive interpreter: one `EVAL_COUNT` gate, two
enum matches for the `interpolated`/`cache` special cases, a jump-table `match`
on the node kind, a recursive call, and a fresh `HashMap` memo per block.

`DfProg` (`src/worldgen.rs`, "compiled density evaluator") linearises the DAG
once per generator into a flat post-order instruction list and evaluates it
with a stack: constants are folded into the instruction stream, a constant
operand becomes an immediate form (`AddC`/`MulC`/`MinC`/`MaxC`), a `cache` node
compiles to its input, and `range_choice` / `interval_select` / `div` emit
conditional jumps so only the taken branch is evaluated.

Two kinds of node stay on the interpreter, on purpose: splines (`f32`
locations, derivatives and values, with `f32` coordinate round-trips - restating
them in `f64` would be a different world, and they are 0.3% of the visits) and
`find_top_surface` / `old_blended_noise` / `end_islands`.

### Identity, before speed

| check | result |
|---|---|
| 64-chunk block dump (`MC_SPINE_WG_DUMP_N=8`), full / `NOFEAT` / `NOFEAT`+`NOCARVE` | byte-identical (25,165,824 B each) |
| the same dump at seed 12345, and from a negative-origin window | byte-identical |
| nether (seed 4242) and end dumps | byte-identical |
| `compiled_density_matches_interpreter_per_block` (3 chunks x 4 presets, every 7th y, every column, `to_bits` comparison) | green |
| `compiled_density_matches_interpreter_without_chunk_grids` (corner sampling, `cc = None`) | green |
| committed fixtures (`dimension_terrain_1211`, the seed-0 overworld gate, the forest fixtures, RNG streams) | green, unchanged |

The mutation campaign holds the two identity tests to be able to fail: a
`cache` node compiled to a miss, and the `interpolated` grid slot read one slot
off (`MUTATION_STABILITY=1`, both red/red/green, `tools/mutation_check.sh`).

### Measurement

Same binary, one flag (`MC_SPINE_DF_INTERP=1` selects the interpreter), 64-chunk
dump, three interleaved runs, **medians**. Spread across the three runs: 0.010 s
on the compiled full arm and 0.023 s on the interpreted one, against a 0.385 s
delta.

| workload | interpreted | compiled | delta |
|---|---|---|---|
| full (features on) | 8.201 s / 128.1 ms per chunk | 7.816 s / 122.1 ms | **-0.385 s (-4.7%), -6.0 ms/chunk** |
| `MC_SPINE_NOFEAT=1` | 4.486 s / 70.1 ms | 4.187 s / 65.4 ms | -0.299 s (-6.7%), -4.7 ms/chunk |
| `MC_SPINE_NOFEAT=1 MC_SPINE_NOCARVE=1` | 3.030 s / 47.3 ms | 2.862 s / 44.7 ms | -0.168 s (-5.5%), -2.6 ms/chunk |
| `MC_SPINE_WG_PROFILE=8 MC_SPINE_NOFEAT=1` (columns + surface) | 1.038 s / 16.2 ms | 0.929 s / 14.5 ms | -0.109 s (-10.5%), -1.7 ms/chunk |

Per stage, per chunk (full - nofeat - nofeat+nocarve):

| stage | interpreted | compiled | delta |
|---|---|---|---|
| terrain (density + material + aquifer + surface window) | 47.3 ms | 44.7 ms | -2.6 ms |
| carvers | 22.8 ms | 20.7 ms | -2.1 ms |
| features | 58.0 ms | 56.7 ms | -1.3 ms |
| **chunk** | **128.1 ms** | **122.1 ms** | **-6.0 ms** |

The carver and feature stages lose time too because carver anchors and some
feature placement query the same density function.

For the record, the **pre-change binary against the final one**, same
discipline: 8.065 s vs 7.804 s, **-3.2%**. That is what a build of this tree
gains; it is smaller than the flag A/B because the *interpreted* arm of the new
binary is itself ~1.7% slower than the pre-change binary (one extra field, one
extra test per block, LTO layout movement) - part of the evaluator win is paid
back outside it. Quote the flag A/B to attribute the evaluator, and the binary
A/B to predict a build.

The host was ~2x slower on 2026-11-18 than the 2026-10-22 table above, which is
why the absolute numbers are not comparable to it; every delta above is from
interleaved runs of one session.

### What the 4.7% says about where the terrain time goes

The evaluator went from 1,751,278 interpreter node visits per chunk to 387,576
(the remainder is the corner samples that fill the `interpolated` grids plus the
`Op::Call` subtrees), i.e. ~4.5x fewer steps, for a 4.7% wall-clock win on the
full chunk and 2.6 of the terrain stage's 47.3 ms per chunk.

So the whole reachable range of this lever was 2.6 to 6.0 ms/chunk: a
single-digit-percent lever, not the dominant cost. The rest of the terrain stage
is the per-block work around the density function: `cols()` and `feat_state()`
lookups per block, the material rule, the aquifer, vein sampling and
`deep_state_cached`. A further terrain-stage change should start there, not in
the evaluator. The earlier sentence "the chunk time is dominated by evaluating
the density-function DAG" was inferred from the visit count and does not survive
the wall clock.

### Cross-check against hyperchunk

`Soju06/hyperchunk` @ `387c9ad30ac1b4eddbcaad31a02e8864d5b394c1` (MIT),
`core/src/df_compile.c` and `core/include/hc_df.h`, read as prior art for this
change - **no code copied** (the reference clone sits outside the repository at
`/home/yuu/mc-spine-local/reference/hyperchunk-387c9ad/`). What it corroborates:

* the DAG is *flattened* into an index-addressed node table and walked by a
  loop, not recursed over (`hc_df_node_t` + `hc_df_eval`, scratch keyed by node
  index). This change is a stack machine over a post-order list instead; same
  idea, and the stack is what the task asked for.
* `add`/`mul` with a constant operand get their own opcodes (`HC_DF_ADD_CONST`
  / `HC_DF_MUL_CONST`), which is what `AddC`/`MulC` are here.
* markers (`cache_2d`, `cache_once`, `flat_cache`, `interpolated`) stay
  pass-through/marker ops. Its comment records that `cache_2d`/`cache_once` are
  value-neutral *for that router*, proven on the bytecode. This tree's argument
  is structural instead: `block_density` builds the memo per block and every
  reachable node is pure, so dropping the memo cannot change a value (see the
  section comment in `src/worldgen.rs`).
* spline knots/derivatives/values are `f32` and the coordinate is cast to
  `f32` - the same conclusion this tree's interpreter already encodes.

Where it goes further, and what is therefore left on the table here: it compiles
splines into a `float` knot table (`hc_df_spline_t`) and splits
`find_top_surface`'s input cone into y-invariant / y-variant nodes so the
invariant part is evaluated once per ladder. This change keeps both on the
interpreter (`Op::Call`) - they are 0.3% of the visits here, and the measured
ceiling left in the whole per-block evaluator is ~1.5 ms/chunk, so neither is
where the next win is.

### Counter semantics (changed by this)

`WG_COUNT=1` still prints `eval_c calls`, and what it counts changed:

* interpreted (`MC_SPINE_DF_INTERP=1`): density-function node visits, as before.
* compiled: only the visits that still go through `eval_c` - the corner samples
  that fill the `interpolated` grids, and the `Op::Call` subtrees (splines,
  `find_top_surface`, `old_blended_noise`, `end_islands`). The per-block spine
  runs as instructions and is **not** counted.

So it is a count of interpreter node visits, not a work counter, and the two
arms are not comparable by it. `WG_COUNT=1 MC_SPINE_DFSTATS=1` prints the
per-node histogram (`DF_HITS`) that attributes the remaining interpreted visits;
it is empty for the compiled part by construction.

## Parallelising the terrain stage (2026-11-18)

Lever 2, slice 1: the per-chunk **terrain** stage — density, material rules,
aquifers, surface/deep rules — runs on a pool of scratch `Gen`s, one chunk per
worker, and the serial stage consumes the completed section rows. Vanilla draws
the same line: `NoiseBasedChunkGenerator.fillFromNoise` is
`CompletableFuture.supplyAsync(..., background executor)` per chunk, gated by
`ChunkStep`'s neighbour-status requirements (`apy.a`, `dwc.a` in the pinned jar),
while `FEATURES` and `CARVERS` complete synchronously on the thread that popped
the task (`dwb.h`/`dwb.g` return an already-completed future).

The population dispatch, the promotion boundary, the `hasPostProcess` marks and
the feature order are untouched: `terrain_section` still asks `feat_state` first,
cell for cell, in the same order, and the dumps' bytes do not move.

### Identity, before speed

| check | result |
|---|---|
| 64-chunk block dump (`MC_SPINE_WG_DUMP_N=8`), full / `NOFEAT` / `NOFEAT`+`NOCARVE`, base commit `d12adb5` vs this tree | byte-identical (25,165,824 B each; sha256 `670fdfe3…` / `57101c43…` / `bb499dfb…`) |
| the same dump at seed 12345, from a negative-origin window, the Nether (seed 4242) and the End | byte-identical |
| `dimension_terrain` (End/Nether, seed 0 and -1), the seed-0 canonical gate (`overworld_chunk_0_0_1211`, 98,304 cells) | green |
| matrix-v1 nether 0/128 rows (`nat-4242-nether-fortress` (-25,6), `nat-4242-nether-fossil` (-22,20)) via `port_triage` | 0 / 32,768 each |
| mushroom window `(-25,19)+3x3`, recorded 81-pop order (`t6b/nat-nf-str`) | 0 / 294,912 (dump sha256 `97eaee35…`, the same dump the hasPostProcess page recorded) |
| full suite | 664 passed, 1 ignored (serial and `--test-threads=8`) |

The parallel path is proven to *execute* rather than assumed: `MC_SPINE_GEN_TRACE=1`
prints one line per round (`round chunks=10 filled=10 jobs=5 sections=240 fill_ms=57`),
and `terrain_prefetch_fills_more_than_one_chunk_at_a_time` holds a rendezvous that
releases only once two workers are inside a chunk fill (a serial dispatch makes it
red after a timeout instead of passing).

### Measurement

`MC_SPINE_GEN_JOBS` (`0`/`1` = inline, the escape hatch) else
`min(available_parallelism, 8)`; inside a `CPUQuota=550%` unit that reports 5.
64-chunk `MC_SPINE_WG_DUMP_N=8` dump, three interleaved runs per arm, medians,
`phase` is the binary's own `[wgdump] ... in <t>` (the generation loop only):

| workload | base | jobs=1 | jobs=5 | jobs=8 |
|---|---|---|---|---|
| terrain only (`NOFEAT`+`NOCARVE`) | 2.633 s / 41.1 ms·ch | 2.545 (1.03x) | **0.712 (3.70x)**, 11.1 ms·ch | **0.557 (4.73x)**, 8.7 ms·ch |
| `NOFEAT` (terrain + carvers) | 4.292 | 4.221 (1.02x) | 3.136 (1.37x) | 2.763 (1.55x) |
| full (features on) | 8.273 | 8.272 (1.00x) | 7.164 (1.15x) | 7.424 (1.11x) |

CPU (whole process, `getrusage`) and peak RSS, medians:

| workload | base | jobs=5 | jobs=8 |
|---|---|---|---|
| terrain only | 2.64 s / 75 MB | 2.98 s / 95 MB | 2.99 s / 105 MB |
| `NOFEAT` | 4.34 s / 84 MB | 5.45 s / 93 MB | 5.28 s / 105 MB |
| full | 8.32 s / 119 MB | 15.64 s / 185 MB | 20.85 s / 247 MB |

So: **the terrain stage is 3.7-4.7x faster in wall clock for +13% CPU**, and the
stages this slice deliberately leaves serial cap the rest — carvers (+18% of the
terrain arm's CPU) and, with features on, the worker-side `cols`/beardifier work
that the owner used to share (full: +88% CPU at 5 workers for a 1.15x wall win).
That CPU number is the honest cost of the split and the reason `MC_SPINE_GEN_JOBS=1`
exists. `jobs=8` is not better than `jobs=5` on the full arm: past 5 the serial
tail is what the batch waits on.

### What the split had to get right

* **`cols` is where the money and the traps are.** Terrain is not just the
  per-block loop: `cols()` (14.5 of the 44.7 ms) includes the beardifier
  resolution, which scans a 17x17 structure-start window. Doing that per worker
  looked free and was not — measured, the full dump went 8.36 s serial ->
  **23.05 s at 5 workers, 39.29 s at 2** because each worker recomputed the
  window for its own chunks.
* **The structure caches are per *thread*, not per `Gen`.** `STARTS` and
  `RING_CACHE` (`src/structure_pipeline.rs`) are `thread_local!`, and a round
  spawns fresh worker threads, so a naive split recomputes them per thread *per
  round*. They are now shared **per round** (`feature::starts_round_begin`): the
  cache is created before the workers spawn, the owner shares it too, and it is
  dropped when the round ends. Process-wide sharing was tried first and rejected
  by the suite: `dimension_terrain_matches_all_native_fixture_states` failed on
  the Nether seed -1 row (`(-994,89,538)`: 19460 vs 5849), because a start's value
  depends on the context it was first computed in (`FC` reads go through the
  feature layer, and a beardifier resolution sees preliminary columns). 281
  `chunk_starts` calls per run (the serial number) at every worker count is the
  gate that the sharing is neither duplicating nor losing work.
* **The feature layer reads terrain the terrain pass used to warm.**
  `deep_c`/`aq_lv`/`prelim_c` are per-`Gen` memos; a feature read against a cold
  owner memo re-runs the material/aquifer rules. `Gen::prefetched_cell` answers
  that read from the landed row instead.
* **The delivery path stays opt-in.** Its prefetch runs on the tick thread, and a
  round of chunks built while the tick waits is a *latency* change:
  `tick_tests::wall_clock_boundaries_are_gated_on_elapsed_time_not_tick_count`
  (whose assertion is "40 ticks cost microseconds against a 1 s period") left its
  boundary once the whole suite ran the delivery path on 8 workers. The offline
  dump is what this slice measures and it is the default; the server arm needs a
  tick-latency measurement, not a throughput one, before it flips.

### Gates

`terrain_prefetch_fills_more_than_one_chunk_at_a_time` (concurrency, plus every
chunk of a round claimed), `prefetched_terrain_rows_match_the_serial_fill_at_any_worker_count`
(bytes per section for jobs 1/2/8), `a_prefetched_section_is_identical_to_the_serial_section_with_features`
(the composed boundary, features included). Two mutation entries hold the
dispatch able to fail (`MUTATION_STABILITY=1`, both red/red/green): an off-by-one
in the worker's first claim, and a section consumed under the next section's key.
