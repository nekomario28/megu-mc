# Mineshaft: generation-stage height capture and exact conditional writer replay

Baseline: `94f3ca6bc6a7d5260729908958888ccd72f5aea6`.
This closes the missing-input investigation in the
[admission checkpoint](mineshaft-admission-20261113.md), **not production mineshaft
acceptance**. No Rust runtime, dispatcher, test expectation or retained capture
was changed at this checkpoint; the NORMAL layout component then had no production
placement caller. The later [Rust connection checkpoint](mineshaft-production-20261113.md)
records a real production caller, new regression controls and fresh whole-volume
comparisons separately, without changing this conditional replay's scope.

## What was actually captured

The authenticated 1.21.1 server jar has SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. Its native bodies establish:

- `ProtoChunk.setBlockState` updates the heightmap types selected by its current
  persisted status. Through SURFACE this includes OCEAN_FLOOR_WG/WORLD_SURFACE_WG;
  CARVERS and later select the four non-WG maps instead. WG does not mean a floor
  freshly rescanned after structures/features.
- `WorldGenRegion.getHeight` delegates to the requested chunk/type and adds one
  to `ChunkAccess.getHeight`'s highest-taken Y. The captured world getter returns
  **absolute first-available Y**, not a packed value relative to min-Y.
- `StructurePiece.isInterior` compares transformed `y + 1` with OCEAN_FLOOR_WG.

A new stock Overworld fixture, seed 4242, structures enabled, ran with view and
simulation distance 2. The server ran from 09:24:17 to 09:24:50 UTC on 2026-11-13.
It used a 1,800 MiB scope / CPUQuota 100%, heap 1 GiB and
ActiveProcessorCount=1. Both post-startup game-time queries returned 2;
the fixture's boxed runtime-write count was zero. This does not establish that no
startup tick ran. Neither the retained A/B worlds nor their settings were edited.

A local logging-only agent observes the actual decoration caller and
`StructureStart.placeInChunk`, rather than invoking that method itself. Hooks are
thread-local and use an independent ordered writer. For target `(3,-2)`:

- one start `(4,3)`, 200 ordered pieces; intersections 85, 86, 185;
- native clip `[48,-63,-32]..[63,319,-17]`;
- before/after chunk status `minecraft:carvers`, WG map already present;
- all 256 columns read through the actual WorldGenRegion getter, with the
  chunk's highest-taken Y separately checked as getter value minus one;
- **12 native writer height queries**, all OCEAN_FLOOR_WG, agreeing with the
  snapshot; observer-generated snapshot reads are excluded from that count;
- **53 ordered native set events**, all successful, producing 53 changed cells.

Snapshots preserve the Xoroshiro state. Applying the ordered write events to the
entire 98,304-cell before-image exactly reproduces the after-image; the checker
rejects unaccounted changes rather than assuming snapshots were atomic. The WG
map is identical before and after. The generator's source and native class
bodies, agent sources/jars, settings, raw snapshots and ordered trace are bound
by checksums. No production behavior branches or RNG calls are injected by the
agent; coordinates select observations, not corrections.

The actual incoming Xoroshiro `(lo,hi)` is
`(-3401251364168619252, 6696036118855211742)`, matching the earlier caller-derived
index-1 / step-3 contract. The actual outgoing state is
`(-1629721374336782616, -6208513398033862939)`. This is a fresh dispatch observation,
not a reconstruction of the retained A/B worlds' original history.

## Writer replay and the full-volume result

The replay derives a native start through actual `createStructures`, with native
biome tags, then invokes actual `StructureStart.placeInChunk`. It supplies the
captured generation-stage WG map with explicit type/stage/units and procedural
native biomes. It supplies **no saved pieces**, and does not search RNG seeds.
Unserved scene/height reads and unsupported calls reject the run.

First, the captured native before-image is the scene. The replay matches all 53
ordered live-native events, all 12 height queries and the outgoing RNG exactly.
Applying its writes matches the captured native after-image over all 98,304 cells.
The three biome calls resolve to snowy_plains. Restoring after a compiling
wrong-enum control reproduces the complete replay JSON byte for byte.

Then the retained historical `post-order-A.bin` and `post-order-B.bin` are used
as separate initial volumes and compared to their original native A/B references.
Both have 53 initial differences and **zero final differences over 98,304 cells**,
with no new mismatch. Their writer scenes are byte-identical, so B reuses A's
native replay output; B's distinct complete volume/reference is still checked
independently. Both also match the live-native event sequence and final RNG.

This is a conditional downstream result: historical port scenes and a captured
reference heightmap are explicit interventions. It is not a fresh port dump, a
Rust writer, original-population-order recovery or an acceptance-fixture promotion.
Air subtypes/properties are preserved; there is no coordinate repair mask.

The captured WG values happen to equal all 256 saved MOTION_BLOCKING and
OCEAN_FLOOR values in both retained targets. Recovering WG fixed **input authority**,
not a numerical height error in this particular chunk. The actual native RNG
stream, not fitted seeds 0/1, supplies the exact randomized ceilings. The earlier
[scripted seed-0/1 experiment](mineshaft-writer-replay-20261113.md) remains unchanged.

The new fixture's final FULL chunk differs from retained A at 30 cells and B at
53 cells. It is not substituted for either reference, and those differences are
not masked or called an acceptable noise floor. The zero-diff results above use
each retained reference, not the new fixture's final FULL state.

## Guards actually observed failing

Native lifecycle/observer canaries, normal and restored:

- At SURFACE, placing stone at y=40 gives WG first-available 41. At CARVERS,
  placing stone at y=80 leaves WG 41 while live OCEAN_FLOOR becomes 81.
- A compiling control that retains SURFACE for the second write fails
  `WG_FROZEN_STAGE_EXPECTED_41_LIVE_81`.
- A compiling observer that draws a long fails `WG_OBSERVER_CONSUMED_RNG`.

The capture checker rejects missing native queries, wrong query type, missing
writes and missing hook wiring, then accepts restored input. The native replay
rejects a declared MOTION_BLOCKING contract, a missing `(63,-18)` height entry
(naming that missing key), and a compiling handler that accepts the old enum.
These are diagnostic guards, not production Rust mutation proofs or hosted CI.

Initial unsigned-default-package helper loading failed with a signer mismatch
against the signed game classes. Helpers now use their own namespace; preflight
rejects the old source with `LOG_HELPER_PACKAGE_COLLISION`. A scratch helper import
also failed under `python3 -P` before native replay was launched. The driver now
loads that one file explicitly with `importlib`, without adding scratch to
`sys.path`. Failed sources/logs stay local. Neither failure counts as native success.

## Evidence and next boundary

Local artifacts: `~/mc-spine-local/scratch/current/mineshaft-wg-height-20261113/`.

- `preflight.json`: native lifecycle/observer positive, compiling inverse and
  restored controls; 09:23:29.975659–09:23:56.650295 UTC.
- `native-fixture/`: original fixture script derivative, effective settings,
  generation order, run log, final world and its boundary checks.
- `native-stage/`: independent ordered trace and big-endian 32-bit before/after
  palette indices; `captured-stage.json` preserves height-query identity/units.
- `verify_capture.py`, `capture-verification.json`: complete-frame attribution
  and broken-shape controls, 09:27:08.885508–09:27:08.958077 UTC.
- `MineshaftPipelineProbe.java`, `run_replay.py`, inputs/outputs, inverse-control
  sources/logs and `writer-replay-verification.json`: successful resumed replay,
  09:31:07.304375–09:31:48.495340 UTC, in a 1 GiB / CPUQuota 100% scope.
  `run_probe`'s effective heap is 768 MiB.

Captured-stage SHA-256:
`929bcf21457629e7f7d0e80bae31fca4e5ad13af13c804341ac824c4c91cc30d`.
Native-before and A/B replay-output SHA-256:
`1724d55e7a46bf8eafb724d8ee7245be3c5e8af5d4c35718affc6371ae150b96`.
Jars, bytecode, worlds, NBT and diagnostic scenes remain local.

Next is production admission/weighted retry, decoration dispatch and procedural
placement, deriving WG state from the port's own generation stages. It must keep
one RNG/state owner, native clipping, cave_air and out-of-BB supports, and prove
real dispatcher reachability with pre-change failure and compiling production
mutants. Fresh port A/B whole-volume checks and the settled full regression gate
remain required. No full Cargo gate, independent review, hosted CI, commit or
push was performed in this continuation. M2/M3 and P2-1 remain open.
