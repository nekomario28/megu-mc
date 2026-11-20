# Phase E Wave 2 / E2-3 structure HOLD recheck and Batch B4 (2026-11-20)

## Frozen provenance and comparison

- Port checkout: `e1e75eafdc6ab44ea5a5d24812aadaaa0040fd96` (`t3/structure-hold-b4-triage`), including ancestor `d139913` (`valid_biome` quart lookup). Rebuilt via `cargo build --offline --locked`; `target/debug/mc-spine` SHA-256 `c319cd02bace1e25fa3450a97878babfde693a5581bbce9c5438f8b35e8cadd7`.
- Native authority: Java 1.21.1 `/home/yuu/tmpx/carverfresh/server.jar`, SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`; Java 21 `/usr/lib/jvm/java-21-openjdk`. Mojang mappings: `/home/yuu/mc-spine-local/audit/historical/fossil-placement-20261106/server.txt`, SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`.
- Seed `4242`, Overworld, frozen native `structures` stage, Y `-64..319`, **98,304** full-state cells per 16×16 chunk. `tools/port_triage.py` used `--mode full` (`noaq=False`), **zero excluded attributes**, and the original native world. New candidates used `tools/structure_capture.py` with actual `structures.starts` and `Children` verified in saved NBT. Captures, dumps, reports, Java disassembly, and gate logs live **outside Git** in `/home/yuu/mc-spine-local/captures/matrix-b4/` (`matrix-v3/` for original B3 natives). `TMPDIR` and Java `java.io.tmpdir` used workspace `target/tmp`; Java `-XX:-UsePerfData` was set.

## Measured rows

The native and port grid SHA-256 values cover the complete 393,216-byte canonical chunk grid, including air and all block-state properties; `order.tsv` hashes bind the separate population-order input. `Before` for B3 means the [Batch B3](structure-batch-b3-20261120.md) comparison before this recheck. B4 candidates have no previous promoted measurement.

| Structure | Chunk `(cx,cz)` | Verified native `Children[].BB` example | Before → after diff / 98,304 | Verdict |
|---|---|---|---:|---|
| `minecraft:buried_treasure` | `(126,-182)` | `[2025,57,-2903,2025,57,-2903]` | **12 → 12** | **HOLD** |
| `minecraft:swamp_hut` | `(-332,-339)` | `[-5312,63,-5424,-5306,69,-5416]` | **14 → 14** | **HOLD** |
| `minecraft:jungle_pyramid` | `(137,-117)` | `[2192,65,-1872,2203,74,-1858]` | — → **1** | **HOLD** |
| `minecraft:ocean_ruin_warm` | `(-51,-192)` | `[-816,38,-3072,-801,53,-3057]` (7 children) | — → **0** | **PROMOTED** |

| Fixture | Native `native.bin` SHA-256 | Current port `dump.bin` SHA-256 | Native `order.tsv` SHA-256 |
|---|---|---|---|
| B3 `buried-treasure` | `176534289d27ae4b7c120beb1f01f4d150ebf7e53d613b28300f012f4f2cb1e8` | `fee9aaf98ea72f816c9ee305892eb1b83037d13534e5257014cd13178853ad42` | `f1bf39be506cc50f190e0d40517eb42179353efdf1eb3fdec9789bc0aa56df0f` |
| B3 `swamp-hut` | `1a6ea3a1b385dadefc7bf8c88a65e9609a10c59e7f91c7a832ac8206411c7c43` | `72d89c232b1cd5d97f8860844ce5ea24d46bb69e6334d510e230468d0ff8a2b5` | `0eb6743e48bf5314b825779448dc45760cc5f452297b48b5e59e56f8938e4b74` |
| B4 `jungle-pyramid` | `01326d6bb88f381d17f9769af03eb56acecc67c0b6df4e26cee789a749a3e524` | `1213ef68793689fc2efdfaeffd66c36d0848b6fd9401640ba935e53b588afa99` | `8fd4c20b0c5083a88c6c1cf1e34f7d741d90e075bdd45471a8ac9431e879d125` |
| B4 `ocean-ruin-warm` | `b602655e6d6455d91189e3fef6cbf42b0368619a09a0658b734de0a150e47702` | `b602655e6d6455d91189e3fef6cbf42b0368619a09a0658b734de0a150e47702` | `da8738cb919e6c0062fa81ff5ee3dd0b2e47d927b22b71e49dd012b9ded831ad` |

## Diff ownership and native-class inspection

- **Buried treasure, 12 HOLD.** Every mismatch is native `minecraft:sand` versus port `minecraft:gravel`, in beach terrain at `x=2016..2017`, `y=46..47`, `z=-2904..-2897`, **outside** the treasure's one-cell piece BB. The first divergent terrain writer remains **UNKNOWN**. The native `BuriedTreasurePieces$BuriedTreasurePiece` bytecode (`elk$a`, `native-bytecode/buried-treasure-piece.javap`) was inspected; the measured terrain cells do not justify a patch to the allowed structure files.
- **Swamp hut, 14 HOLD.** Ten differences fall in mineshaft corridors `mscorridor#70` (4) and `#72` (6), including stone/cave-air/cobweb/rail/spawner states. Two native lily pads versus port air occur at `(-5312,63,-5418)` and `(-5308,63,-5418)` inside the hut's BB, plus one outside at `(-5310,63,-5413)`; native air versus port brown mushroom occurs at `(-5310,65,-5420)`. Native `run.log` lines 1113–1115 identify feature `57,9` as the lily-pad writer. The native `SwampHutPiece` (`eml`) and `RandomPatchFeature` (`edc`) bytecode were inspected (`native-bytecode/swamp-hut-piece.javap`, `random-patch-feature.javap`). The first diverging port writer is still **UNKNOWN**; no bounded fix in permitted files was established.
- **Jungle pyramid, 1 HOLD.** At `(2207,72,-1857)`, **outside** the pyramid's BB, native `minecraft:oak_leaves[distance=4,persistent=false,waterlogged=false]` differs from port `minecraft:oak_leaves[distance=3,persistent=false,waterlogged=false]`. The first divergent leaf-distance update is **UNKNOWN**; no substantiated structure-kernel fix in the permitted files.
- **Warm ocean ruin, 0 PROMOTED.** Full typed equality, with an actual native start and seven child BBs. No Rust code change was required. Reusing the same frozen capture and current binary via `tools/structure_capture.py --capture-dir ... --chunk -51 -192 --manifest /home/yuu/mc-spine-local/captures/matrix-v2/manifest-promotable.json` appended row `ow-4242-structures--51--192--64-384`. The row binds the native level, region, run log, population-order input, and port dump by SHA-256. Exact inputs and hashes are in `b4-ocean-ruin-warm-promotion.json`. The promotable manifest now has **18 rows**; B3 HOLDs and the B4 jungle HOLD are excluded.

The inspected disassemblies are from the pinned server's internal version jar, resolved through the pinned Mojang mappings. No root cause was proven in `src/structure_pipeline.rs`, `src/structure_template.rs`, or `src/end_city_mansion.rs`; therefore **no Rust source or regression tests were changed**. This report establishes parity only for these captured windows and stage; it does not verify all placements, block entities, loot, or later gameplay.

## Reproduction and validation

- B3 recheck: `tools/port_triage.py <matrix-v3/nat-4242-b3-*/world> <cx> <cz> --n 1 --mode full --preset overworld --height-window noise --binary "$PWD/target/debug/mc-spine"` (two captures), results `matrix-b4/hold-recheck/{buried-treasure,swamp-hut}/triage.json`.
- B4 generation: `tools/structure_capture.py --structure minecraft:<id> --seed 4242 --dimension overworld --root /home/yuu/mc-spine-local/captures/matrix-b4 --scan-radius 192 --max-attempts 4 --jar-base /home/yuu/tmpx/carverfresh --binary "$PWD/target/debug/mc-spine"` for `jungle_pyramid` and `ocean_ruin_warm`. Reports `b4-{jungle-pyramid,ocean-ruin-warm}-report.json`, comparisons under `dumps/nat-4242-b4-*/triage.json`, native starts in `nat-4242-b4-*/world/`. An exploratory `pillager_outpost` scan found no admitted candidate within a 192-chunk radius; no capture or verdict was assigned.

| Validation command | Exact observed result |
|---|---|
| `python3 tools/fixture_suite.py /home/yuu/mc-spine-local/captures/matrix-v2/manifest-promotable.json --out /home/yuu/mc-spine-local/captures/matrix-b4/b4-suite-final` | `fixture_suite: {"DIFF": 0, "INVALID": 0, "MATCH": 18, "NOT_RUN": 0}` |
| `cargo test --offline --locked -- structure --test-threads=1` | `test result: ok. 77 passed; 0 failed; 0 ignored; 0 measured; 950 filtered out; finished in 71.75s` |
| `bash tools/layers_check.sh` | `source graph: 44 modules, 111 edges, 0 named errors` / `layers check: PASS (current debt is named, not resolved)` |
| `bash tools/debt_ratchet.sh` | `ok    wall-clock reads (clock RNG must not return): 0`, `ok    ignored tests (parked gates): 1`, `debt ratchet: 0 over limit` |
| `python3 tools/provenance_check.py` | `provenance: clean - none of assets/pblocks.json, assets/properties.json, resourcepack.zip is tracked or referenced` |

All validation commands returned exit code **0**. Full command outputs are preserved under `matrix-b4/{cargo-structure-test,layers-check,debt-ratchet,provenance-check}.log` and `b4-suite-final/result.json`.
