# Phase E Wave 3 / E3-9 Batch B5 Nether and End structure parity (2026-11-20)

## Frozen native reference and full-state comparison

- Port revision `5759cbef5e5bf9f809365c6b2a4cc2fd9d9d57a9`, branch `t3/nether-end-structure-parity`. `cargo build --offline --locked` passed. The rebuilt `target/debug/mc-spine` has SHA-256 `18a290e379c83672591d0d44dbf8b956f0485d1283513b5dd4a45843ddf59902`.
- Pinned Vanilla 1.21.1 reference `/home/yuu/tmpx/carverfresh/server.jar` has SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`; runtime is `/usr/lib/jvm/java-21-openjdk`. Native captures used frozen `structures` stage, seed `4242`, and dimension-specific world folders `DIM-1` and `DIM1`.
- `tools/structure_capture.py` supports `--dimension nether` and `--dimension end`. It reused each capture only after verifying its saved `structures.starts` and `Children[]`. `tools/port_triage.py` reported `mode=full noaq=False` with no excluded attributes. Each comparison covered all **65,536 typed block-state cells** in one 16×16 chunk from Y `0..255`, including air kinds and state properties. Each canonical binary grid is 262,144 bytes.
- Artifacts, triage reports, native logs, and promotion receipts are outside Git at `/home/yuu/mc-spine-local/captures/matrix-b5/`. `TMPDIR` and Java `java.io.tmpdir` used workspace `target/tmp` with `-XX:-UsePerfData` for the native workflow.

## Verified structures

| Native structure | Dimension and chunk `(cx, cz)` | Saved `Children[]` | Example native child `BB` | Full typed-cell diff | Verdict |
|---|---|---:|---|---:|---|
| `minecraft:fortress` | Nether `(-25, 6)` | 90 | `[-398,55,98,-380,64,116]` | **0 / 65,536** | **PROMOTED** |
| `minecraft:nether_fossil` | Nether `(-22, 20)` | 1 | `[-356,55,319,-350,61,324]` | **0 / 65,536** | **PROMOTED** |
| `minecraft:end_city` | End `(65, 45)` | 43 | `[1047,62,727,1056,65,736]` | **0 / 65,536** | **PROMOTED** |

End City was found on an Outer End island at approximately `sqrt(65²+45²) = 79` chunks from the origin, beyond the central island. Native NBT had a real start and 43 children. All three comparisons completed with zero skipped chunks, zero full-state differences, and equal native/port non-air cell counts (fortress `26,777`, fossil `24,688`, End City `14,977`). No root-cause patch or regression test in the owned Rust modules was warranted by these windows.

## Artifact SHA-256 hashes

The `native.bin` and port `dump.bin` hashes below cover the complete canonical grid for each compared chunk. The population `order.tsv`, native `run.log`, saved `level.dat`, and applicable region file are independently bound in the promotable manifest.

| Structure | Native `native.bin` and port `dump.bin` SHA-256 | Native `order.tsv` SHA-256 | Native `run.log` SHA-256 |
|---|---|---|---|
| fortress | `4df69f134515e18fb4e6afe7f15e8905a52cc4607ffe65a3d03658e27f643d92` | `7fb14b7ffce1ff4220254ca2b8582da4e430ac06e66ebec333ab8a6b8a015d29` | `3c7ca1c7803b429a11416e371f58aecb9ca39bd466e5b9d5369dfa5a641b973d` |
| nether_fossil | `b6628d89652caa1ba6cb1ffc66f2ea1781afea501f87e7e0f3edb9b64a1f0f90` | `0f3b50d763e088e9df27fb23ba4f1326689f2992d8dd0e231e7e1a7fe36d127c` | `1c34bebb342aa8f2449a13fff4c870d83f5f5832d80d48f8a54575e3b2e5a674` |
| end_city | `124c58c4baa2081bf85a4bdfb0795fab6ba79d9cf93ad19e4c0a7d6ce92fc03c` | `86724f8e8199c9a689514468b3aeba5f68865fb0ed6fd76f851df7ed505b37b0` | `a5cf6667096d63000731f8836dfa2f161502126682a1bdf91fc0effb07e65d0e` |

| Structure | Saved `level.dat` SHA-256 | Saved dimension region SHA-256 |
|---|---|---|
| fortress | `cbfae3a5bda8df66749ef71fbab15f7dc8f2b7a9fe01f04d5214236f3beea17c` | `dc48fd42c2ad95ef543ae86b9f2f8a265e8137861f4d5b2036c465e567c68238` (`DIM-1/region/r.-1.0.mca`) |
| nether_fossil | `e896d35e4384bab98d7d1bf22c50ccbe3a8004738994bb3c15bf64fde3e90663` | `34dcecb420d1e678026754f17460789979445c4e2b3b9c55f25a721d0d25aa43` (`DIM-1/region/r.-1.0.mca`) |
| end_city | `5a2d00b491f4ba08a6697c11f59180c0d7fa029e594562712fb89dddcf0eb6ee` | `3c9500bcc81ed7f15426f45f5f62f6a3eaa768d551684cf2ba9e74d3e41315f8` (`DIM1/region/r.2.1.mca`) |

## Fixture promotion

The three zero-diff captures were replayed through `tools/structure_capture.py --capture-dir ... --height-window dimension --manifest /home/yuu/mc-spine-local/captures/matrix-v2/manifest-promotable.json` with unique B5 row IDs.

- `b5-20261120-fortress-4242--25-6-0-256`
- `b5-20261120-nether-fossil-4242--22-20-0-256`
- `b5-20261120-end-city-4242-65-45-0-256`

The existing **18 rows were checked for exact JSON-value equality** before and after promotion. The manifest now contains **21 uniquely identified rows**, SHA-256 `ab21e95a583708db9d3ec77ce69d5694a657f7e3f8176abecd1a1e1b4f287326`. Its bound fortress `run.log` records 16 native nether-brick-fence runtime writes, declared and verified by the fixture suite. Promotion receipts are `b5-{fortress,nether-fossil,end-city}-promotion.json` in `matrix-b5/`.

## Verification

| Command | Observed result |
|---|---|
| `cargo build --offline --locked` | **PASS** |
| `cargo test --offline --locked -- structure --test-threads=1` | **PASS**, 77 passed, 0 failed, 0 ignored, 952 filtered out |
| `bash tools/layers_check.sh` | **PASS**, source graph 44 modules, 111 edges, 0 named errors |
| `bash tools/debt_ratchet.sh` | **PASS**, wall-clock reads 0, ignored tests 1, debt ratchet 0 over limit |
| `python3 tools/provenance_check.py` | **PASS**, provenance clean |
| `python3 tools/fixture_suite.py /home/yuu/mc-spine-local/captures/matrix-v2/manifest-promotable.json --out /home/yuu/mc-spine-local/captures/matrix-b5/b5-suite-final` | **PASS**, `MATCH=21`, `DIFF=0`, `INVALID=0`, `NOT_RUN=0` |

All listed commands exited with code 0. Gate logs are `matrix-b5/{cargo-structure-test,layers-check,debt-ratchet,provenance-check,fixture-suite}.log`; suite results are `matrix-b5/b5-suite-final/result.json`. The measurements establish exact parity for these three frozen chunk windows at the `structures` stage. Additional structure locations, block entities, loot, and later gameplay stages were outside this measured scope.
