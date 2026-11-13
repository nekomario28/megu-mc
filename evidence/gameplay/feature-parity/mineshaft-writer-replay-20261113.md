# Seed 4242: mineshaft writer replay, not zero-diff acceptance

Status: historical causal investigation checkpoint at `a205e2d56405edd99316616285e2bbd7e0b595a7`.
This writer-replay checkpoint made no Rust/runtime change, fresh HEAD build, or
production zero-diff result. Subsequent seed-derived construction is recorded in
[the layout checkpoint](mineshaft-layout-20261113.md), not retroactively added to
this scripted experiment.
The preceding [snowy/residual record](seed-4242-chunk-3-m2-20261113.md) remains
an exploratory comparison, not an acceptance fixture.

## Retained comparison

Replayed the retained `post-order-A.bin` against saved native A and
`post-order-B.bin` against B, preserving all 98,304 state cells and air kinds.
Both have the same 53 residual coordinates/states:

- 32 `cave_air -> stone`;
- 19 `cave_air -> air`;
- 2 `oak_planks -> air`.

All referenced starts were available; none was silently skipped. The saved start
at `(4,3)` has intersecting children 85 (corridor), 86 (stairs), and 185 (corridor).
Their NBT, orientations, bounds and flags agree across A/B. This reproduces old
artifacts, not a new dump from current HEAD. Population-order files are
counterfactual diagnostic inputs; original native generation order remains unknown.

## Actual native writer, with explicit synthetic conditions

Authenticated server jar SHA-1: `59353fb40c36d304f2035d51e7d6e6baa98dc05c`.
Read actual bytecode for `elv$a` (corridor), `elv$c` (base piece), `elv$e` (stairs)
and `ejv` (StructurePiece). Reconstructed the three pieces with their saved NBT
constructors, then invoked their actual `postProcess` methods in child order.

The level is a scripted diagnostic, not ServerLevel: reads come from the retained
port target; native saved target `MOTION_BLOCKING` values supply height queries.
The native direct-holder tag test is explicitly scripted nonblocking (not an
actual captured biome lookup). Worldgen RNG seeds 0 and 1 were fixed before
running, not searched to fit. Both runs have three biome gate calls, twelve height
calls and no unsupported calls. All block writes are recorded with child identity.
Unserved positions and unsupported native requests fail rather than defaulting.

An initial attempt to include neighbor context rejected `(4,-2)` as
`initialize_light`, not `FULL`; `(4,-1)` and `(4,0)` have that status too.
The guard was preserved. The successful replay instead uses only the target
scene and fails on an out-of-scene read; it does not relabel those neighbors FULL.

## Why the original residual set alone is not an acceptance gate

| Scripted RNG seed | Original 53 corrected | Original residuals left | New differences elsewhere | Full-volume residuals |
|---|---:|---:|---:|---:|
| 0 | 52 | 1 | 0 | 1 |
| 1 | 53 | 0 | 1 | 1 |

Seed 0 leaves `(62,24,-17)` as `air` rather than `cave_air`. Seed 1 writes an
extra `cave_air` at `(63,11,-18)`, where native has `stone`. Thus neither replay
is zero-diff, even though seed 1 fixes every coordinate on the original list.
Corridor ceiling RNG consumption is observable and cannot be replaced with
unconditional excavation or a selected fitting seed.

Native child 86 writes all 31 stair residual cells as `cave_air` in both runs.
Child 185 writes both support planks at y=21, below its nominal BB minimum y=22.
Child 85 issues 14/15 set events and child 185 issues 7/8 under seeds 0/1;
child 86 issues 31 in each. The positive native writes make the missing writer
concrete; they do not reconstruct the original generation-time execution.

## Guard and remaining implementation boundary

The local checker validates input/output hashes, replays ordered events into the
final writes map, verifies stair/plank activation, enforces the clip, and compares
the entire target volume. It rejects both deletion of the stair write events and
collapsing their cave-air states to air, then passes again on restored inputs.
These are diagnostic negative controls, not compiling production mutants.

An early scratch driver named `inspect.py` shadowed Python's standard library.
It was renamed; drivers now require `python3 -P` before loading other modules.
The missing-safe-path invocation is rejected with exit 2. Neither this harness
failure nor the rejected partial neighbor is counted as a successful native run.

At this HEAD, `feature::populate` drives carvers and placed features but has no
mineshaft start/piece dispatch. Existing structure-placement primitives and the
fossil template owner do not generate or place these procedural pieces. A
capture-NBT injection or a coordinate correction mask would bypass that missing
production contract and is not implemented.

Next discriminating work is to recover native start/layout construction and the
decoration RNG/index/piece-order contract before adding its production owner.
Piece placement must keep clipping, cave-air identity, randomized ceilings and
support writes outside nominal BBs. The promotion test must derive pieces from
seed/config, reach the real dispatcher, compare A/B separately over full volume,
and preserve the existing seed-0 fixture; then pre-change and compiling-mutant
checks plus the settled full gate are required. No new RNG/index default is chosen.

## Local evidence

`~/mc-spine-local/scratch/current/mineshaft-4242-20261113/` contains
`inspection.json`, `native-replay.json`, `replay-receipt.json`,
`verified-replay.json`, source drivers, actual bytecode text and `SHA256SUMS`.
The native run finished at 2026-11-13T06:35:48Z; the verifier has separate timestamps.
Jar, bytecode, native worlds and captured NBT remain local.
No full Rust/native regression gate, independent review, hosted CI, commit or
publication was performed for this checkpoint. M2/M3 and P2-1 remain open.
