# Worldgen offload status — streamed-chunk generation queue

Date: 2026-11 (working-tree change, uncommitted). Scope: `src/stream_gen.rs`,
`src/main.rs`, `src/play_protocol.rs`, `src/worldgen.rs`.

## Problem

Player joins collapsed the server to ~6 TPS (single tick `max_work_ms` ≈ 1871).
`MEGU_CHUNK_PROFILE=1` attributed ~100 ms per delivered chunk (worldgen map ≈
49.6 ms, data ≈ 22.1, light ≈ 18.3, floors ≈ 11.1, ~94% `block_at` cache
miss). Generation ran on the per-connection send thread under the shared
generator lock, and any tick-side read of a not-yet-generated chunk populated
its whole demand box *on the tick itself*.

## What moved (this change)

`src/stream_gen.rs` adds a bounded asynchronous generation queue with two
lanes, all std threads (no `MeguTaskPool` dependency — the resident pool is
`mod-suite`-only and this path must work in every build):

- **Prepare workers** (`streamgen-w*`, `MEGU_STREAM_GEN_JOBS`, default
  `clamp(cores-2, 2, 4)`): build the pure half off-lock — `ChunkCols` plus base
  terrain rows on a scratch `Gen`, identical inputs to
  `prefetch_terrain_group_dim`'s worker arm.
- **One committer** (`streamgen-commit`): takes each prepared job *in request
  order* and runs the lock half under `PER_DIM.gens` —
  `install_prefetched` + `ensure_populated(cx, cz, feature::populate)` + one
  `terrain_section_dim` per section (short acquisitions, same as the send
  loop's) — then `pack_section` + `publish_section` + `install_band_rows`,
  filling the same published-section and opacity-band stores the send loop
  fills today.

In `stream_chunks` (play_protocol.rs): the batch's `news` is enqueued up front;
chunks the queue declines keep the inline `prefetch_terrain_group_dim` path;
before each `cached_chunk_frame` the sender blocks on its own ticket only.
`cached_chunk_frame`'s `ensure_populated`/`terrain_section_dim` calls then run
against already-generated state, and its `take_published`/`cell_source` reads
hit the committer's published sections and bands.

## What did NOT move (deliberately)

- **Encoding, overlay application, emitter bookkeeping, packet IO** stay on the
  connection thread (`map_chunk_lane`/`chunk_data_lane`, `overlay_band`,
  `GenSources`, `encode_play_level_chunk_with_light`). They are per-client and
  must remain ordered per connection.
- **Materialization of generated content** stays on the tick:
  `materialize_pending_generation` drains `Gen::staged_payloads` in populate
  order — unchanged, and the commit order preserves it.
- **Tick-side demand generation** for chunks *outside* the queue's requests
  (e.g. a mob/fluid read at the frontier of the streamed window) still resolves
  synchronously — a `block_at` cannot defer its answer. The queue removes the
  streamed-window cases; residual frontier populates are counted, not hidden.
- **`block_at`/`dim_base_state` uncached reads** still take the generator lock
  per call. The lock-free read paths already in place (`block_at_cached`,
  `stored_state`, `opacity_windowed`, `sky_floors` snapshots, `take_published`)
  now hit published state for streamed chunks; widening the uncached read to
  consult `section_store` was left out as a separate semantic decision.
- The serial populate arm is kept: the delivery path's `starts_round` gating is
  unchanged, so queued populates use the same serial arm the send loop used
  (the A3-a wave arm engages only when `MEGU_GEN_JOBS>1` opens a round, as
  before).

## Determinism / safety invariants

- **Commit order = request order**: a single committer drains `prepared` keyed
  by sequence; `pop_done`, `feat_c`, `shape_marks`, `staged_payloads` and
  `generated_ticks` append exactly as the serial send loop ordered them.
- **No duplicate generation**: tickets dedup by `(dim, cx, cz)`; a chunk is
  generated once under racing requests (queue dedup + `pop_done` in
  `ensure_populated` + row dedup in `install_prefetched`).
- **Save ordering**: overlay writes still flow only through
  `materialize_pending_generation` on the tick, in staged-payload order.
- **Bounded**: ticket map capped at 512; declines fall back to the inline path.
- **No stranded waiters**: prepare/commit panics are caught and resolve the
  ticket `false` (waiter falls back to inline generation); a generator re-init
  (`generation_seq` bump) resolves all outstanding tickets failed, keeps the
  sequence lane draining (pending/prepared jobs resolve without committing
  stale data), and re-requests re-queue under the new generation.
- **Lock hygiene**: workers hold no locks during prepare; the committer's only
  held-across-boundary lock is `PER_DIM.gens` (same order as the send loop's
  own holds); `install_band_rows`/`publish_section` run after the hold is
  released, as the encode does today.

## Observability

- `stream_gen::STREAMGEN_REQUESTED / COMMITTED / FAILED / FULL` — queue
  throughput counters.
- `stream_gen::TICK_GENERATED_CHUNKS` — incremented in
  `Gen::populate_in_order` when the caller is the tick thread; the join-storm
  regression is this counter climbing. It is the direct "generation ran
  off-tick" measurement: with the queue covering the streamed window, a join
  adds ~0 to it.
- `MEGU_STREAMGEN_PROF=1` — periodic `[streamgen]` summary from the committer.
- `MEGU_STREAM_GEN=0` — kill switch restoring the previous inline path for
  A/B measurement.

## Tests

`src/stream_gen.rs` `#[cfg(test)]`: request-order commits on a non-caller
thread, dedup under racing requesters, failed-prepare fallback, cap decline,
and generation-reset requeueing — all green offline.

## Verification run

- `cargo check --offline --locked -j2` — clean.
- `cargo test --release --offline -j2 stream_gen::` — 5/5 pass.
- Focused sweeps (`chunk`, `deliver`, `stream`, `populate`, `tick_`, `world_`,
  `save_`) — pass except two pre-existing fixture failures reproduced on the
  pristine tree: `tick_tests::p2_3_s4_switch_client_dimension_*` and
  `persistence_tests::save_v2_panics_abort_*` (both fail identically with the
  change stashed).
