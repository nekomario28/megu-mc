# Tick parallelism status — H9/H10/H11

## What moved off the tick thread

`src/tick_workers.rs` owns a small `std::thread` pool (no `MeguTaskPool` — this
path must exist in every build). `live_mob_tick_dimension` now calls
`tick_workers::mob_plans` once per dimension, before its ordered mob loop, and
each worker computes a `MobPlan` for one snapped mob containing only pure
read-only probes:

- `MobEnv`: feet block state, `in_water`/`in_lava` classification (OW fluid
  helpers for the Overworld, `BReg` fluid checks elsewhere), the drowning
  eye-submerged scan, the suffocating-cell solidity scan, `height_at`/`block_at`
  sky-visibility reads for sun burning, and the block under a sheep.
- `PetPlan` (etype 15|122 only): the `(owner, sitting)` read from
  `live_mob_states` and the `pet_follow_teleport_step` floor search.
- `MobTargeting`: nearest non-spectator player, attackable pick under the
  per-etype hostile list, the spider/drowned unprovoked daylight rules, the
  enderman stare scan (live player pose + helmet slot + LOS ray), the target
  LOS ray, and the tempting-item look pick.

Workers read only snapshots (`mobs_snap`, a `TargetPlayer` copy of the
`LiveMobPlayer` slice) plus the shared read-only world/block-state caches and a
few live client fields (spectator flag, enderman-stare pose/helmet). No worker
writes to entities, blocks, RNG, or the network.

## What stayed on the tick thread

Everything that mutates or communicates: entity push/cramming, pet teleport
application, fluid travel application, all damage/heal/death paths, fire/freeze
ticks, behavior steps (`live_mob_behavior_step` — it mutates `live_mob_states`
and potion state), hostile AI families (`w1_hostile_mob_tick`), the crowd-gate,
breeding, explosions, `LIVE_MOB_AI` profile mutation + `tick_mob_ai_step`
(the profile is persistent mutable state and is not shareable), `live_mob_move`,
melee/bow attacks, all `set_block`/`note_block_update` writes, every broadcast,
and every `world_rng::next_u64` draw. RNG draw order is therefore byte-for-byte
serial — the regression test pins `world_rng::saved_state()` across modes.

## Commit order and validity

The mob loop still iterates `mobs_snap` in order and consumes each plan
in place: `plans.take_valid(i, pos)` waits for slot `i` (worker results are
stored per-slot, never reordered) and returns the plan only while it is still
true:

- the mob's position still equals the probe position,
- `CHUNK_VERSION_SEQ` and the generation seq are unchanged since dispatch —
  so no `set_block`/generation write landed mid-batch and invalidated the
  world reads,
- the recorded `provoked` flag, spectator flags, and enderman-stare reads
  (live pose + helmet) still match at commit.

Post-travel fields (`submerged`, `cell_solid`) are re-verified per use against
the position at that point. Any staleness, any missing/failed slot, or the
whole batch being lost falls back to the *identical inline expressions* the
serial code ran — `env_probe`/`compute_targeting` are the single definitions
shared by the worker and the fallback, so the serial semantics live in one
place.

## Boundedness and panic safety

- Fixed worker count: `MEGU_TICK_WORKERS`, default
  `max(1, threads-2)` clamped to `1..=4` (hard clamp `1..=16`).
- One bounded job list per dimension tick — at most one `MobJobInput` per
  snapped mob; no unbounded queue (mirrors `stream_gen`'s ticket bound).
- Every job runs under `catch_unwind`; a panic resolves that slot to `Err`
  and the commit recomputes inline. A worker that dies mid-job can never hang
  the tick: a slot that has not resolved within 250 ms marks the batch dead and
  later slots fall back instantly.
- Serial fallback is total: when the pool is disabled or fails to spawn,
  `mob_plans` returns a batchless set and every `take_valid` returns `None`.

## Kill switch and counters

- `MEGU_TICK_PARALLEL=0` disables dispatch entirely (checked once at startup);
  `MEGU_TICK_WORKERS=<n>` overrides the worker count.
- Counters (atomics, always on):
  `TICKPAR_REQUESTED` jobs enqueued, `TICKPAR_COMPUTED` plans finished by a
  worker — the off-tick-compute proof — `TICKPAR_FRESH` plans consumed by the
  commit, `TICKPAR_STALE` plans discarded for changed inputs,
  `TICKPAR_FAILED` panic/pool/timeout losses.
- `MEGU_TICKPAR_PROF=1` prints a summary line every 64 batches.

## Tests

- `src/tick_workers.rs` unit tests: ordered slot resolution under
  out-of-order completion, `Err`/panic fallback counting, moved-mob and
  world-epoch staleness, the 250 ms stall bound, serial mode, panic survival
  under `catch_unwind`, single-execution of every submitted job, kill-switch
  dispatch, and live-input replay (`is_spectator`, stare pose, helmet).
- `src/tick_tests.rs::tick_parallel_mob_tick_matches_serial_observably`:
  identical fixture run twice — once serial, once parallel, same seed/eids —
  asserts identical mob positions/hp/timers/velocities, identical world overlay
  writes, xp drops, AI-state map keys, and identical RNG state afterward, and
  asserts `TICKPAR_COMPUTED`/`TICKPAR_FRESH` both increased so the parallel
  path provably executed.
- `tools/layers_check.sh`: `tick_workers` registered in the stream_gen layer
  with edges `tick_workers -> block_registry`, `tick_workers -> worldgen`
  (module layer updated in `policy.json` + `deps_target.py`).

## Verification

- `cargo check --offline --locked -j2` — clean.
- `cargo test --offline -j2 -- tick_workers tick_parallel` — 13 passed.
- `cargo test --offline -j2 -- mob_` — 129 passed.
- `cargo test --offline -j2 -- tick_tests::` — 143 passed.
- `cargo test --offline -j2 -- mob_ai mob_behavior mob_tick` — 90 passed.
- `cargo test --release --offline -j2` (focused: `tick_workers tick_parallel`,
  `mob_`, `tick_tests::`) — 13 + 129 + 143 passed.
- A concurrent landing was mid-refactor in `src/play_protocol.rs` during final
  verification, so the release-mode sweep ran in a scratch `git worktree` at
  `HEAD` + this change's diff (same `CARGO_TARGET_DIR`, vanilla fixtures
  symlinked) — bit-identical sources under test, no interference from the
  in-flight edit. The main checkout was re-verified green (`cargo check` +
  the 13 focused tests) once that refactor settled.
- `tools/layers_check.sh` — PASS: source graph 46 modules / 117 edges,
  0 named errors.

## Limitations

- Parallelism is intra-dimension only; dimensions still tick serially (a
  later dimension's probes must see earlier dimensions' writes).
- `live_mob_behavior_step` and `tick_mob_ai_step` stay serial — both mutate
  persistent per-mob state (`live_mob_states`, `LIVE_MOB_AI` profiles) that
  cannot be snapshotted without changing semantics.
- The plan is opportunistic: under heavy mutation (many `set_block`s in one
  mob pass) more plans go stale and fall back inline; `TICKPAR_STALE` measures
  this.
