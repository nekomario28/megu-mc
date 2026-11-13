# Chunk-bounded Anvil reads

## Contract and reason

`tools/vanilla_region_blocks.py::chunk_bytes` is shared by saved-world triage,
heightmap, ore and tick-inventory tools. Reading one chunk must not load unrelated
region payloads. It now seeks to the location entry and reads only that chunk's
header and compressed payload, checking the declared sector allocation and file
bounds before allocating the body. There is no persistent region cache: a later
call observes a changed saved world instead of a retained snapshot.

The existing gzip / zlib / uncompressed cases remain supported. Unsupported
compression (including external payload markers) remains an error; this change
does not add a codec, a new generation stage, or a worldgen behavior.
Decompression and NBT parsing are unchanged; this is not a general decompression
memory limit or a consistency guarantee for worlds being saved concurrently.

## Measured comparison — 2026-11-13

Base: `033d324ae0114b7f56af84fa147ac58f13ed83b5`.
The before reader was loaded from that exact Git blob, not reconstructed by hand.
The after reader source SHA-256 is
`b3d1d379614bed695fa292ebba2faa1a8b8308c38825fb2b5a6f5179a4fb5363`.

All allocated Overworld chunks in both retained seed-4242 worlds were read:
**1,682 chunks across eight region files**. Before/after arms were interleaved
three times each against unchanged, warm local files. Every returned NBT byte
sequence matched, and region-file hashes were checked before/after measurement.
These allocated chunks include intermediate statuses; they are not 1,682 FULL
chunk parity acceptances.

| Metric, per corpus pass | Whole-region reader | Chunk-bounded reader |
|---|---:|---:|
| Bytes returned by Python file reads | 1,630,871,552 | 1,785,377 |
| Python `read` calls | 1,682 | 5,046 |
| Reader + digest median | 56.53 ms | 33.71 ms |
| Reader + digest min–max | 56.25–57.41 ms | 33.67–33.78 ms |

The eliminated work is whole-region copying for each chunk, not a reduction in
NBT or comparison work. Extra seeks, `fstat`, and small read calls are the tradeoff.
The byte counts are logical reads at the Python API, **not physical disk traffic**.
No total `port_triage`, build, generator, cold-disk, or M3 speedup is claimed.

Full local measurement, source copy, per-chunk digest checks, and logs live at
`~/mc-spine-local/scratch/current/region-range-read-20261113/`:
`measurement.json`, `measure.py`, `verification.json`, and `review.log`.
No world, jar, or captured NBT bytes are added to the repository.

## Failure detection and verification boundary

`tests/nbt_region_test.py` is already called by both `tests/verify.sh` and the
normal CI workflow. Its real-file read-budget test uses a sparse region with
unrelated trailing bytes. The pre-change reader failed by reading 16,777,216
bytes against a 4,148-byte budget. Normal -> old-reader negative control ->
restored normal was observed again after implementation; no source mutation
was left in the worktree.

The focused suite passes for supported compression formats, negative coordinates,
multiple sectors, neighboring slots, re-reading a changed world, missing/truncated
input, invalid allocation/length and existing IEEE-754 NBT semantics. Consumer
checks `chunk_triage_test.py`, `port_triage_test.py`, `carver_diff_test.py`, and
`test_saved_tick_inventory.py` also pass. This is Python reader/consumer
verification, not a compiling Rust mutant or a native generation oracle.

A read-only local Codex review was attempted with a 90-second timeout, but model
connection failed and the command exited 124 without a review. Independent review
is therefore **unavailable**, not passed. Manual diff review was performed.
Rust/native/full `tests/verify.sh` and hosted CI were not run for this change.
