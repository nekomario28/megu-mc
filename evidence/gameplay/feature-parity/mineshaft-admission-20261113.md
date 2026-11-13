# Mineshaft admission and decoration contract checkpoint

Source baseline: `94f3ca6bc6a7d5260729908958888ccd72f5aea6`, after publishing
the [NORMAL layout component](mineshaft-layout-20261113.md). This continuation
changed no Rust runtime code and produced no fresh port dump or zero-diff result.
It recovered the next native contracts and stopped the placement replay at a
missing stage-specific input rather than substituting a different heightmap.
The later [generation-stage capture and writer replay](mineshaft-stage-writer-20261113.md)
closes that input gap and obtains conditional whole-volume A/B agreement; it does
not connect production placement or retroactively change this checkpoint.

## Actual start admission, not saved-piece injection

The authenticated 1.21.1 jar is SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. A local probe loads its datapack with
native `RegistryDataLoader`, binds biome tags with native `TagLoader`, constructs
the native Overworld noise generator/biome source/RandomState, and invokes
`ChunkGenerator.createStructures` on real ProtoChunks.

The native HolderLookup is explicitly filtered to the real `mineshafts` structure
set. The 19 excluded structure sets are named in the output, not silently treated
as implemented or tested. Native NORMAL and Mesa biome memberships contain 49
and 3 entries respectively; an empty membership is a failing prerequisite, not a
negative admission result. No captured start or piece NBT enters this procedure.

Across six fixed integer seeds and 289 chunks per seed, 1,734 actual native
admission calls produce six valid starts, all NORMAL. This sample is not a Mesa
acceptance fixture. For seed 4242, `(4,3)` is admitted with 200 ordered pieces.
Its biome-test position is `(72,9,48)`, sampled at quart `(18,2,12)` through
`BiomeSource.getNoiseBiome`, yielding `snowy_plains`. Admission does **not** use
the saved chunk's surface biome or the blended `BiomeManager.getBiome` accessor.

The target's first native weighted draw selects entry 1 (Mesa), whereas the
accepted start is NORMAL. The authenticated `createStructures` body removes a
failed selection and retries the remaining weights. Its candidate-selection RNG
is distinct from each attempted structure's freshly seeded generation context.
Thus admitting NORMAL is not equivalent to selecting NORMAL first or silently
omitting Mesa. Biome validity is checked after `findGenerationPoint`, including
the selected type's vertical adjustment.

A compiling inverse control omits biome-tag binding and is rejected with
`unbound structure biome tag minecraft:mineshaft` before writing output. The
restored native run reproduces the complete admission JSON byte for byte.

## Decoration contracts recovered from native bodies

The authenticated `ChunkGenerator.applyBiomeDecoration` body groups the native
structure registry's encounter order by decoration step. This is a different
index space from the placed-feature topological order. At step 3 the native
registry yields `buried_treasure`, `mineshaft`, `mineshaft_mesa`, `trail_ruins`, and
`trial_chambers`: NORMAL mineshaft has **index 1**, not 0 even if no buried-treasure
start exists in the target.

The caller wraps **XoroshiroRandomSource** in WorldgenRandom for decoration;
layout/admission's legacy LCG is not its placement stream. Direct calls to the
native seed methods for seed 4242 and decoration block origin `(48,-32)` return
population seed `8215116175315958658`; native `setFeatureSeed` then applies index 1,
step 3. Non-consuming Xoroshiro state and typed draw prefixes are retained locally.
These are caller-derived contracts and direct native method results, **not a
capture of the original world's running dispatcher or RNG history**.

The caller reseeds once per structure type, iterates its referenced starts, and
`StructureStart.placeInChunk` visits intersecting pieces in their original list
order on that shared stream. It does not reseed every piece. Both retained A/B
targets reference only packed start `12884901892`, i.e. `(4,3)`. A general
multi-start reference iteration order has not been runtime-verified here.
The native writable Y range is `minBuildHeight + 1 .. maxBuildHeight - 1`, here
`-63..319`; the earlier scripted clip began at -64.

## Placement replay rejected: a different heightmap was being served

The new replay derives the start through the native admission path and calls
actual `StructureStart.placeInChunk`, with caller-derived decoration RNG and
procedural native biomes. It retains the explicit intervention of an old port
scene and saved native heights. No saved pieces are supplied in its input.

It stops before producing replay output:

```text
AssertionError: unexpected heightmap OCEAN_FLOOR_WG
    at ejv.b ...
    at elv$a.a ...
    at ejz.a ...
```

`StructurePiece.isInterior` (`ejv.b`) tests the transformed `y + 1` against
**OCEAN_FLOOR_WG**. The [historical scripted replay](mineshaft-writer-replay-20261113.md)
returned saved MOTION_BLOCKING for every height query without checking the
requested type. Its 52/53 and 53/53 original-cell corrections remain valid only
under that declared substitute-height condition; they are not results using the
native requested heightmap. Its `unsupported=0` counted unhandled calls, not
semantic agreement of heightmap inputs.

The target FULL captures contain OCEAN_FLOOR, MOTION_BLOCKING,
MOTION_BLOCKING_NO_LEAVES and WORLD_SURFACE, but not OCEAN_FLOOR_WG. Neither a saved
OCEAN_FLOOR value nor a freshly scanned post-feature floor is silently accepted
as the missing generation-stage value. The new probe checks the requested enum
before serving the map, and the actual native request has demonstrated that guard
failing. No new full-volume residual count or production parity claim follows
from this rejected replay.

## Verification state and next bounded work

Native admission, its compiling missing-tag control, restored admission, and the
direct native RNG probe ran in 1 GiB / CPUQuota 100% scopes. Java's effective heap
flag is `-Xmx768m`; `ActiveProcessorCount=1` limits incidental JVM parallelism.
There was no full regression, hosted CI, new independent review or server run.

The exact-source Rust RNG check extracts the current `feature::pop_seed` body
and copies `generation_math`/`trace_env`, rather than substituting a handwritten
population-seed formula. Twelve native vectors cover both mineshaft slots over
the six seeds, with eight float bit patterns and the following long per vector.
The isolated harness passes 10 tests, including existing math tests. A compiling
wrong-index diagnostic control fails the native float prefix; the restored
control passes. This checks math and the extracted current helper, not production
structure-dispatch reachability.

Its first compile found two omitted transitive fixture files, not a parity
failure. The snapshot builder now copies the module's `include_str!` inputs. A
retry was not launched during resource gate HOLD at load1 28.18 (cap 24), CPU PSI
36.0. After documentation work, a fresh gate returned GO; only the Rust part ran,
finishing at 2026-11-13T08:56:00Z. Native source/output identities were checked
against the retained completed phase instead of paying for those probes again.

At this checkpoint, the next step was a stage-authentic OCEAN_FLOOR_WG source
and resumed named writer check. That diagnostic step is now recorded separately
in the [stage/writer follow-up](mineshaft-stage-writer-20261113.md). Admission integration must preserve Mesa-first rejection
and weighted retry; the layout component alone cannot implement that policy.
Production dispatcher reachability, block placement, fresh-port exact-volume A/B
comparison, meaningful pre-change failure and settled regression gates remain open. The
historical 53-cell checkpoint is not changed by this investigation.

## Retained local evidence

`~/mc-spine-local/scratch/current/mineshaft-dispatch-20261113/` contains:

- `pipeline-receipt.json`, `pipeline.json`, `admission-source.java`: the completed
  native admission run and the exact source version bound by its receipt;
- `pipeline-restored.json`, `missing-biome-tags.log`, `native-phase-receipt.json`:
  native inverse/restored controls; the phase receipt was reconstructed from the
  observed successful subphase and stamped artifacts after the Rust fixture
  failure, and is explicitly not a second native run;
- `seeding.json`, `seeding.tsv`, `MineshaftSeedProbe.java`: direct native seeds,
  weighted indices, and typed prefixes;
- `seeded-failed-source.java`, `seeded-failure.log`, `seeded-input-A.json`:
  the stricter placement probe and its actual rejected heightmap request;
- `verify_contract.py`, `contract-verification.json`, `rust-contract/` and
  normal/wrong-index/restored compile/test logs: isolated Rust checks and their
  exact-source identities; the initial missing-fixture failure is in the session
  command record, not the subsequently refreshed normal compile log;
- native bytecode listings, source hashes and `SHA256SUMS`.

Jars, bytecode, native worlds, captured NBT and replay scenes stay local. The
Rust layout and methodology commits were already pushed as `788a2ac` and
`94f3ca6`; this follow-up checkpoint is a separate, uncommitted continuation.
