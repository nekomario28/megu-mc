# P2-1-T3 Phase C-2 / Batch B3 structure captures (2026-11-20)

## Scope and provenance

- Base: `da9ae45c94017367fb36283d69576af54afaa37d` (`perf/worldgen-parallel`), isolated worktree branch `t3/da107dc7`.
- Native: pinned official Java 1.21.1 server `<home>/tmpx/carverfresh/server.jar`, SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`. Mappings: `<local>/audit/historical/fossil-placement-20261106/server.txt`, SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`. Java 21: `/usr/lib/jvm/java-21-openjdk`.
- Every row: seed `4242`, Overworld, `structures` stage, `frozen` native capture, `noise` window Y `-64..319` (384 layers), one 16×16 chunk = **98,304 fully typed block-state cells**. Comparison: `tools/port_triage.py` full mode (`noaq=False`), with no attributes excluded. `tools/structure_capture.py` verified an actual `structures.starts` record with `Children`, in addition to candidate admission.
- Capture and comparison artifacts: `<local>/captures/matrix-v3/`. Manifest of **promoted** rows: `<local>/captures/matrix-v2/manifest-promotable.json`. This manifest is local evidence outside Git; the row declarations bind per-file input hashes and runtime-write declarations. The broader `matrix-v3/manifest.json` includes measured HOLD rows and is not a promotable gate.
- Original port build SHA-256 `841f1cb02e0833d162457609b26be234f51b59f76d8ee30345c425fc9d83ea07`. Rebuilt port with the seagrass fix SHA-256 `22bfd27c2c21945925a536267ab020f23fa442c7572e4fef90cdf74b09c3c291` (`cargo build --offline --locked -j 2`). Python, Cargo and Java commands used workspace `target/tmp` as `TMPDIR`; Java was supplied `-Djava.io.tmpdir=... -XX:-UsePerfData`.

## Five verified starts and full-state comparisons

| Structure | Start chunk `(cx,cz)` | Representative native `Children[].BB` (`x1,y1,z1,x2,y2,z2`) | Diff / 98,304 | Result |
|---|---|---|---:|---|
| `minecraft:desert_pyramid` | `(-24,-374)` | `[-384,65,-5984,-364,79,-5964]` | **0** | PROMOTED |
| `minecraft:shipwreck` | `(-377,-379)` | `[-6032,47,-6061,-6024,55,-6034]` | **0** | PROMOTED |
| `minecraft:ocean_ruin_cold` | `(-151,-180)` | `[-2416,39,-2895,-2401,54,-2880]` (12 child entries) | **1 before fix → 0 after fix** | PROMOTED |
| `minecraft:buried_treasure` | `(126,-182)` | `[2025,57,-2903,2025,57,-2903]` | **12** | HOLD |
| `minecraft:swamp_hut` | `(-332,-339)` | `[-5312,63,-5424,-5306,69,-5416]` | **14** | HOLD |

The first `desert_pyramid` candidate `(-56,-367)` was refuted by the saved `structures.starts` (`nat-4242-b3-desert-pyramid/refuted.txt`). Its promoted capture was taken at `(-24,-374)` (`nat-4242-b3-desert-pyramid-2`). `tools/structure_capture.py::native_start` now skips reference-only entries and invalid starts. Regression: `test_a_reference_without_a_start_is_not_a_verified_start`.

## Artifact SHA-256 and result records

All paths in the first column are directory names under `matrix-v3/`. The `native.bin` and `port dump.bin` values are SHA-256 hashes of the **full canonical 393,216-byte chunk grid**, including air kinds and block-state properties. Other bound inputs (region files, `level.dat`, logs, ordering) have their individual hashes in the recipe reports' `row.inputs.sha256` and the promoted manifest.

| Native fixture (`nat-4242-b3-*`) | `native.bin` SHA-256 | `dumps/<fixture>/dump.bin` SHA-256 | `order.tsv` SHA-256 |
|---|---|---|---|
| `desert-pyramid-2` | `541be77d1825a853010badfdcd4b3c68069902ae753412e900b0050dd1f5e396` | `541be77d1825a853010badfdcd4b3c68069902ae753412e900b0050dd1f5e396` | `382b6af95c4d7b5a88a58f1345fad51b9410e23b3bcc67f14a7f7556aff98b9a` |
| `shipwreck` | `23f5559e61b81c10b01a3bd1d2f1651424339a336bef54d19a76fab58f1e4933` | `23f5559e61b81c10b01a3bd1d2f1651424339a336bef54d19a76fab58f1e4933` | `47aa89ea06ba7d54a9221020126902796744c4b4bc750d8dc743c7707c0ae86c` |
| `ocean-ruin-cold` | `eace3a2f657fd712c3bde1c1c34dd51cce7c373a5aab39f7c06e8063f5026e1e` | `eace3a2f657fd712c3bde1c1c34dd51cce7c373a5aab39f7c06e8063f5026e1e` | `449028cfa41c27265217c2352256d664b60eefe42f24b43053c8e256057c5ed2` |
| `buried-treasure` | `176534289d27ae4b7c120beb1f01f4d150ebf7e53d613b28300f012f4f2cb1e8` | `fee9aaf98ea72f816c9ee305892eb1b83037d13534e5257014cd13178853ad42` | `f1bf39be506cc50f190e0d40517eb42179353efdf1eb3fdec9789bc0aa56df0f` |
| `swamp-hut` | `1a6ea3a1b385dadefc7bf8c88a65e9609a10c59e7f91c7a832ac8206411c7c43` | `72d89c232b1cd5d97f8860844ce5ea24d46bb69e6334d510e230468d0ff8a2b5` | `0eb6743e48bf5314b825779448dc45760cc5f452297b48b5e59e56f8938e4b74` |

Per-structure recipe reports are `b3-<structure>-report.json` (desert uses `b3-desert-pyramid-2-report.json`); promotion reports are `b3-desert-pyramid-promotion.json`, `b3-shipwreck-promotion.json`, and `b3-ocean-ruin-cold-promotion.json`. Each capture has its own `boundary.json`, `order.tsv`, `run.log`, and world region files. Zero-diff B3 rows were added to `manifest-promotable.json` as `ow-4242-structures--24--374--64-384`, `ow-4242-structures--377--379--64-384`, and `ow-4242-structures--151--180--64-384`.

## First measured disagreements and action

**`ocean_ruin_cold`, 1 cell, fixed.** Before fix, at block `(-2414,40,-2880)` (inside `minecraft:orp#0`), native `minecraft:water[level=0]` contrasted with port `minecraft:tall_seagrass[half=upper]`. Pre-fix port SHA-256 was `250c36a01ffbc5718230378268dd305d1af06282558745a6884c3a75cc2ba016`, retained at `dumps/nat-4242-b3-ocean-ruin-cold-before-shape-fix/`. The separately captured native trace `nat-4242-b3-ocean-ruin-cold-trace/run.log` identifies the ordered writers at this exact position:

1. `FCELL` at log line 132 writes the original tall-seagrass upper half (state `2009`) during feature placement.
2. `FCELL` at line 166 places gravel (state `118`) at `(-2414,39,-2880)`; line 172 writes air (state `0`) at the now-invalid tall-seagrass upper half via the template boundary shape update (`StructureTemplate.updateShapeAtEdge`, reaching `DoublePlantBlock.updateShape`).
3. `RUNTIME` at line 762 writes water (state `80`) back to `(-2414,40,-2880)` via the subsequent fluid tick.

`src/template_structures.rs::update_shape` now applies the vanilla double-plant vertical-partner rule to tall seagrass. A template change that removes its matching lower half invalidates the upper half. The focused regression `shape_passes_remove_tall_seagrass_upper_when_template_breaks_its_lower_half` verifies both invalid-partner removal and valid-partner preservation. After rebuild the exact original native chunk matches **0 / 98,304**, full-state; fixed port SHA-256 `eace3a2f657fd712c3bde1c1c34dd51cce7c373a5aab39f7c06e8063f5026e1e` (the re-triage is also preserved at `dumps/nat-4242-b3-ocean-ruin-cold-fixed/`). Native repeat `native.bin` at `nat-4242-b3-ocean-ruin-cold-trace` has that same SHA-256. The `ocean_ruin_cold` capture recorded one runtime write; its hash and state census are pinned in the promotable row.

**`buried_treasure`, 12 cells, HOLD.** All 12 are `minecraft:sand` (native) versus `minecraft:gravel` (port), at `x=2016..2017`, `y=46..47`, `z=-2904..-2897`, in beach biome **outside the buried-treasure BB**. Triage assigns them to an underground terrain cell group. The comparison identifies an upstream terrain/surface-state discrepancy and provides no evidence of a shared structure-kernel defect within this lane. Exact earliest native/port terrain writer remains **UNKNOWN**; the full-structures-stage dump alone cannot establish the earliest stage of divergence. No fix or promotion.

**`swamp_hut`, 14 cells, HOLD.** Triage isolates 10 cells in `minecraft:mineshaft` corridor BBs `mscorridor#70` (4) and `mscorridor#72` (6); their stone, cave-air, cobweb, rail and spawner disagreements belong to the mineshaft lane. Within `minecraft:swamp_hut`'s intersecting BB, 2 cells are native lily pads versus port air at `(-5312,63,-5418)` and `(-5308,63,-5418)`, and 1 is native air versus port brown mushroom at `(-5310,65,-5420)`. A fourth surface cell outside structure BB is native lily pad versus port air at `(-5310,63,-5413)`. Native `run.log` lines 1113–1115 show all three native lily-pad writes from feature `57,9` in the generation run. The first missing *port* writer/stage for the lily pads, and the extra port brown-mushroom writer, remain **UNKNOWN** without a stage-by-stage port trace. The observed disagreements do not establish a clean shared structure-kernel fix. No change to the mineshaft implementation and no promotion.

## Verification

| Gate | Observed result |
|---|---|
| `python3 -m unittest tests/structure_capture_test.py` | **PASS**, 16 tests, 0 failures |
| `cargo test --offline --locked -j 2 structure -- --test-threads=1` | **PASS**, 75 passed, 0 failed, 0 ignored, 732 filtered out |
| `python3 tools/fixture_suite.py <local>/captures/matrix-v2/manifest-promotable.json --out <local>/captures/matrix-v3/b3-suite-final` | **PASS**, `MATCH=17 DIFF=0 INVALID=0 NOT_RUN=0` |
| `bash tools/layers_check.sh` | **PASS**, 44 source modules, 111 edges, 0 named errors; baseline named debt remains |
| `bash tools/debt_ratchet.sh` | **PASS**, 0 over limit (wall-clock reads 0, ignored tests 1) |
| `python3 tools/provenance_check.py` | **PASS**, tracked/referenced protected assets clean |

The 17-row suite checks saved native and saved port dumps with pinned hashes and stage metadata. A separate fresh-port regression with rebuilt port binary SHA-256 `22bfd27c2c21945925a536267ab020f23fa442c7572e4fef90cdf74b09c3c291` re-ran all 16 chunk-format promoted rows. The initial run (`matrix-v3/b3-fresh-regression.json`) measured **15 rows at zero diff**, one non-chunk fixture skipped, and **2 / 884,736** differences on the pre-existing village-taiga 3×3 row `ow-4242-structures-4--32--64-384-n3`. That initial invocation omitted the promoted fixture's recorded population order. Re-running that row with `--pop-order <local>/captures/matrix-v1/nat-4242-village-taiga/order.tsv --pop-order-scope recorded` gave **0 / 884,736**, full-state, dump SHA-256 `0dd5a9d8abb350c3d73adaa573aa083a128b8eaa75ef3a5d696618a1d11654e8` (`matrix-v3/b3-fresh-regression/ow-4242-structures-4--32--64-384-n3-recorded/triage.json`). Thus **all 16 chunk-format promoted rows match with their required generation-order inputs** after the Rust change. Full-state parity here applies to the specified chunk coordinates and generation stage; it does not establish parity for all placements, block entities/loot, other runtime metadata, or arbitrary chunk update schedules.
