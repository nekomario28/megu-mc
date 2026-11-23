# L26 — OGG decode off the packet-ingest path

## What changed

`AudioRuntime::ingest` no longer calls `SoundLibrary::decode_source`
synchronously. Ingest now resolves the sound name/registry id and variant
(seed-picked, same as before), builds a `DecodeJob` carrying the encoded
`SoundSourceData` plus all mixer routing metadata, and pushes it onto a
bounded `DecodeQueue`. A single `megu-audio-decode` std thread (plain std,
no deps) pops jobs — woken by `Condvar`, never polls — decodes to PCM via
`decode_one`, and returns results over an `mpsc` channel. `drain_decode_results`
(runs at the top of `ingest`, `ingest_stop`, and `pump_frames`) completes
results in submission order and submits `PlaySoundEvent`s to the existing
`SoundMixer`, so per-source ordering is preserved even though decodes can
finish out of order.

## Bounds and lifecycle

- `MAX_DECODE_QUEUE_JOBS = 128`, `MAX_DECODE_QUEUE_BYTES = 16 MiB` (size
  hints from file/zip-entry lengths; sources are references, not bytes).
- `DecodeQueue::push` never blocks: on overflow it evicts the oldest queued
  job *for the same source*; if that cannot fit, the new job is declined.
  A lone job larger than the byte cap is accepted only into an empty
  queue. Drops/evictions are counted in `AudioStats::decode_dropped`.
- `stop_sound` while decodes are outstanding records a `PendingDecodeStop`
  (category/name match, order-bounded); `finish_decode` discards matching
  jobs so a stopped sound never reaches the mixer — same semantics as the
  mixer's own stop filtering.
- `DecodePipeline::shutdown`/`Drop` closes the queue (backlog discarded)
  and joins; at most the in-flight decode is awaited. `AudioRuntime::finish`
  shuts the pipeline down before the sink.
- Decode failure still falls back to the deterministic placeholder tone and
  `decode_errors` accounting, unchanged.
- `PcmQueue::pop_blocking` and the sink writer fallback are untouched.

## Supporting refactors (client-app only)

- `SoundLibrary` is shared as `Arc<RwLock<SoundLibrary>>`; the worker takes
  a write lock only for the decode+cache insert, ingest holds read locks
  for resolve.
- `sound_assets.rs`: `decode_source` split into cache-probe /
  uncached-decode / cache-insert phases so the worker decodes under a read
  lock.
- `zip_entries.rs`: `entry_comp_size` exposes the compressed size hint.
- `session.rs`: `AudioStats::decode_dropped` counter.

## Tests (`cargo test --release --offline -j2`, client-app)

New unit tests in `audio_out.rs`:

- `decode_queue_is_fifo_and_close_discards_backlog`
- `decode_queue_cap_evicts_same_source_then_declines`
- `decode_queue_job_cap_evicts_only_same_source`
- `decode_pipeline_returns_results_in_submission_order`
- `decode_pipeline_shutdown_discards_backlog_promptly`
- `ingest_returns_before_decode_finishes_then_submits_in_order` — gated
  decoder proves `ingest` returns while decodes are parked, results land
  in order
- `stop_sound_cancels_in_flight_decode_but_not_later_ones`

`qual_audio_3_tests.rs` B4 was updated to pump until async voices land.

Result: all client-app targets pass. One pre-existing, unrelated failure:
`integrated::tests::resolve_server_bin_canonicalizes_and_prepare_world_dir_links_only_vanilla_datapack`
(path canonicalization env issue — fails identically on a clean tree).
`cargo check` clean, no warnings.

## Caveats

- Decode latency for the *first* play of a sound is now worker-scheduling
  bound (~µs–ms) instead of decode-bound; positional sounds are unaffected
  ordering-wise.
- The 16 MiB byte cap is an estimate (encoded sizes), matching the
  hint-based style of existing caps.
