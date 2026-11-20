# Storage durability gate — 2026-11-20

**Decision: NO-GO for a storage rewrite.** The clean-room WAL changes counted write amplification only slightly at this workload and had no measured checkpoint-time advantage. The indexed-sector prototype was substantially more expensive. These measurements are a mechanism comparison, not production-world evidence.

## Scope and evidence identity

- Checkout: `mc-spine`, branch `t3/storage-durability-bench`, starting revision `5759cbef5e5bf9f809365c6b2a4cc2fd9d9d57a9`.
- Workload: synthetic sparse overlay with one changed cell in each of 64, 512, or 2,048 chunks.
- Host: Linux 6.18.60-2-cachyos-lts, x86_64, AMD Ryzen 7 9800X3D, ext4 on `/dev/nvme0n1p5`.
- The GitHub issue connector returned 404 for the requested issue and its search timed out; web retrieval also failed. The issue's full body and acceptance criteria are **UNKNOWN**. The title and gate request in the task brief are the only issue-specific inputs used here.
- `tools/storage_bench` is clean-room std-only Rust. It has no external crates and does not import FerrumC, LMDB, or an mmap implementation. The WAL and indexed-sector variants test bounded storage mechanisms only; the indexed-sector prototype is not Minecraft Anvil-compatible.

## Current persisted format

mc-spine does not persist generated terrain as Anvil region/chunk files. Terrain is generated from the configured world generator; player edits are sparse overlays. `src/main.rs:10722-10825` defines the overlay and companion-file readers/writers. `src/main.rs:8493-8507` defines `Region` as in-memory hot/warm chunk sets; this is not a persisted region store.

The save files are `world_overlay.bin` for the Overworld, and `world_overlay_nether.bin` / `world_overlay_end.bin` for non-Overworld edits. Each overlay table is: chunk count as Minecraft VarInt; for each chunk, big-endian `i32 cx`, big-endian `i32 cz`, cell count as VarInt; then each cell's big-endian `u32` local index and block-state ID as VarInt. HashMap iteration supplies the current writer order, so a byte-for-byte stable ordering is not promised. A companion file contains only its dimension's table; the Overworld file starts with its table and continues with world metadata.

New files use an `SPN4` envelope: 4-byte magic, 8-byte big-endian payload length, 20-byte SHA-1 of the payload, then the payload (`src/main.rs:10986-11010`). SHA-1 detects accidental corruption; it is not authentication. Legacy unwrapped v2.37 saves remain accepted. The Overworld payload can also carry `MET2`/`MET3`/`MET4` metadata, tick state ending in `TIK1` (`src/tick_persistence.rs`), and later dimension, loot, and drop-owner footers (`src/main.rs:11164 onward`, `src/main.rs:11753 onward`, `src/main.rs:11924 onward`, `src/main.rs:12099 onward`, `src/main.rs:12212 onward`).

`src/nbt.rs` separately reads named big-endian NBT, rejects truncation, invalid collection lengths, unknown tags, and trailing bytes, and bounds gzip expansion by the caller's maximum. NBT is not the outer overlay save encoding. `src/landing.rs` commits a validated block change through `set_block_if_dim`; the normal overlay save path persists it later.

## Atomic write and recovery guarantees

`write_world_file` writes the complete envelope to `<path>.tmp`, calls `File::sync_all`, optionally copies the current file into `.bak` and rotates `.bak.1` / `.bak.2`, then renames `.tmp` over the live path (`src/main.rs:11107-11162`). It does not fsync the containing directory after rename. Backup copies are not explicitly fsynced. Backup rotation failures are logged but do not block the live save. The writer reports save errors; startup reads the live path only and does not fall back to `.bak` or `.tmp` (`src/main.rs:12268-12302`). A missing live file is treated as an empty save. Invalid protected saves cause startup to exit before accepting clients (`src/main.rs:31058-31060`).

For `SPN4` files, the reader rejects a short header, a payload-length mismatch, or a SHA-1 mismatch (`src/main.rs:10994-11010`). This protects against ordinary torn/truncated writes and accidental bit flips. It does not prove power-loss durability of rename metadata, recover the last good backup automatically, or authenticate files. Legacy unwrapped saves have no envelope checksum. Also, the overlay table parser itself can stop at truncated chunk/cell data without returning an error (`src/main.rs:10777-10802`); the outer checksum is what protects a normally written `SPN4` payload from such partial parses. Companion file parsing rejects trailing bytes.

The harness's filesystem crash cases are deterministic controls, not actual machine power cuts. Its out-of-order-fsync control models an index becoming visible before its referenced record is durable; it verifies the prototype's bounds check rejects the dangling index. No hardware or filesystem crash injection was run.

## Measurements

Command: `cargo run --release --offline --manifest-path tools/storage_bench/Cargo.toml`, with `TMPDIR="$PWD/target/tmp"`.

Write amplification is counted submitted application bytes divided by 5 logical changed bytes per cell (4-byte index + 1-byte state VarInt). Current-format writes count the full synced temporary snapshot plus the prior snapshot copied to `.bak`. WAL writes count appended records (sequence, coordinates, index, state, 4-byte digest) followed by one `sync_all`. Indexed writes count 4 KiB chunk records plus 4-byte index entries, with record `sync_all` before index `sync_all`. The timings below are one observed run, in microseconds; storage metadata, block-level read/modify/write, device cache, and variance are not included in byte counts.

| Mutated chunks | Format | Bytes written | Write amplification | Checkpoint µs | First read µs | Warm read µs |
|---:|---|---:|---:|---:|---:|---:|
| 64 | Current full snapshot + backup | 1,858 | 5.806× | 38,561.2 | 36.7 | 0.2 |
| 64 | Append WAL | 1,792 | 5.600× | 136,254.5 | 24,545.9 | 0.1 |
| 64 | Indexed-sector prototype | 262,400 | 820.000× | 338,501.8 | 674.1 | 10.9 |
| 512 | Current full snapshot + backup | 15,174 | 5.927× | 23,820.5 | 39.5 | 0.5 |
| 512 | Append WAL | 14,336 | 5.600× | 41,578.4 | 317.1 | 0.4 |
| 512 | Indexed-sector prototype | 2,099,200 | 820.000× | 137,245.5 | 3,470.4 | 141.1 |
| 2,048 | Current full snapshot + backup | 60,904 | 5.948× | 68,999.4 | 1,915.1 | 0.7 |
| 2,048 | Append WAL | 57,344 | 5.600× | 109,576.3 | 233.9 | 1.2 |
| 2,048 | Indexed-sector prototype | 8,396,800 | 820.000× | 414,598.6 | 4,695.2 | 537.5 |

“First read” reopens the file and requests Linux `POSIX_FADV_DONTNEED` for that file; the call returned success in all rows. This is a best-effort page-cache eviction hint, not proof that storage hardware supplied every byte. Warm read is the average of 200 complete sequential reads. These are whole-file reads, not random single-chunk lookup measurements. The small WAL lead is 3.6–5.9% fewer counted bytes. It was faster at 64 edits, slower at 512 and 2,048 edits in this single run. The indexed prototype pays one 4 KiB sector per changed chunk and loses badly on these sparse records. Checkpoint and cold-read times vary with filesystem/device state; they are observations from one pass, not a stable ranking.

## Durability and recovery matrix

| Control | Harness result | Scope / limit |
|---|---|---|
| Truncated `SPN4` temp before rename | PASS — rejected by payload length; previous live file remains readable | Synthetic envelope and temp-write model |
| Mid-file byte flip in `SPN4` | PASS — rejected by SHA-1 mismatch | Accidental corruption detection only |
| Stale `.tmp` beside valid live file | PASS — live save loads; temp is ignored | Reader does not clean stale temp files |
| Truncated WAL append | PASS — partial fixed-size record rejected | Prototype record parser |
| Mid-record WAL byte flip | PASS — record digest mismatch rejected | 32-bit truncated SHA-1 in clean-room prototype is not production-grade integrity design |
| Index fsync before referenced record fsync | PASS — dangling index rejected as beyond durable file length | Deterministic ordering model, not a host crash |
| Actual abrupt process kill / power loss | NOT RUN | Would not establish hardware power-loss behavior without a crash-capable test setup |

Harness tests: **6 passed, 0 failed**. The deterministic durability runner reports **6 PASS**. No production `mc-spine` source was changed, and no real world save was used.

## Gate conclusion

**NO-GO: do not rewrite mc-spine storage based on this evidence.** The current save already snapshots sparse edits, validates new-file length and checksum, syncs the temporary file before replacement, and retains backups. The prototype WAL reduces counted writes by only a few percent; checkpoint times were noisy and did not show a consistent advantage in this run. The indexed-sector layout is a poor fit for one-cell sparse mutations in this analogue. Neither prototype has real-world workload, restart, power-loss, compatibility, or migration evidence. A later proposal needs the actual Issue #42 criteria, representative saved worlds and mutation distributions, backup/recovery policy, and restart/crash evidence before it can pass a rewrite gate.

## Verification

- `cargo test --offline --manifest-path tools/storage_bench/Cargo.toml` — PASS (6/6).
- `cargo run --offline --manifest-path tools/storage_bench/Cargo.toml -- --durability` — PASS (6/6 controls).
- `cargo run --release --offline --manifest-path tools/storage_bench/Cargo.toml` — completed; table above.
- `bash tools/layers_check.sh` — PASS (44 modules, 111 edges; current debt named, not resolved).
- `bash tools/debt_ratchet.sh` — PASS (0 over limit).
- `python3 tools/provenance_check.py` — PASS (clean).
