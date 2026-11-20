# Issue #41 — palette churn comparison (2026-11-20)

## Decision

**Do not replace `src/paletted.rs` with the tested free-slot/periodic-compaction design.** The existing implementation is faster on the 384-section cold-build workload (3,118.8 vs 34,346.7 ns/section), steady edits (218.6 vs 519.5 ns/edit), and 256-state churn (6.5 vs 650.0 ns/edit). The trial also requests more heap allocation in these workloads. Its useful behavior is reclaiming a uniform section and avoiding a stale 15-bit representation after churn. A *separately measured*, infrequent uniform demotion in the existing code may merit a narrowly scoped future experiment if real-world traces show sufficient retained-memory pressure. The present data does not justify adopting even that change in production now.

## Provenance and procedure

- Baseline commit before this change — `5759cbef5e5bf9f809365c6b2a4cc2fd9d9d57a9`.
- Branch — `t3/benchmark-palette-churn`.
- Host — Linux x86_64, AMD Ryzen 7 9800X3D, 16 logical CPUs. `rustc 1.98.0 (88d9e12ae 2026-08-18)`; `cargo 1.98.0 (797e8a9bc 2026-08-05)`.
- Comparison — actual `PalettedSection` from baseline `src/paletted.rs` is read by `tools/palette_bench/build.rs` at compile time and included up to its separately coupled `run_paletted_bench` entry point. The production implementation is neither edited nor duplicated in tracked files.
- Trial — original, clean-room indirect/direct non-straddling bit-packed palette with per-entry reference counts, reusable vacant slots, and a candidate compaction every 128 effective edits. FerrumC motivated the architectural comparison. No FerrumC source, lookup tables, assets, registry mappings, or dependencies are incorporated.
- The crate has zero external dependencies. Its independent Minecraft protocol 767 section encoder/strict decoder tests canonical packet equality and decoded states/biomes. No live Minecraft server, real chunk capture, or FerrumC executable was benchmarked.

Run from repository root, with the host's full `/tmp` avoided:

```bash
cd tools/palette_bench
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
cargo test --offline
cargo run --release --offline
cargo run --release --offline
cd ../..
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
bash tools/layers_check.sh
bash tools/debt_ratchet.sh
python3 tools/provenance_check.py
```

Metrics below are from the first of two consecutive final release runs after formatting. The second run is provided for batch stability. Timings are wall-clock `Instant` nanoseconds per operation, with 64 get sweeps of 4,096 cells, 64 cold-build iterations per individual case, 20 packet serialization/decoding sweeps in the batch, and one ordered trace per set workload. The single-edit case is timing-noisy. Set and build allocation measurements count cumulative requested heap bytes and calls, including successful reallocations, through a `GlobalAlloc` wrapper. They are **not** live heap usage. Resident estimates are `size_of` plus owned buffers/capacities, excluding allocator metadata and caller storage. The batch `VmRSS` is the entire process with both implementations and workload buffers live. Synthetic states use locally generated IDs and lightweight opacity/hidden predicates, so timings do not include real registry callbacks.

## Per-section results

Every case represents 4,096 block-state cells. `stone` is constructed uniformly, while `fill_stone` starts as air and writes stone to every cell.

| Workload | Edits | Cold build ns baseline / trial | Set ns baseline / trial | Get ns baseline / trial | Resident bytes baseline / trial | Final bits baseline / trial | Canonical packet bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Air only | 0 | 261.6 / 3,259.4 | — | 0.56 / 0.53 | 48 / 120 | 0 / 0 | 8 |
| Uniform stone | 0 | 261.0 / 3,622.8 | — | 1.51 / 0.58 | 48 / 120 | 0 / 0 | 8 |
| Fill stone | 4,096 | 265.8 / 4,424.9 | 1,344.0 / 196.3 | 1.61 / 0.53 | 2,104 / 120 | 4 / 0 | 8 |
| Expand to 2 states | 1 | 3,178.0 / 13,410.7 | 150.0 / 4,929.0 | 1.15 / 1.82 | 2,104 / 2,180 | 4 / 4 | 2,059 |
| Expand to 16 states | 15 | 4,107.5 / 23,851.3 | 717.4 / 384.7 | 1.15 / 1.76 | 2,160 / 2,264 | 4 / 4 | 2,073 |
| Expand to 256 states | 255 | 2,738.6 / 252,704.0 | 657.5 / 1,836.6 | 1.17 / 1.81 | 5,168 / 5,752 | 8 / 8 | 4,491 |
| Expand to 512 states | 511 | 2,966.2 / 14,123.9 | 329.9 / 906.7 | 0.75 / 0.86 | 8,240 / 8,312 | 15 / 15 | 8,200 |
| 256-state high churn | 16,384 | 3,788.7 / 145,254.7 | 6.5 / 650.0 | 0.74 / 1.80 | 8,240 / 5,752 | 15 / 8 | 4,491 |
| Remove/re-add 31 of 32 | 3,999 | 2,549.8 / 11,709.9 | 134.0 / 220.3 | 1.57 / 2.40 | 2,912 / 3,048 | 5 / 5 | 2,777 |
| Alternate all air/stone | 16,384 | 264.9 / 2,766.6 | 541.3 / 192.4 | 1.21 / 0.53 | 2,104 / 120 | 4 / 0 | 8 |

The cold-build input is the **final** distribution for each case, whereas the set path begins at the listed scenario's initial state. In churn, all 4,096 cells are overwritten in four passes, alternating two disjoint 256-state sets. The baseline reaches direct 15-bit storage after briefly holding 257 different historical palette entries. The trial reuses free slots and finishes at 8 bits, saving 2,488 resident bytes (30.2%) per section, at ~100× higher set time and ~3.6 MB cumulative requested heap bytes.

| Edit-path allocation workload | Requested bytes baseline / trial | Allocation calls baseline / trial |
| --- | ---: | ---: |
| Fill stone | 2,056 / 574,228 | 2 / 128 |
| Expand to 16 | 2,588 / 2,228 | 16 / 9 |
| Expand to 256 | 147,388 / 110,716 | 260 / 77 |
| Expand to 512 | 155,580 / 193,588 | 261 / 125 |
| 256-state churn | 8,192 / 3,629,984 | 1 / 2,112 |
| Remove/re-add 31 of 32 | 0 / 606,588 | 0 / 323 |
| Alternate all air/stone | 2,056 / 2,296,912 | 2 / 512 |

For single-case cold construction, baseline/trial allocation bytes are respectively 0/0 for uniform states, 2,056/18,456 for 2 states, 2,112/18,600 for 16, 5,120/23,528 for 256, and 8,192/27,624 for 512. The trial's build path allocates temporary indices/references in addition to retained buffers.

## Bit-width and fragmentation checkpoints

All mandatory transitions were asserted *during incremental mutation*, with independently built snapshots also checked at 1, 2, 16, 17, 32, 33, 64, 65, 128, 129, 256, 257, and 512 distinct states.

| Distinct active states | Expected baseline / trial bits |
| --- | ---: |
| 1 | 0 / 0 when constructed uniformly |
| 2–16 | 4 / 4 |
| 17–32 | 5 / 5 |
| 33–64 | 6 / 6 |
| 65–128 | 7 / 7 |
| 129–256 | 8 / 8 |
| 257–512 | 15 / 15 |

After changing 31 of the original 32 values to stone, the baseline remains at 5 bits and 2,912 bytes while the trial compacts to a single-state 0-bit section and 120 bytes. The trial grows back to 5 bits and 3,048 bytes after the 31 values are reintroduced. At each of four high-churn checkpoints the baseline remained 15-bit and the trial 8-bit. Both produced the same 4,096 decoded values and canonical on-wire bytes at every checkpoint.

## Multi-section batch and on-wire validation

The 384 sections comprise 64 copies each of the following distributions, with different position offsets where relevant — air, single stone, 2, 16, 256 and 512 distinct block states. Biomes cycle through seven distinct values. Every encoded packet is decoded and compared to the source array. Subsequent edits apply 96 writes per section (36,864 operations), then compare all resulting states and canonical packets again.

| Batch metric | Baseline, run 1 | Trial, run 1 | Baseline, run 2 | Trial, run 2 |
| --- | ---: | ---: | ---: | ---: |
| Cold build ns/section | 3,118.8 | 34,346.7 | 3,310.3 | 33,282.6 |
| Cold requested heap bytes, all 384 | 1,137,152 | 5,691,392 | 1,137,152 | 5,691,392 |
| Cold allocation calls, all 384 | 449 | 2,817 | 449 | 2,817 |
| Retained estimate, bytes, all 384 | 1,137,152 | 1,200,640 | 1,137,152 | 1,200,640 |
| Clean serialization ns/section | 289.7 | 285.0 | 274.2 | 269.0 |
| Steady edit ns/write | 218.6 | 519.5 | 215.3 | 560.0 |
| Steady requested heap bytes, 36,864 edits | 7,797,432 | 16,020,992 | 7,797,432 | 16,020,992 |
| Steady allocation calls, 36,864 edits | 25,012 | 10,624 | 25,012 | 10,624 |
| Dirty canonical rebuild+serialize ns/section | 4,249.6 | 44,470.1 | 5,533.9 | 41,147.2 |

- Total canonical wire size before edits — **1,092,672 bytes** for 384 sections, identical between both implementations.
- Total canonical wire size after edits — **1,801,125 bytes**, identical.
- Shared strict wire decoder — **3,136.0 ns/section** on run 1 and **2,998.1 ns/section** on run 2. Both representations produce identical packets, so one decoder measurement applies to both.
- Verified decode, reconstruct both representations, and reassert all values and wires — **64,765.0 ns/section** on run 1, **64,518.7 ns/section** on run 2 (includes expensive differential assertions).
- Whole-process `VmRSS` after constructing both representations and retaining inputs — **11,340 KiB** and **11,344 KiB**, respectively. These are *not* per-implementation isolated RSS figures.
- Following edits, **301 of 384** production resident `write_wire` buffers differ in bytes from a fresh, canonical repack due to retained unused palette entries. Their decoded states are identical. Canonical-equivalence assertions compare a freshly packed production section to the trial's canonical writer, and independently decode the resident production packet. Any production delivery code that requires byte identity must honor `is_clean_wire()` and repack when false.

## Verification and scope

- `cd tools/palette_bench && cargo test --offline` — **PASS**, 3/3 tests.
- `cd tools/palette_bench && cargo run --release --offline` — **PASS** twice, `identity_assertions=PASS canonical_wire_assertions=PASS reload_assertions=PASS`.
- `bash tools/layers_check.sh` — **PASS**, 44 modules, 111 edges, 0 named errors. The `gate_probe.py` output prints expected negative-case diagnostics.
- `bash tools/debt_ratchet.sh` — **PASS**, 0 over limit.
- `python3 tools/provenance_check.py` — **PASS**, no tracked/referenced protected upstream assets.
- No production storage API, repository-level dependency, runtime state, or license source was changed. All benchmark code lives in `tools/palette_bench/`. Results are synthetic microbenchmarks on this hardware. A production change should be justified by real chunk-load and edit-rate distributions followed by a separate, equivalently configured end-to-end run.
