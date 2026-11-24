# Rust mineshaft connection: block/RNG checkpoint

Baseline HEAD: `94f3ca6bc6a7d5260729908958888ccd72f5aea6`, branch
`perf/worldgen-parallel`. This is an **uncommitted implementation checkpoint**,
not a release, M2/M3 acceptance, or complete mineshaft entity support.
The [connection contract](../../../specs/mineshaft-dispatch/README.md) owns the
boundaries; the earlier [native stage checkpoint](mineshaft-stage-writer-20261113.md)
remains conditional replay evidence, not retroactive Rust validation.

This document retains the **earlier block/RNG checkpoint** and its original
source hashes/counts. See the [subsequent entity/save checkpoint](mineshaft-entities-20261113.md)
for current payload persistence, natural Mesa and multiple-start coverage.

## Connected path

Ordinary `feature::populate` now discovers procedural mineshaft starts and
places referenced pieces at the structure step before placed features. Layout,
placement primitives, noise/blended biome lookup, FC writes, state transforms
and registry predicates retain their existing owners. Production reads no saved
pieces, captured heights, reference scenes or coordinate corrections.

Admission uses weighted rejection/retry and fresh layout contexts. Decoration
uses registry slots, independently of selector order or absent starts, and one
Xoroshiro stream per type across referenced starts/pieces. The writer implements
room, corridor, crossing and stairs, including floors below nominal piece bounds.
Native FULL-face/CENTER-down/falling predicates are name/property-translated data,
not substitutes based on solid rendering or full collision.

Review also exposed an empty-feature-order early return that would have suppressed
structures in a legitimate datapack. The real executable is now tested with an
overlay removing all placed biome features: the mineshaft floors remain present,
SPASS executes and no SATTEMPT executes. This is not `MC_SPINE_NOFEAT`, which
intentionally disables the entire diagnostic decoration layer.

## Native and Rust checks actually executed

The new pipeline oracle invokes actual native `createStructures`, layout and
piece/start methods. Its corpus contains six admission grids (1,734 trials), six
accepted NORMAL starts, six Mesa adjustment vectors, native reference-container
orders and 18 authored writer scenes. Three Mesa admission cases use an explicitly
labelled native FixedBiomeSource badlands intervention; they do **not** establish
natural Overworld Mesa locations. Authored terrain distinguishes frozen WG from
live height and exercises floor gaps, suspended supports and piece variants.

- Nine focused Rust mineshaft tests passed. They compare ordered events and RNG
  states, admission/biome selection, Mesa heights, reference order, registry order,
  native support predicates and real populate reachability.
- The **default-feature Rust suite** passed: **421 passed, 1 ignored**. Its serial
  test execution took 103.17 seconds. This is not the complete `tests/verify.sh`
  promotion gate: its repeated eight-thread rounds, wire suite and full unrelated
  mutation campaign were not rerun.
- All-target/all-feature Cargo check passed. The pinned source-bound layer gate
  observed 38 modules, 99 edges and zero errors, with accepted findings empty.
- **13 compiling mutants** were rejected, each with two failing executions and
  rebuilt restored controls: eleven mineshaft contracts, the other shared fastutil
  consumer in scheduled ticks, and the real empty-feature executable case.
- Replacing only the dispatcher-bearing `feature.rs` with the baseline version
  made the production test fail on `minecraft:air` versus `minecraft:oak_planks`.
  This was an assertion failure, not a missing-input or compile failure. Restoring
  current bytes and rebuilding passed all nine focused tests.
- Native default/check modes matched without rewriting references. Missing native
  biome tags rejected with `UNBOUND_MINESHAFT_BIOME_TAG minecraft:mineshaft` and no
  output. Wrong JSON reference and wrong support-byte controls rejected with their
  named comparison failures; neither rewrote the reference, and both restored
  checks passed. The support table covers 26,684 states. Its absent moving-piston
  block entity is an explicit authored getter input, not runtime piston coverage.

### A false-green gate was repaired, not counted as proof

The first dispatcher test checked ordered blocks only. A compiling mutant that
reseeded before every piece could still produce those same blocks in the authored
target scenes. The test now observes outgoing RNG **after the actual dispatcher**,
non-consumingly and only under cfg(test), and compares it to native state. The
same mutant then failed twice and the rebuilt restored case passed. The earlier
10-proved/1-survived receipt is retained separately; it is not the final campaign.

A separate snapshot preparation failure omitted compile-time evidence fixtures:
13 compiler errors referred to 12 distinct files, and the runner repeated the
invalid baseline eleven times. The local snapshot producer now includes those
inputs and validates literal include paths before Cargo. Removing one included
fixture deliberately rejects at `SNAPSHOT_INCLUDE_MISSING`, then restoration
passes. Source edits and Cargo output were isolated from the primary checkout.

## Fresh ordinary generation, not replay

A newly rebuilt normal executable generated seed 4242, chunk `(3,-2)`, using the
default population order. Trace selection affected logging only. It reached
NORMAL start `(4,3)`, 200 procedural pieces, and emitted exactly the **53 ordered
writes** in the independently captured live-native writer window. Captured data
was read only afterwards by the comparator; it was never supplied to generation.

Every comparison below checks all **98,304 cells**, preserving all air kinds and
state properties, and requires the native target to be FULL:

| Reference, kept distinct | Exact differing cells |
|---|---:|
| Fresh native world from the live caller capture, LastUpdate 2 | **0** |
| Original retained native A, LastUpdate 1 | **30** |
| Original retained native B, LastUpdate 1 | **53** |

Against A the remaining pairs are 30 diorite→granite cells. Against B those same
30 plus 23 andesite→diorite cells remain. Neither comparison has an air/plank
residual. A and B themselves differ at 23 cells. The fresh native world already
differs from those originals in the same way; it is an additional reference,
**not a replacement for A/B or a reconstruction of their population history**.
The causal writer/history of those material differences remains a separate task.

Port dump SHA-256:
`7a38d6cb9e1ec54d2535a34a30bdb4e7c4bd3879cf59cf488328e1a611bbc5e7`.
This one fresh complete-chunk match does not establish multi-seed/biome parity,
general runtime multi-start ordering, natural Mesa coverage or entity/save support.
The target has no block entities, so its zero difference cannot prove those paths.

## Source binding and retained receipts

Local artifact root:
`<local>/scratch/current/mineshaft-production-20261113/`.
The controls bind a working-tree source/input manifest, not HEAD alone; no commit
or push occurred. Later documentation-only edits do not rewrite earlier receipts.

| Artifact | SHA-256 |
|---|---|
| `tests/data/mineshaft_pipeline_1211.json` | `c42d98c97a456443edfd1689ce05e8c39108777fed3fb1f52302b4254292e79c` |
| `assets/mineshaft_support_1211.bin` | `696f7048a675996506a14df6afea45167f152e5f1a6dbe4703e4db32f8b97fdc` |
| local `mutation-inputs.json` | `378fe5e2edac4f0e5905e01af3105aaef90d8a743a078ea56901e9aad3570164` |
| local `mutation-verification.json` | `fe25d8c6f5e3dded18c70a547db6d8f61123813f3de9208fb98da4c01ec5c3b7` |
| local `settled-verification.json` | `232dae925eead39aa50616a1c1aaa336bcf97ebf5917bc298f27bfcc9c03fdcb` |
| local `oracle-controls.json` | `3355862fd4ec348d8df5369b7950b8b25b9a1c394e53a967da9a8d289b200bba` |

The retained raw logs include the failed snapshot, weak RNG gate, repaired
campaign, pre-change/restored checks, oracle controls and all three whole-volume
comparisons. The build-manifest continuation separates JSON stdout from compiler
stderr rather than treating mixed logs as Cargo artifacts. Resumed verification
phases are labelled as such in their receipts; a continuation stamp is not the
start time of the earlier mutation/full-suite run.

Checks ran in named CPUQuota 100% scopes, MemoryMax 1,800 MiB for Rust/native
checks and 512 MiB for the additional comparisons, after fresh light resource
GO decisions. The Java helper uses `-Xmx768m`. Other host jobs were not stopped.
No performance gain, GPU execution, hosted CI or independent-review approval is
inferred from these local results. One current read-only Codex review was attempted
with a 90-second bound; model/Responses connections failed and timeout returned
124. `review-current.log` is an unavailable-review receipt, not a completed review
or approval. The implementing agent's shape/diff/document review is separate.

## Still open

At this earlier checkpoint, one-shot spawner flags lived for the Gen session, without a bounded
start cache or persisted structure state. Chest-minecart entities, spawner NBT,
runtime/save handoff and restart semantics are not implemented. Natural Mesa,
actual-runtime multiple-start traversal/lifetime and broader regression/promotion
remain separate gates. Original A/B history, asset redistribution/provenance,
P2-1, M2 and M3 are not closed by this checkpoint.
