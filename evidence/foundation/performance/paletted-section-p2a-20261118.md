# M3 Slice P2a: `PalettedSection` Differential & Memory/Pack Audit (2026-11-18)

Base commit: `9fc6414` on `perf/worldgen-parallel`.

## Scope

Standalone `PalettedSection`, `SectionPalette` (`Single`, `Indirect` 4–8 bits, `Direct` 15 bits), and `SectionLightClass` (`Empty | Opaque | Mixed`) in [`src/paletted.rs`](../../../src/paletted.rs), included into [`src/main.rs`](../../../src/main.rs) (`--paletted-bench`). Not yet integrated into `worldgen`, `section_store`, `op_bands`, `map_chunk`, light arrays, or `set_block` (reserved for P2b–P2f).

### Wire Layout Contract (Protocol 767 Non-Straddling `u64` Packing)

Matches [`write_palette`](../../../src/main.rs#L15976-L16021) and [`section_encode`](../../../src/main.rs#L16022-L16036) byte-for-byte (`per = 64 / bits`, `words = 4096.div_ceil(per)`):

| Variant / `bits` | Values / `u64` (`per`) | Word Count | Payload Bytes |
|---|---:|---:|---:|
| `Single` (`0`) | — | 0 | 0 B (48 B struct) |
| `Indirect` (`4`) | 16 | 256 | 2,048 B + palette |
| `Indirect` (`5`) | 12 | 342 | 2,736 B + palette |
| `Indirect` (`6`) | 10 | 410 | 3,280 B + palette |
| `Indirect` (`7`) | 9 | 456 | 3,648 B + palette |
| `Indirect` (`8`) | 8 | 512 | 4,096 B + palette |
| `Direct` (`15`) | 4 | 1,024 | 8,192 B |

`get`/`set` match on `bits` (`4, 5, 6, 7, 8, 15`) so `idx / per` and `idx % per` compile to constant shifts/multiplies without hardware division. Resident palettes promote `0 -> 4 -> 5 -> 6 -> 7 -> 8 -> 15` on `set` and never shrink while resident.

## Release Differential & Performance Audit (>= 10,000 Real Sections)

Command (resource-bounded: `CPUQuota=400%`, `MemoryMax=5G`):

```sh
systemd-run --user --scope -p CPUQuota=400% -p MemoryMax=5G cargo build --release --offline --locked -j 4
systemd-run --user --scope -p CPUQuota=400% -p MemoryMax=5G ./target/release/mc-spine --paletted-bench
```

Dataset (seed `4242`, post-feature `terrain_section_at` with `feature::populate`):
- **Overworld**: `18x18` = 324 chunks × 24 sections = **7,776** sections
- **Nether**: `10x10` = 100 chunks × 16 sections = **1,600** sections
- **End**: `10x10` = 100 chunks × 16 sections = **1,600** sections
- **Total**: **10,976** real sections (`44,957,696` cells)

Measured output (`/home/yuu/mc-spine-local/audit/p2a-paletted-20261118/paletted-bench.log`):

| Metric | Measured Value |
|---|---:|
| Total sections checked | **10,976** |
| `write_wire` vs `section_encode` byte mismatches | **0** |
| `get(i)` vs raw `[u32; 4096]` mismatches | **0** |
| Palette histogram | `Single=7,058`, `4-bit=3,744`, `5-bit=173`, `6-bit=1`, `7-bit=0`, `8-bit=0`, `Direct=0` |
| Mean `approx_bytes` per section (all 3 dimensions) | **802.6 B** |
| Mean `approx_bytes` per Overworld chunk (24 sections) | **20.95 KiB** (vs 384 KiB dense `u32`, ~18.3× smaller) |
| Pure `PalettedSection::pack` throughput (10 rounds × 10,976 sections) | **2,030.6 ns/section** (~48.7 µs per 24-section chunk) |

Synthetic boundary coverage (`paletted_tests::paletted_exact_bit_transitions_and_wire_identity`, `paletted_tests::paletted_cow_set_never_shrinks_and_promotes_all_boundaries`) additionally verifies exact byte-for-byte wire parity and `get`/`set`/CoW invariants across every palette transition (`1, 2, 16, 17, 32, 33, 64, 65, 128, 129, 256, 257, 512, 4096` distinct states, including 15-bit `Direct` IDs `>= 16384`).

## Mutation Gates

Verified with `MUTATION_FILTER=paletted_ MUTATION_STABILITY=1 CARGO_BUILD_JOBS=4 bash tools/mutation_check.sh --offline`:

- `paletted_read_shift_matches_pack_layout` — **ok** (proved, stability=1)
- `paletted_single_returns_stored_state` — **ok** (proved, stability=1)
- `paletted_direct_keeps_all_15_bits` — **ok** (proved, stability=1)
