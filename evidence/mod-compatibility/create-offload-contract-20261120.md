# Create / Factory Java-to-Native offload probe

Date: 2026-11-20
Scope: clean-room Java 21 ↔ Rust JNI ABI probe; no Minecraft or mod runtime is loaded.

## Frame contract

Java writes into a four-slot ring of fixed 65,536-byte `ByteBuffer.allocateDirect` buffers. The current probe is single-threaded; a production ring needs explicit producer/consumer ownership and synchronization before concurrent use. The configured ring footprint is **262,144 bytes**. Each used frame has a 24-byte little-endian header:

| Offset | Width | Meaning |
|---:|---:|---|
| 0 | 4 | Magic `0x3546464f` (`OFF5` bytes) |
| 4 | 1 | ABI version `1` |
| 5 | 1 | Scenario: `1` topology delta, `2` pose + palette diff, `3` server logic output |
| 6 | 2 | Primary record count |
| 8 | 8 | Monotonic generation, scoped independently per scenario |
| 16 | 4 | Payload byte length |
| 20 | 4 | CRC-32/IEEE over payload |

Rust gets the direct address and capacity through JNI, checks capacity and frame length before slicing, then validates magic/version, CRC, record shape, values and generation before accepting. It decodes every record in the payload. It fails closed with negative results: `-1` header/version, `-2` length/CRC/count limit, `-3` unknown scenario, `-4` record shape, `-5` required pose count, `-6` stale/out-of-order generation, `-7` invalid logic values, `-8` invalid decoded record fields, `-11` invalid capacity, `-12` missing direct address, `-13` payload length beyond capacity. Generations advance only after all checks pass.

Scenario payload shapes:

- **A — kinetic topology:** 2,000 compact 8-byte deltas (`u16 node_id`, `u8 kind`, `u8 flags`, `f32 value_delta`). The per-node-call comparison puts one 8-byte record in each independently framed JNI call. The fixed batch carries the 2,000 records in one frame.
- **B — contraption stream:** four 52-byte pose records (`u32 contraption_id`, `u32 generation`, `f64 x/y/z`, `f32 yaw/pitch/vx/vy/vz`) followed by 32 six-byte dirty-palette entries (`u16 block_index`, `u16 palette_id`, `u16 flags`). The 1,024 static interior blocks are not resent.
- **C — server/client boundary:** one 24-byte server-output record (`f32 speed`, `f32 stress_units`, `u32 active_slots`, `f32 pose_delta x/y/z`). Rust checks finite floats and bounded active slots and accepts it. Client rendering remains an ownership contract for Flywheel/Sodium-style integrations; neither client mod is loaded or behavior-tested here.

## Measurements

Command: `TMPDIR="$PWD/target/tmp" python3 tools/offload_ffi_probe/run_probe.py` (run inside the host's bounded compute scope). Java 21, optimized standalone Rust `cdylib`, 2,000 warm-up iterations, 10,000 measured batch samples; the per-node comparison uses 100 samples of 2,000 calls. Times are microseconds, p50/p95/p99. Serialization is Java payload construction. “Rust deserialize/validate” includes ring-frame fill, JNI crossing, CRC, field decoding and semantic validation; it is not isolated native instruction time.

| Measurement | p50 | p95 | p99 |
|---|---:|---:|---:|
| A: Java serialize 2,000 deltas | 24.055 | 48.922 | 88.808 |
| A: direct-buffer JNI + Rust deserialize/validate 2,000 deltas | 175.431 | 180.491 | 186.924 |
| A: 2,000 per-node JNI calls, total | 276.643 | 360.632 | 386.631 |
| B: Java serialize 4 poses + 32 palette diffs | 0.191 | 1.112 | 3.547 |
| B: direct-buffer JNI + Rust deserialize/validate pose/diff frame | 4.408 | 4.779 | 4.989 |

Bandwidth accounting:

- **A:** 64,000 bytes as 2,000 separate 8-byte records + 24-byte headers; 16,024 bytes batched. **4.00× fewer bytes** in this framing model.
- **B:** 4,096 bytes/tick to resend 1,024 static blocks at the modeled 4 bytes/block; pose frame is 208 pose bytes + 192 dirty-diff bytes + one 24-byte header = **424 bytes/tick**, about **9.66× fewer bytes**. These are probe payload sizes, not Minecraft packet captures.
- **Ring:** 4 × 65,536 = **262,144 bytes** reserved by the Java harness.

Tests report `PASS`, 8 cases: A/B/C accepted, corrupt header rejected, corrupt CRC/payload rejected, 65,537-byte direct buffer rejected, stale generation rejected, and next generation accepted.

## Evidence boundary

This demonstrates the standalone JNI/direct-buffer ABI shape, frame rejection behavior, compact framing arithmetic, and local microbenchmark timings. It does not establish production Minecraft Create integration, arbitrary mod compatibility, thread-safe ring scheduling, network bandwidth, server tick impact, collision correctness, Flywheel/Sodium rendering, or comparative performance in a live game. Scenario C confirms the sample values cross and pass validation; client visual ownership is specified, not exercised.
