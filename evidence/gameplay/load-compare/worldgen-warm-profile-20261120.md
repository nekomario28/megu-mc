# Phase E Wave 4 / Lane E4-12 — release initial-81-chunk worldgen and `WARM=1` latency profile (2026-11-20)

This is a **measurement and attribution receipt**. No `.rs`, `.py`, `.sh`, `.java` or `.toml`
file was changed; no server constant, driver deadline or gate was relaxed. It explains why
`WARM=1 bash tools/load_compare.sh 20 0` fails on `main` at `3f1cb4a`, where the time goes in
the release binary, and the smallest change that removes the failure.

## 0. Identity and provenance

| item | value |
|---|---|
| Checkout | `<worktree>`, branch `t3/worldgen-warm-latency-profile`, HEAD `3f1cb4a16e685799661d46c937196a47a54cf5f3` (`main`) |
| Release build | `mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"; cargo build --release --offline --locked` → **PASS** in 58.14 s; binary `target/release/mc-spine`, SHA-256 `0a0ab28c6c0e13649606b5603efa3fbadd0786b308d52148f6a53d47e6a83979` |
| Debug build | `cargo build --offline --locked` → **PASS** in 11.41 s (same three warnings) |
| Tools | `tools/perf_bench.py` SHA-256 `7c731c52c8c5b4e831af1cc1bae1a51638104b45892a066de2f26a49b267891f`; `tools/load_compare.sh` `9e16162406187863f8c1f712c7c50ca1f90eaf447fbe2c8a17022a3390d5c021`; `tools/load_client.py` `d79746c847873912686dd1adcffdc85bcc60b7d07fd1e1b91796535de4262c3b` |
| Bench command | `TMPDIR="$PWD/target/tmp" python3 tools/perf_bench.py --binary "$PWD/target/release/mc-spine" --profile-counters --arms full,nofeat,terrain --jobs 1,5,8 --rounds 1 --dump-n 8 --tmpdir "$PWD/target/tmp" --json /tmp/opencode/e4-12-perfbench.json --markdown` → 36.99 s, all dump digests verified |
| Live server | release binary in a scratch dir symlinking `datapacks`, `assets`, `config_packets.bin`; `MC_SPINE_SEED=0`, `MC_SPINE_RESOURCE_PACK_URL=""`, ports 25612–25619; `TMPDIR=/tmp/opencode/e4-12/tmp` |
| Driver | unmodified `tools/load_client.py` copied **outside the repo** to `/tmp/opencode/e4-12/load_client_long.py` and patched only to raise its login deadline 15 s → 180 s and to print phase timestamps. All protocol bytes are otherwise identical to the repository driver. |
| Raw artifacts (outside Git) | `/tmp/opencode/e4-12/`: build logs, `perfbench.json`, `spine/spine.log`, `driver-*.txt`, `run-*.txt`, `loadcompare-*.txt` |
| Host context | 16 cores, 30 GiB RAM. Other lanes were running `cargo test`/`cargo build` under `devin-compute` during these runs; absolute ms values are load-sensitive (see §7). |

### Corrections to the task wording

1. `python3 tools/perf_bench.py --help` exposes `--arms full,nofeat,terrain` only. **`--arms candidate` is not a valid value** (there is no candidate arm without a `--compare-binary`), so the bounded run below uses the three real arms, `--jobs 1,5,8`, `--rounds 1`.
2. The task's `src/main.rs:20233` (`stream_chunks`) is stale for this HEAD: line 20233 is inside `write_light_arrays`. On `3f1cb4a`, `stream_chunks` starts at `src/main.rs:20469` and the `0x0C` send is `src/main.rs:20593`.
3. There is no `src/lighting.rs` in this tree. Sky/block light lives in `src/main.rs:18199` (`block_light_sections_dim`), `19296` (`sky_light_sections_world`), `20212` (`write_light_arrays`) and `20267` (`update_light_packet_dim`), with `src/light_pack_tests.rs` for tests.
4. `perf` is **not installed** on this host (`which perf` → not found). Attribution therefore uses the binary's own env-gated counters (`MC_SPINE_GEN_TRACE`, `MC_SPINE_CHUNK_PROFILE`, `MC_SPINE_TICK_PROFILE`, `MC_SPINE_M3_PROFILE`; see `src/trace_env.rs:10-15`), not a sampled profile.

## 1. Build result and non-test warnings

Both the release and the dev `cargo build --offline --locked` (non-test) emitted **the same three `dead_code` warnings**; nothing else:

| warning | location |
|---|---|
| `function spawn_etype_for_biome is never used` | `src/main.rs:16825` |
| `function choose_supported_hostile is never used` | `src/feature.rs:5665` |
| `function biome_name_at_block is never used` | `src/worldgen.rs:10634` |

`biome_name_at_block` is indeed unused in non-test builds: its only call site is a `#[test]` gated on `FEAT_PROFILE` at
`src/worldgen.rs:14922` (`terrain_parallel_throughput`). A separate `biome_name_at` hot path is
discussed at `src/feature.rs:9356`, but it does not call this symbol. These warnings are
pre-existing on `main` and are not a regression from this lane.

## 2. `tools/perf_bench.py` — 64-chunk dump, release binary

`full` = terrain + carver + features; `nofeat` = terrain + carver (`MC_SPINE_NOFEAT`); `terrain` =
terrain only. `ms/chunk` is over `dump_n=8` → 64 chunks.

| workload | jobs | wall (s) | phase (s) | CPU (s) | ms/chunk |
|---|---|---:|---:|---:|---:|
| full | 1 | 4.843 | 4.815 | 4.821 | 75.7 |
| full | 5 | 3.203 | 3.176 | 5.740 | 50.1 |
| full | 8 | 3.040 | 3.012 | 6.031 | 47.5 |
| nofeat | 1 | 2.410 | 2.384 | 2.399 | 37.7 |
| nofeat | 5 | 1.373 | 1.345 | 2.652 | 21.5 |
| nofeat | 8 | 1.293 | 1.259 | 2.750 | 20.2 |
| terrain | 1 | 1.319 | 1.271 | 1.305 | 20.6 |
| terrain | 5 | 0.402 | 0.375 | 1.315 | 6.3 |
| terrain | 8 | 0.343 | 0.314 | 1.449 | 5.4 |

Verified digests (with `--no-verify-sha` **not** used): `full` `670fdfe34728…`, `nofeat`
`57101c434d49…`, `terrain` `bb499dfb6c53…` — no mismatch at any job count.

M5 stage durations from the `--profile-counters` passes, `jobs=1` (sums over the 64-chunk dump,
per-thread, under profiling; `n` is the call count):

| stage | ms | n | ms/call | present in `terrain`? | present in `nofeat`? |
|---|---:|---:|---:|---|---|
| `cols` (density / material rule / surface / aquifer setup) | 3252.7 | 516 | 6.31 | 271.7 ms / 64 | 910.3 ms / 230 |
| `beard` (structure/beardifier resolution) | **1527.7** | 238 | **6.42** | 0 | 0 |
| `carver` | 1196.6 | 1089 | 1.10 | 0 | 1067.2 ms / 1089 |
| section `fill` | 894.3 | 1536 | 0.58 | 890.0 | 883.3 |
| `feature` placement | 631.1 | 121 | 5.22 | 0 | 0 |

Other counters, `full/j1`: `aquifer_cells` 5,287,056; `surface_cells` 39; deep material-rule
fallbacks 735,665; `rc_hit` 549,294; `prefetch_miss` 1,373,895; DF4 vector groups 4,687,778
vs 73,829 one-lane bails (1.6%); chunk starts owner/worker 511/0. At `j5`/`j8` the same dump
gives owner/worker starts 126/385 and DF4 vec 3,662,746 — i.e. the extra work is off-owner
terrain work, not new work. These stage timers are **nested** (notably `cols` includes
beardifier-driven column builds), so they must not be summed against the 4.843 s wall. The clean
arm deltas are the attribution: **structure/beardifier + feature population is 2.433 s of the
4.843 s `full/j1` wall (50 %), carvers 1.091 s (23 %), terrain 1.319 s (27 %)**; on this host
feature placement itself is smaller than the README's earlier 50 ms/chunk figure.

Derived deltas at `j1`: `full − nofeat` = 2.433 s (38.0 ms/chunk) is features + beardifier;
`nofeat − terrain` = 1.091 s (17.0 ms/chunk) is carvers; `terrain` itself is 20.6 ms/chunk.
At `j8`: 1.747 s (27.3 ms/chunk) and 0.950 s (14.8 ms/chunk) respectively. Terrain itself
scales 20.6 → 6.3 → 5.4 ms/chunk with workers; features/beard do not scale by default on the
delivery path (§5).

## 3. Live release server — exact latency to packet `0x0C` for the initial 81 chunks

`window_batch(view=4)` = `(2·4+1)²` = 81 (`src/main.rs:20372-20375`); the first batch is not
gated on an acknowledgement (`src/chunk_ack.rs:41-49, 57-59`), so `news` is 81 chunks
(`src/main.rs:20517-20532`) and `0x0D` … `0x0C` brackets that one batch
(`src/main.rs:20544`, `20593`). The client waits for `0x0C` under a fixed 15 s deadline
(`tools/load_client.py:60, 74-77`).

Timestamps below are from the long-deadline driver; `T0` is the first handshake byte. All runs
had `login_success ≈ 0`, `known_packs 0x0E ≈ 0`, `play 0x03 ≈ 0.041`, `batch_start 0x0D ≈ 0.042`;
only the last row differs:

| server environment | to `0x0C` (s) | driver wall (s) | server CPU (s) | verdict vs 15 s |
|---|---:|---:|---:|---|
| **unset / default** (profiling env on, port 25612) | **62.784** | 63.834 | — | FAIL |
| **unset / default** (clean, port 25613) | **60.389** | 61.485 | — | FAIL |
| **unset / default** (clean, port 25618, CPU sampled) | **57.461** | 58.513 | **129.920** | FAIL |
| `MC_SPINE_GEN_JOBS=1` | 8.290 | 9.347 | — | PASS |
| `MC_SPINE_GEN_JOBS=5` | 6.846 | 7.896 | — | PASS |
| `MC_SPINE_GEN_JOBS=8` | 6.510 | 7.551 | — | PASS |
| `MC_SPINE_GEN_JOBS=8` (CPU sampled) | 6.477 | 7.527 | **11.690** | PASS |
| `MC_SPINE_GEN_WORKERS=5` (older name) | **59.597** | 60.639 | — | FAIL |

The last row is the discriminator: `MC_SPINE_GEN_WORKERS` raises `terrain_jobs()` but leaves
`delivery_prefetch_jobs()` at 1, reproducing the default failure. `MC_SPINE_GEN_JOBS` raises
both and fixes it. The fast configurations also use ~11× less server CPU, so this is not a
wall-versus-CPU trade-off.

End-to-end confirmation with the repository's own harness (`WARM=1 … tools/load_compare.sh 20 0`,
vanilla jar from the local cache, `MC_SPINE_LOAD_WORK` redirected to `/tmp/opencode/e4-12/loadwork`):

| run | mc-spine driver | cpu (s) | rss (MiB) | threads | window valid? |
|---|---|---:|---:|---:|---|
| default | `LOAD failed:` → `load_client.py:75` `TimeoutError: timed out` (`recv_exact`, `wire_smoke.py:41,51`) | 31.170 | 232.9 | 6 | **No** — same invalid point E4-6 flagged |
| `MC_SPINE_GEN_JOBS=8` | `LOAD ok packets=1176 chunks_moved=90` | 9.810 | 373.4 | 4 | Yes |

The script's warm driver is `timeout $((20+30))` = 50 s (`tools/load_compare.sh:63-66`), shorter
than the ~57–63 s first batch, so the warm client is killed mid-window and the measured `LoadA`
also hits the 15 s deadline. With the batch at 6.5 s both drivers fit. Do not cross-compare the
two vanilla rows (CPU 2.790 s vs 4.690 s) — host load differed between the two runs.

## 4. Where the ~60 s go in the slow default

From the profiling run on port 25612 (`MC_SPINE_GEN_TRACE=1 MC_SPINE_CHUNK_PROFILE=1
MC_SPINE_TICK_PROFILE=1`):

* **Terrain prefetch is not the bottleneck.** `[terrain]` printed 41 rounds of 2 chunks,
  `jobs=1`, 48 sections each; `fill_ms` total **1134.1 ms**, mean 27.7, max 45.8 — only ~1.9 %
  of the 62.8 s batch. (`src/worldgen.rs:7047-7062`; group size is `terrain_group(1)=2` at
  `src/main.rs:20398-20400`.)
* **`map_chunk` is.** `CHUNKPROF` reports every 30 builds (`src/main.rs:20197-20202`):
  * `n=30`: `map=1239.3ms/chunk data=1234.6 section=845.2 light=4.7 (sky=2.5 block=2.1
    floors=1.5 block_at=3952 miss=3952)`. `read/chunk: n=8612 ms=387.41 band_path n=528
    ms=387.21`; `encode/chunk: reads=8612 … encode=388.00ms`; `phases/chunk: biomes=1.19ms
    band_install=0.02ms section_encode=0.18ms`.
  * `n=60`: cumulative `map=691.1ms data=621.5 section=425.7 light=69.6 (sky=67.8 block=1.8)`;
    `read/chunk: n=8920 ms=193.90`; `encode=194.52ms`; `block_at=2812 miss=2812`.
  * Cumulative averages imply the first 30 chunks averaged ~1239 ms of `map_chunk`, the next 30
    ~143 ms. Every `block_at` in these windows was a **miss** (3952/3952, 2812/2812).
* **The tick thread queues behind the generator lock.** `GENLOCK` (every 100 ticks,
  `src/worldgen.rs:6395-6426`): ticks 200–300 `wait=25548.9ms hold=51422.8ms n=173351`;
  ticks 300–400 `wait=34805.6ms hold=71681.5ms n=1402398` — ~1.58 M lock acquisitions in
  200 ticks. `TICKPROF` for the same windows: `body=255.56ms/tick` with
  `generated_content=255.445ms`, then `body=375.36ms/tick` with
  `generated_content=334.836ms`, `scheduled_blocks=19.742ms`, `sched/read_batch=13.144ms`,
  `sched/write_loop=6.414ms`, `random_ticks=0.644ms`. Idle windows are ~0.03 ms/tick.
* Each such acquisition is one `wg2_block` → `worldgen::block_state` → `gen_lock()` per cell
  (`src/main.rs:7551-7555`, `src/worldgen.rs:10003-10011`), and `block_state_at` calls
  `feat_state` for the 3×3 feature scan. This is the per-cell read path counted by `CHUNKPROF`
  as 8.6–8.9 k reads/chunk.

So the 15 s budget is spent almost entirely in **feature/structure population plus the
per-cell send-path material lookup**, not in density/surface/aquifer terrain or in networking.
Light is small (4.7 ms/chunk early, 69.6 ms later, sky-dominated).

## 5. Root cause: two worker-count gates disagree on the delivery path

The release server has **two different defaults for "how many worldgen jobs"**:

1. `gen_workers()` → `worldgen::terrain_jobs()` (`src/main.rs:20391-20393`). `terrain_jobs()`
   defaults to `min(available_parallelism, TERRAIN_MAX_JOBS)` = 8 on this 16-core host
   (`src/worldgen.rs:7066-7093`). This value gates the **A3-a parallel feature/carver dispatch**:
   `src/worldgen.rs:8775-8796` sets `g.a3a_dispatch = Some(pending)` when `jobs > 1`, and
   `src/feature.rs:10702-10717` then runs `precarve_order` + `populate_parallel`
   (`src/feature.rs:9791-9873`).
2. `delivery_prefetch_jobs()` (`src/main.rs:20402-20417`) returns **1** unless
   `MC_SPINE_GEN_JOBS` is explicitly set. This value alone decides whether a round-scoped
   cache is opened around the batch: `src/main.rs:20546-20572` opens `starts_round_begin()`
   only when `delivery_workers > 1`.

`starts_round` is the key to the structure-start caches. `starts_round_begin/end`
(`src/structure_pipeline.rs:1484-1506`) create a shared `RoundCache` and also open
`worldgen`'s shared terrain caches (`src/worldgen.rs:7646`). `adapting_starts_cached`,
`ring_chunk_i` and `ring_chunks` use that shared single-flight cache when a round is open
(`src/structure_pipeline.rs:1511-1529, 532-553, 573-587`); otherwise they fall back to a
thread-local cache. A3-a scratch generators copy `owner.starts_round`
(`src/feature.rs:9812, 9836`, `9380`).

Consequently the **default configuration combines "parallel feature/carver dispatch" with "no
shared round cache"**. Each A3-a scratch generator then rebuilds structure starts and terrain
answers on its own thread-local cache, and the measured first window costs 57–63 s. The two
consistent configurations are fast: serial features with no round (`GEN_JOBS=1`, 8.29 s) and
parallel features with a round (`GEN_JOBS=5/8`, 6.5–6.8 s). This is corroborated by the M3
counters: `beard` is 0 in the `terrain`/`nofeat` arms and 1527.7 ms/238 calls in `full/j1`,
while `cols` builds rise 64 → 230 → 516 from `terrain` → `nofeat` → `full`.

This is a **latency bug caused by configuration asymmetry, not by slow density noise**. It is
consistent with the README note at `src/main.rs:20385-20390` that 4 workers raised CPU in a
*moving-view* `WARM=1` comparison; that note measures steady-state CPU, whereas this receipt
measures the initial window wall clock, and the two are not the same quantity.

## 6. Prioritized optimization targets for Wave 4 Phase D-2

**P0 — make the two gates consistent (one-condition fix, highest priority).** Either
(a) add `self.starts_round.is_some()` to the A3-a dispatch `gate` at
`src/worldgen.rs:8782`, so a parallel feature/carver batch can only be dispatched when a shared
round cache exists — this keeps the delivery prefetch serial and preserves the tick-latency
contract documented at `src/main.rs:20402-20411`. The closest measured analogue is
`MC_SPINE_GEN_JOBS=1` (8.29 s), which also serialises carvers; with `terrain_jobs()` still 8,
option (a) must be re-measured rather than assumed to equal that number; or
(b) make `delivery_prefetch_jobs()` fall back to `gen_workers()` when `MC_SPINE_GEN_JOBS` is
unset (`src/main.rs:20412-20417`) — measured 6.5 s, but it changes delivery tick latency and
must first re-validate
`tick_tests::wall_clock_boundaries_are_gated_on_elapsed_time_not_tick_count`. Prefer (a) as the
lowest-risk change; treat (b) as a separately measured alternative. Acceptance for either:
`WARM=1 bash tools/load_compare.sh 20 0` produces a valid `LoadA` line, time to `0x0C` stays
well under 15 s with margin, all dump digests and the frozen population digest are unchanged.

**P1 — collapse the per-cell GEN-lock/material-rule read in the send path.** 1.58 M GENLOCK
acquisitions in 200 ticks and 8.6–8.9 k `CHUNKPROF` reads/chunk with 100 % `block_at` misses
come from `wg2_block` → `block_state` → `feat_state` per cell. A dense per-cell cached material
answer, or cell-order evaluation that shares the 8 noise corners, is the structural fix. This
is a large isolated slice: earlier attempts recorded in the load-compare README (full-column
material precompute; `FC.wr` FxHash; `interpolated` treated as a plain cache) were measured and
**reverted** — do not repeat them. Gate on `CHUNKPROF` read-ms/chunk and GENLOCK `n` plus the
frozen digest.

**P2 — structure/beardifier start scans.** `beard` is the largest single M5 stage at `j1`
(1527.7 ms / 238 calls, 6.42 ms/call) and `ring_admit` scans a conservative ±8-chunk window
(`src/structure_pipeline.rs:556-569`). After P0, measure whether round-scoped caches are being
dropped too early between batches (`starts_round_end` at `src/main.rs:20571`) and whether a
bounded cross-round cache for seed-invariant ring indices removes repeated scans. Gate on the
M3 `beard` counter and the structure-start scan probe, with output digest unchanged.

**P3 — small, low-leverage items (only after P0–P2).** `Rule::Seq` re-borrows `self.rules[rid]`
on every iteration of the sequence (`src/worldgen.rs:5990-6003`); three non-test `dead_code`
warnings (§1); sky light 67.8 ms/chunk in the later chunks; 1.6 % DF4 one-lane bail rate. None
of these explains the 15 s timeout and none should be attempted before P0 is verified.

## 7. Evidence boundaries

* The A/B runs were back-to-back on one host while other lanes ran builds; absolute ms and CPU
  values move with that load. The failure/pass distinction and the ~9× wall ratio are far
  larger than that noise, and the `WARM=1` driver outcome is binary.
* M5 durations are per-thread sums with profiling enabled and are explicitly load-sensitive
  (`src/worldgen.rs:1794-1797`); use them for attribution only. The M1–M4/M6 counts and the
  dump SHA-256 checks are the load-independent evidence.
* The 64-chunk dump is not the 81-chunk live window; do not scale `ms/chunk` linearly into a
  server deadline. The live driver measurement in §3 is the authoritative latency number.
* `MC_SPINE_GEN_JOBS=8` makes the `WARM=1` window valid, but its RSS/CPU is **not** a
  like-for-like vanilla comparison (mc-spine implements fewer features); it only replaces the
  invalid data point E4-6 flagged with a measured one.
* No tests were run in this lane because no code changed. Digest verification in `perf_bench`
  passed for every arm/job combination; the tree's own determinism gates remain the authority
  for any P0–P3 change.

## 8. Static gates and cleanup

```
bash tools/layers_check.sh   -> source graph: 44 modules, 111 edges, 0 named errors; layers check: PASS
bash tools/debt_ratchet.sh   -> ok wall-clock reads: 0; ok ignored tests: 1; debt ratchet: 0 over limit
python3 tools/provenance_check.py -> provenance: clean (no tracked assets/pblocks.json, assets/properties.json, resourcepack.zip)
git diff --check             -> clean
```

`target/` is gitignored (`git status --ignored` → `!! target/`). `target/tmp` was removed after
the runs; `target/release/mc-spine` was left as an ignored build artifact. The only tracked
change in this lane is this evidence file, committed with `git commit -s` (DCO sign-off, no AI
attribution trailers).
