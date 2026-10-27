# Ore air exposure — a certain discard is not an unconditional rejection

Historical ore isolation: the native ore contract below remains valid, but its
then-current residual counts are superseded by [feature-contracts.md](feature-contracts.md).

## Reference contract

Pinned Java 1.21.1 `OreFeature.canPlaceOre` (`ecz.a`) first tests the replacement
rule, then calls `shouldSkipAirCheck(RandomSource,float)`. That helper returns
true at chance <= 0, false at chance >= 1, and otherwise draws one float.
**False means inspect the six neighbours**: placement succeeds when none is air.
It does not mean reject the ore. Both endpoints consume no exposure RNG draw;
a rejected replacement rule also consumes none.

The port returned false immediately at chance=1. This suppressed buried diamond
and lapis configured features even though their origins matched the reference.
The existing shared predicate now follows the helper's control flow; both normal
and scattered ore use it. No geometry or BitSet change is included.

`tools/ore_air_oracle.py` invokes the actual pinned class, not a translated
predicate. Its synthetic lookup supplies stone or air around a stone replacement
candidate; there is no server startup. The committed vectors cover three seeds,
chances 0 / 0.5 / 1, enclosed plus six exposed faces, and matching/nonmatching
replacement states. Each row records the result and the following raw RNG long.
The Rust test calls the production predicate through `FC` over the same inputs.

```sh
python3 tools/ore_air_oracle.py --jar /path/to/pinned/server.jar
cargo test --offline --locked --release -j1 ore_air_exposure_matches_native_decisions_and_rng
MUTATION_RELEASE=1 CARGO_BUILD_JOBS=1 MUTATION_FILTER=ore_air_exposure \
  bash tools/mutation_check.sh --offline
```

The native jar is authenticated by the existing `vanilla_java.pinned_runtime`.
`--write` deliberately regenerates the vectors; normal replay compares them.
The old implementation fails on an enclosed chance=1 candidate, before any
snapshot update ([old-code-red](ore-air/old-code-red.txt)). The mutation entry
restores that defect and proves a failing assertion between passing controls.

## Same-reference world result

Seed 0, structures off, canonical requested forceload/dump region
`(-64,32)` through `(-62,34)`; target `(-64,32)`. Reused saved vanilla world has
only the vanilla datapack and all nine chunks at `minecraft:full`. The port uses
its committed own-jar datapack and the same dump/population path as the baseline.
This preserves the existing fixture contract; it does **not** prove that the
port's implicit decoration margin or population order already equals vanilla's.
Hashes, timestamps, reference identity and command inputs are in
[receipt.json](ore-air/receipt.json). The dump executable was selected from
Cargo's build manifest, not an assumed output filename.

| Comparison | Before | After |
| --- | ---: | ---: |
| Region, `--tick` absent | 1,680 | **1,447** |
| Target, `--tick` absent | 53 | **30** |
| Region, `--tick` present | 1,789 (previous checkpoint) | **1,554** |
| Target, `--tick` present | 114 (previous checkpoint) | **91** |

The baseline no-tick dump was re-compared to the same saved world. All 23 changed
target cells now match: 10 become deepslate diamond ore, 7 lapis ore and 6
deepslate lapis ore. The 126-row native replay, old-code failure and changed
world cells establish that the corrected path actually executes.

### Remaining target cells

| Pair (port → reference) | Cells | Status |
| --- | ---: | --- |
| air → lava | 20 | saved-world flow, separate runtime artifact |
| andesite → coal ore | 6 | unresolved feature origin/shape/overwrite |
| stone → coal ore | 1 | unresolved feature origin/shape/overwrite |
| chest → chest (state properties) | 1 | unresolved dungeon orientation |
| chest → cave air | 1 | extra dungeon chest, unresolved placement/RNG |
| air → short grass | 1 | unresolved patch/predicate/margin |

`--tick` absent excludes leaf-distance changes but **does not exclude these
lava-flow cells**. Thus the raw count is 30, of which 10 are not attributed to
runtime. This is not zero worldgen residual. `--tick` adds 61 target leaf cells.
The older shorthand “chest 1” missed the extra chest: there are two chest-related
cells, not one. The fresh [cell report](ore-air/no-tick.txt) names both.

The existing diagnostic also prints `unsupported configured_carver
minecraft:nether_cave`; this run is overworld only. That known unsupported type
is recorded in [dump.log](ore-air/dump.log), not silently counted as supported.
The prior BitSet-capacity hypothesis remains unproven. Start the next ore pass
with the seven coal cells and include origins in neighbouring chunks: target
origin equality alone does not establish the identity of cross-border writers.

## Verification and boundary

- Serial and 8-thread release suites: **248 passed / 0 failed / 1 ignored**.
- Native replay: **126 rows match**; mutation: **1 proved, 0 skipped**.
- Formatting, release all-target clippy (`-D warnings`), layer and debt gates pass.
- Layers: 20 modules / 47 edges / 0 named errors. Wire smoke passes.
- Independent local Codex [review](ore-air/review.txt): no actionable defects;
  static review only, with execution evidence recorded separately.
- The population digest changed from `95fa4397cc1e760f0a0454b06cb502cb` to
  `4585793ccde64dbdb7856597bb844ebc` only after native predicate evidence and
  the world-cell improvement; it is a regression pin, not a parity oracle.

Logs are under [ore-air/](ore-air/). Full `tests/verify.sh`, every mutation entry,
adapter checks, fresh vanilla generation, whole-world parity and hosted CI were
not run for this slice. The local work does not close M2/R1 or authorize a merge.
