# Create / Factory Java-to-Native offload probe

On 2026-11-20, this clean-room probe ran OpenJDK 21 ↔ Rust JNI on x86-64 Linux with the real `mod-offload` C ABI from `47d49ff`.

## OFF5 input frame

The single-threaded Java harness owns a ring of four reusable 65,536-byte direct buffers, reserving **262,144 bytes**. Java writes each record directly into a ring slot, then updates `java.util.zip.CRC32` through a reusable view of its payload. The producer keeps the buffer live and unmodified until the synchronous JNI call returns. Each used frame has a 24-byte little-endian header.

| Offset | Width | Meaning |
|---:|---:|---|
| 0 | 4 | Magic `0x3546464f` (`OFF5` bytes) |
| 4 | 1 | ABI version `1` |
| 5 | 1 | Scenario `1` topology delta, `2` pose + palette diff, or `3` server logic output |
| 6 | 2 | Primary record count |
| 8 | 8 | Monotonic generation, scoped by scenario |
| 16 | 4 | Payload byte length |
| 20 | 4 | CRC-32/IEEE over payload |

JNI reads the address and capacity. Rust bounds the supplied frame prefix before it reads the header, checks the declared length and CRC, then validates each record's shape and decoded fields. Negative results include `-1` for magic/version, `-2` for payload length/CRC/count, `-3` for unknown scenario, `-4` for record shape or pose count, `-6` for stale/out-of-order generation, `-7` for invalid logic values, and `-8` for invalid records. Buffer errors use `-10` for null, `-11` for invalid direct capacity, `-12` for missing direct address, and `-13` for a length outside the buffer or ABI bounds. The validation-only JNI call tracks accepted generations per thread. The adapter enqueue call tracks them per region and advances them only after the C ABI accepts the delta.

- **A, kinetic topology.** 2,000 compact 8-byte records with node ID (`u16`), kind and flags (`u8` each), and value delta (`f32`). The per-node arm builds a full header for each record.
- **B, contraption stream.** Four 52-byte poses with ID and generation (`u32` each), position (three `f64`), yaw/pitch and velocity (five `f32`), followed by 32 six-byte dirty-palette entries with block index, palette ID, and flags (`u16` each).
- **C, server logic output.** One 24-byte record with speed and stress (`f32`), active slots (`u32`), and three `f32` pose deltas. The probe validates sample values for the server-to-client ownership contract.

## S5 output through the C ABI

The JNI library links the local Cargo path dependency and calls `mso_region_create`, `mso_region_queue_delta`, `mso_region_step`, `mso_region_export` and `mso_region_free`. It resolves positive Java handle IDs under a registry lock and rejects unknown or destroyed IDs. The fixture also calls the real kinetic, transport, fluid, and recipe C functions. The adapter queues a validated OFF5 frame as the payload of an S5 adapter record (kind `0x100` or higher). The factory fixture drives the kernel through its state APIs.

A step takes an absolute tick and exports directly to a writable Java buffer from byte zero. The low 32 bits of a successful JNI result give the byte count. Bit 32 indicates due work remains and can be retried at the same tick after draining output. The S5 v1 frame starts with a 32-byte header containing magic/version, record count, batch sequence and region key. Each record has a 16-byte header containing tick, source, kind and payload length. S5 uses the ABI's batch sequence. CRC and generation belong to the OFF5 envelope. Empty export writes zero bytes. If the first record cannot fit, the C ABI leaves the buffer and queued record intact. A caller can also drain a partial export by retrying the same absolute tick with adequate capacity. JNI uses `-14` for an invalid handle and `-100 - mso_status` for C-ABI failures. Java rejects read-only output before calling JNI.

The OpenJDK run passed the original eight checks for A/B/C acceptance, corrupt magic and payload, oversized capacity, stale generation and next generation. The S5 checks verified exact headers and payloads for adapter, transport, tank, and recipe records. They also exercised null, heap, read-only and invalid-capacity output, truncated and corrupt input, reserved record kind, stale generation, invalid and destroyed handles, backwards time, short-buffer and partial-export retry, and recovery from a full queue without losing the rejected enqueue's generation.

## Separated OFF5 timings

Run with `mkdir -p target/tmp && export TMPDIR="$PWD/target/tmp"` and `python3 tools/offload_ffi_probe/run_probe.py` inside the host's bounded compute scope. This run used an AMD Ryzen 7 9800X3D, OpenJDK `21.0.13.1+2` with `UseCRC32Intrinsics` and `UseCLMUL` enabled, and Rust `1.98.0` in Cargo release mode. The loaded JNI library had SHA-256 `2cdbadea858c7736584a1c3176aefb1d842ba41a052e1d30b7314b9da1d5b182` and modification time `1795178569022570131` ns since the Unix epoch.

Each batch scenario ran 12,000 warm-up iterations and 10,000 samples. Java timed direct writes, CRC calculation and finalization, and the JNI call around the already-filled buffer as distinct intervals. The JNI interval includes the crossing, direct address/capacity lookup, Rust's table-driven IEEE CRC-32, and full validation/decode. The per-node comparison includes both build and JNI, with 20 warm-up and 100 measured groups of 2,000 calls.

| Measurement (µs) | p50 | p95 | p99 |
|---|---:|---:|---:|
| A, direct payload/header writes | 3.847 | 4.849 | 6.021 |
| A, Java CRC32 and frame finalization | 0.250 | 0.281 | 0.350 |
| A, total direct-buffer frame build | 4.098 | 5.199 | 6.352 |
| A, JNI round-trip + Rust validation/decode | 6.672 | 7.184 | 7.474 |
| A, 2,000 per-node frame builds + JNI calls | 102.303 | 152.838 | 204.637 |
| B, direct payload/header writes | 0.291 | 0.541 | 0.662 |
| B, Java CRC32 and frame finalization | 0.060 | 0.080 | 0.091 |
| B, total direct-buffer frame build | 0.351 | 0.621 | 0.752 |
| B, JNI round-trip + Rust validation/decode | 0.211 | 0.291 | 0.351 |

The previous topology p50s of **24.055 µs** and **175.431 µs** covered different work. The first timed Java heap payload construction. The second timed Java's bit-by-bit CRC and copy to direct memory, the JNI crossing, Rust's second bit-by-bit CRC, and record validation. Both checksum loops contributed to the higher combined figure. The old B Java arm also built only poses while its JNI arm included palette diffs.

The A framing model uses 64,000 bytes for 2,000 independent records and 16,024 bytes batched, a **4.00× reduction**. The B model uses 4,096 bytes to resend 1,024 static blocks at four bytes each and **424 bytes** for the pose/palette batch, a **9.66× reduction**. These are wire-size calculations for the probe.

## Real S5 factory timings

The settled fixture has **10,000 transport machines** (5,000 empty and 5,000 blocked) and **2,000 clean kinetic nodes** across 20 networks. The Java baseline scans all 12,000 entries on each tick. The S5 fixture builds and settles the same workload through the ABI before measurement. Each measured Java/JNI tick returns zero ready work or output, with 2,000 warm-up iterations and 10,000 samples. An additional 1,000,000-tick loop timed inside Rust yielded **5.84 ns/tick mean** for the native C-ABI scheduler step with no export or per-tick JNI crossing.

The mixed fixture has **5,000 dormant and 500 active chains** and was sampled over 250 two-tick cycles after 20 warm-up cycles. The first tick exports 500 transport transfers and 1,000 tank updates (37,532 bytes). The second tick exports 500 recipe events (10,532 bytes). Host actions supply the inputs and collect the outputs through the C ABI. Every eighth cycle they also update the active kinetic components synchronously and apply their solved speeds to the recipes. The two step rows time Java around JNI, scheduler work and export to a reused direct buffer. The complete cycle includes host actions and Java output checks.

| Measurement (µs) | p50 | p95 | p99 |
|---|---:|---:|---:|
| Settled Java full-scan tick | 1.794 | 2.285 | 2.475 |
| Settled S5 JNI step + empty export | 0.080 | 0.081 | 0.090 |
| Mixed transport/fluid S5 JNI step + export | 205.237 | 238.831 | 286.291 |
| Mixed recipe S5 JNI step + export | 162.627 | 188.716 | 208.684 |
| Mixed host actions per cycle | 401.909 | 573.854 | 597.629 |
| Mixed complete two-tick cycle | 774.674 | 960.916 | 990.732 |

The settled comparison measures an event-driven scheduler against a Java full scan. The mixed fixture measures native kernel work and the Java boundary on this host. Minecraft/Create tick impact, concurrent ring behavior, network captures, and Flywheel/Sodium rendering remain unmeasured.
