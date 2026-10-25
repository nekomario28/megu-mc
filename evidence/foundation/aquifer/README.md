# Pinned aquifer / preliminary surface seam

Target authority is Minecraft 1.21.1 / protocol 767, server SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. Horizon/Valence versions are not
specifications for this seam. No dependency, server/world startup, GPU or donor
execution is introduced.

## What changed and why

The old `aquifer_vectors` fixture came from Pumpkin's `aquifer_sampler.rs` test
and used a donor `GlobalRandomConfig`; several of its queries (for example
`x=-100`, `y=-100`) lie outside the chunk-scoped vanilla aquifer and cannot be
asked of a real `NoiseBasedAquifer`. Those vectors are retained only as donor
regression pins. The native reference is `vanilla.json`, produced by
`tools/aquifer_oracle.py`:

- the authenticated server jar's own `data/minecraft/worldgen` is loaded by
  `RegistryDataLoader` through the real codecs;
- an ordinary `ProtoChunk` and `NoiseChunk.forChunk` construct the aquifer;
- `preliminarySurfaceLevel` and `Aquifer.computeSubstance` are called through
  their public API. No world, server, `Unsafe` allocation or constructor bypass.

The native comparison exposed four independent defects:

1. **Preliminary surface.** `NoiseChunk.computePreliminarySurfaceLevel` keys by
   `QuartPos` (`x >> 2`), not `SectionPos`, scans
   `initial_density_without_jaggedness` from `minY + height` down to `minY` by
   the noise cell height, and takes the first value **above 0.390625**; no match
   is `Integer.MAX_VALUE`. The old loader used the converted
   `find_top_surface` node and a `> 0.0` threshold. The native router field now
   takes precedence; converted `aquifers.surface_level` remains a legacy
   fallback.
2. **Cubic spline arithmetic.** `CubicSpline` stores locations, derivatives and
   constants as **f32**, casts the coordinate to f32, and evaluates with float
   `Mth.lerp`. The old f64 Hermite formula produced different density bits; at
   `(64, 56, 32)` the native value is `0.0248…` while a neighbouring cell is
   above the surface threshold. The float form is now used globally, not only
   by the aquifer.
3. **Deep aquifer test.** 1.21.1 uses
   `OverworldBiomeBuilder.isDeepDark`: `erosion < -0.225F && depth > 0.9F`.
   `NoiseRouter.h()/i()` are the erosion/depth accessors; the similarly named
   fields are continents/erosion/depth, so field-name reading selects the wrong pair.
   The old code evaluated a synthetic `exclusion` node, which 1.21.1 does not
   use. The float literals are promoted to f64 (their f32 values, not `-0.225`
   and `0.9` as f64). A converted pack's `exclusion` is ignored.
4. **Donor skip and NaN range.** Pumpkin's skip-above optimisation is not in
   vanilla and was removed. The pressure-range test keeps the native
   `!(q < -2.0) && !(q > 2.0)` result for NaN via `q.is_nan() || (-2.0..=2.0)`.

## Reference and scope

`vanilla.json` holds 64 preliminary-surface columns, 150 chunk-local fluid
states (air, water, lava and barrier/solid), and 3,136 raw
`initial_density_without_jaggedness` values at every quart column the surface
probe reads. Seed 0, chunk (7,4), densities `0.0`, `-1.0`, `+1.0`. The Java probe
also records the server SHA-1, nested-game SHA-256, source SHA-256 and datapack
description; Python requires complete request/result/provenance equality, raw
16-hex density bits, unique prelim columns, all four fluid outcomes, exact row
counts and the pinned oracle hash.

`aquifer_vectors` still passes as a donor internal-method regression set. It is
not native evidence: its coordinates include invalid world positions and its
expected values came from a donor generator configuration. Native evidence is
`aquifer_matches_the_pinned_chunk_through_its_public_api` plus the synthetic
deep-predicate wiring test; neither proves a whole world.

## Verification

| Gate | Result |
| --- | --- |
| Native corpus replay, twice | 64 prelim + 150 state rows exactly equal; 3,136 density rows equal |
| Focused Rust | 3 passed (`aquifer_`, native + deep + donor vectors) |
| Selected canonical mutations | 6 proved; 0 not-a-gate/parked/skipped |
| fmt / all-target warnings-denied clippy | Passed |
| Layers / debt / wire | 19 modules, 44 edges, 0 named errors; 0 over limit; container and gameplay wire PASS |
| Full serial release | **213 passed / 17 failed / 1 ignored**, exit 101; 10 failures contain `PoisonError` |
| Independent static review | Found the deep-field mapping error; corrected to erosion/depth, rechecked by the six mutations and a follow-up static review (`review.txt`, `review-followup.txt`) |

The baseline was 205/23/1. Six failures now pass and no new failure was added:
`heightmap_reads_the_floor_the_blocks_are_at_not_the_noise_surface`,
`aquifer_vectors`, `biome_vectors`, `bundled_carver_configs_are_the_1_21_1_shapes`,
`feat_determinism`, `feat_populate_perf`. The last three had been downstream of
the shared mutex poison; their passing is not a claim that every feature capture
is green. The generic comparator still reports `unchanged_failure_baseline:
false`, and the scoped check asserts the exact six removals and the 213/17/1
counts instead of weakening that rule. No test or capture was re-recorded.

Remaining primary failures include the dripstone/root feature fixtures,
`feature_population_output_matches_the_pre_refactor_capture`, lazy population,
material rules, surface density, the ore-vein capture and vertical-gradient
seeds. They are outside this seam.

## Limits and failed attempts

- One seed, one chunk, one density set. There is no native deep-dark chunk in
  the corpus; the deep predicate is pinned by a synthetic unit that swaps
  router constants, not by a real chunk. Full surface evaluation, biome
  carver/spawner dialects, arbitrary seeds/chunks, whole-world chunks and
  performance parity remain unverified.
- The float-spline change is global and changes the converted/reference density
  path too. It is required for exact native bits, but the old feature captures
  that were already red are not repaired by it.
- The first native assumption used `SectionPos` alignment and failed at
  `(64,47)`; the decompiled `SectionPos`/`QuartPos` distinction corrected it. The
  first spline mutation was algebraically identical after IEEE negation and
  stayed green; it was replaced by a genuinely different `Mth.lerp` order.
  The first full-check unit lost its final marker from journald despite a
  successful run; the final unit reran the same checks with an
  invocation-bound marker, and only that unit is cited in `run.json`.
- Full `tests/verify.sh`, the whole mutation campaign and hosted CI were not run
  for this slice. Hosted checks on the parallel PR line were not started because
  of account billing, not test results.

See [the closed rationale](../../../specs/done/aquifer-native/README.md) and
`run.json` for source/binary/unit identities.
