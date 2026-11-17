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
| parallel chunk generation | not attempted: the generator holds one global `GEN` mutex, so this needs `Gen` per worker (the structure is there - all caches are per-`Gen` - but it is a larger change) |

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
