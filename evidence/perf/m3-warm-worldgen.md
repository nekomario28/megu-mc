# M3 worldgen CPU and allocation comparison

The baseline is `90bc2caca449ccaf94c24524a3cb649719b17212`. Both binaries were built
with Rust 1.98.0 and `cargo build --release --offline --locked -j 1 --bin mc-spine`.
The AMD Ryzen 7 9800X3D host supports AVX2 and AVX-512. SHA-256 identifies the
two release artifacts.

| Binary | SHA-256 |
|---|---|
| Baseline | `6fbe27d87a07949534f5abccd275413f38c9c2bb6feaf3bc6d0f4b1eeefd84e1` |
| Candidate | `1205d930828d0f85f0ff15d9e0e823d6a7646aa2da6f870c6a2fff06c6d96d92` |

## Live warm load

The [load harness](../../tools/load_compare.sh) workload was run in new scratch
directories to avoid directory deletion. Both sides used the repository
[load client](../../tools/load_client.py), seed 0 and requested view distance
4. Each measured CPU interval started after a 20-second `LoadWarm` drive and
included the three-second settle, `LoadA` login and its 20-second drive. CPU
comes from server user and system ticks in `/proc`. The mc-spine arms ran in
alternating order with `MC_SPINE_GEN_JOBS=8 MC_SPINE_CHUNK_PROFILE=1`.

| mc-spine | Server CPU samples | Median CPU | Median first login | Median warm login |
|---|---:|---:|---:|---:|
| Baseline | 20.75, 20.36 s | 20.56 s | 8.55 s | 2.22 s |
| Candidate | 17.46, 16.17 s | 16.82 s | 5.19 s | 1.66 s |

Each mc-spine login received 81 unique chunks. The last common delivery
profile point, 150 map builds across both logins, reported median section time
of 25.15 → 16.60 ms per chunk. The timed movement drives received zero chunk
packets, so the live CPU comparison includes warmed delivery and tick work,
and live generation throughput is unverified.

Vanilla 1.21.1 used 4.23 CPU seconds in the same driver window with Java 21
and the server JAR pinned by `tools/load_compare.sh`. Its login delivered
nine chunks before the driver returned from the first batch, and its movement
phase also delivered zero. The candidate still used more server CPU in this
workload. Equal-work live throughput and the G3 50 ms tick budget remain open.

## Offline generation

[The repository harness](../../tools/perf_bench.py) interleaved both release binaries
for three rounds at each worker count. Each run generated 64 Overworld chunks
at seed 0 from chunk (0, 0). The harness was pinned to two physical cores with
four logical CPUs. Values are medians. Throughput uses the timed dump phase,
and CPU uses total child CPU time. Timing runs had counters disabled.

| Workload | Jobs | CPU before → after | Phase before → after | Chunks/s before → after |
|---|---:|---:|---:|---:|
| Full | 1 | 6.470 → 4.087 s | 6.524 → 4.168 s | 9.81 → 15.36 |
| Full | 8 | 8.748 → 4.821 s | 4.129 → 2.657 s | 15.50 → 24.09 |
| Terrain and carvers | 1 | 3.949 → 1.836 s | 4.036 → 2.017 s | 15.86 → 31.73 |
| Terrain | 1 | 1.653 → 0.925 s | 1.907 → 1.087 s | 33.57 → 58.88 |

The separate full one-worker M3 counter pass observed the same stage call counts
and aquifer cell count. Elapsed stage times overlap because stages call one
another.

| Stage | Calls | Before | After |
|---|---:|---:|---:|
| Column builds | 516 | 3,257.8 ms | 2,937.3 ms |
| Beardifier resolution | 238 | 1,522.0 ms | 1,272.7 ms |
| Section fill | 1,536 | 1,310.8 ms | 631.5 ms |
| Carving | 1,089 | 2,406.0 ms | 1,019.4 ms |
| Feature placement | 121 | 667.0 ms | 578.9 ms |

Both full runs had 5,287,057 aquifer queries. Vector density groups went from
4,687,778 to 3,725,872 as the surface scan reused the eight-lane path.
`MC_SPINE_WG_PROFILE=1` timed the first full column build at 264.79 → 203.22 ms
(median of 11 processes). With `WG_COUNT=1 MC_SPINE_DFSTATS=1`, the diagnostic
interpreter reported 29,111,194 visits and 257 visited nodes in each binary.

Heaptrack used `MC_SPINE_WG_PROFILE=1 MC_SPINE_NOFEAT=1 MC_SPINE_GEN_JOBS=1`
and a 4×4 seed-0 dump. Both dumps matched byte-for-byte. Allocations went from
10,762,088 to 565,305 (−94.7%), and temporary allocations from 10,003,728 to
78,019. A separate symbol-enabled baseline traced the largest allocation stack
to the SHA-256 seed derivation in surface-rule biome lookups, including those
triggered by carvers. The generator now derives that seed once.

The release `--paletted-bench` validated 10,976 sections in each binary, with
zero cell and wire mismatches. The measured 2,200 → 2,256 ns/section difference
is within the plan's noise floor.

## Parity and checks

The harness checked each timing and profile dump against these hashes taken from
the untouched baseline. The historical defaults in the harness already differed
at `90bc2ca`. The A/B used the harness's `--expected-sha` option with a fresh
baseline map. Every candidate sample matched the following digests.

```json
{
  "full": "368ab50dc931faec43469cd4fd9620f56c1f8c0931f0708c3bfd9e9b3e8297b7",
  "nofeat": "16dd14904130aa56a887cb4fb7e18b2492784cb147a08325cdc46df64f91b766",
  "terrain": "c7fa585e8ec40df5825325c6d39fc6f6d17d5c01e3e80f2f4e4b3bae9b50c3e0"
}
```

Offline 4×4 full dumps and heightmaps also matched byte-for-byte for Overworld
seed 4242 from (−2, −3), Overworld seed −1 from (−3, 1), and Nether and End seed
4242 from (−2, −3). The Overworld seed-4242 block dump also matched with
`MC_SPINE_DF8=0` and `MC_SPINE_DF4=0`.

- `cargo check --offline --locked -j 1 --all-targets` passed with zero warnings.
- `cargo test --offline --locked -j 1 -- generation_contract_tests feature::pipeline_tests aquifer_native_tests --test-threads=1`
  passed all 160 tests.
- Sixteen focused tests passed, covering scalar/SIMD density, memo and aquifer
  identity, paletted wire identity, and serial/parallel parity. Three additional
  native capture tests passed for whole Overworld, structure and End chunks.
  The section fill and mark order also matched the scalar cell path.
- `tools/layers_check.sh`, `tools/debt_ratchet.sh`, `tools/provenance_check.py`,
  and `git diff --check` passed.
