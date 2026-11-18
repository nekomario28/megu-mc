# Village taiga 1 -> 0 (`P2-1-T0` + `Feature.markAboveForPostProcessing` + `BushBlock`)

Status: **測定で決着。`village-taiga` (4,-32)+3×3 は `1 / 884,736 -> 0 / 884,736` (`OCEAN_FLOOR` 0 列差)。`matrix-v2/manifest-promotable.json` は 11 -> 12 行すべて `MATCH`、exit 0。**

## 0. Base & Reference Provenance

| | |
|---|---|
| base commit | `354128a4e2d9ba44a7e797bf9b4161f397374a7d` (`perf/worldgen-parallel` / `t3/0abcc5e8`) |
| pinned server jar | `/home/yuu/tmpx/carverfresh/server.jar` (SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`) |
| official mappings | `/home/yuu/mc-spine-local/audit/historical/fossil-placement-20261106/server.txt` (SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`) |
| port dump (`dumps/village/dump.bin`) | `0dd5a9d8abb350c3d73adaa573aa083a128b8eaa75ef3a5d696618a1d11654e8` |

## 1. Bytecode Verification (`javap -p -c`)

1. **`Feature.markAboveForPostProcessing` (`ece.a(Ldds;Ljd;)V`)**:
   Steps `Direction.UP` twice (`y + 1`, `y + 2`) from the placed position, reads `level.getBlockState(cursor).isAir()` (`dtc.i()`); if true (including out-of-build-height `VOID_AIR`), returns immediately without marking `y + 2`. Otherwise calls `level.getChunk(cursor).markPosForPostprocessing(cursor)`.
2. **`LakeFeature.place` (`ect.a`)**:
   Calls `markAboveForPostProcessing` at two sites:
   - `y >= 4` (`CAVE_AIR` carve loop, ~line 111): called **only when `y >= 4`**, never when placing `fluidBlockState` (`y < 4`).
   - `barrierBlockState` shell write (~line 139): called unconditionally after `setBlock(..., 2)`.
   The cold-biome `ice` freezing loop does not call `markAboveForPostProcessing`.
3. **`DiskFeature` (`ebx.a`)**:
   Calls `markAboveForPostProcessing` immediately after `level.setBlock(cursor, state, 2)` in `placeColumn` (~line 53).
4. **`BushBlock` (`dgh`) / `DoublePlantBlock` (`dig`) during `LevelChunk.postProcessGeneration` (`dvi.H`)**:
   Enumerating all 30 classes in the `dgh` hierarchy in the pinned jar yields 25 single-block species that inherit `updateShape`, `canSurvive`, and `mayPlaceOn` (`#minecraft:dirt || minecraft:farmland`) unchanged (`TallGrassBlock`: `short_grass`, `fern`; 13 non-wither `FlowerBlock`s; 7 non-azalea/mangrove `SaplingBlock`s; `PinkPetalsBlock`; `SweetBerryBushBlock`), plus the 6 `DoublePlantBlock` / `TallFlowerBlock` species (`tall_grass`, `large_fern`, `sunflower`, `lilac`, `rose_bush`, `peony`) pairing `lower`/`upper` halves. Scoped strictly to `postprocess_shape_marks` (`postprocess_bush_shape`), not shared `update_shape` (which costs 232 diffs at structure-placement time).

## 2. Landed Changes

| change | files |
|---|---|
| **P2-1-T0** (`StructurePiece.placeBlock` `SHAPE_CHECK_BLOCKS`): `TemplateWorld::mark_shape` (default no-op for synthetic `TestWorld`), `FC` override (`self.shape_marks.push(pos)`), called in `place_in_world` on transformed state; `JigsawTarget::mark_shape` called in `place_single_or_legacy` (`Live` only) | `src/template_structures.rs`, `src/jigsaw_structures.rs` |
| **`MarkSrc::FeatureAbove` (`"above"`)**: variant added and stale `MarkSrc` doc comment updated; `feature_population_output_matches_the_pre_refactor_capture` re-pinned (`dcfa9969...` -> `bed420818fc3a0de1d2d26ebc227a4f0`) with seed-0 `(0,0)` and `N=8` untouched | `src/worldgen.rs` |
| **`mark_above_for_post_processing`**: helper in `src/feature.rs` calling `ctx.g.store_shape_mark_from(..., MarkSrc::FeatureAbove)` from `Cf::Disk` and both bytecode sites of `Cf::Lake` | `src/feature.rs` |
| **`postprocess_bush_shape`**: promotion-only `BushBlock` / `DoublePlantBlock` removal inside `postprocess_shape_marks` | `src/template_structures.rs` |
| unit tests & 6 new compiling mutation checks (`MUTATION_STABILITY=1`, registry `488 -> 494`) | `src/jigsaw_structures.rs`, `src/template_structures.rs`, `src/feature.rs`, `tools/mutation_check.sh` |

## 3. Measurements & Gates

| gate | result |
|---|---|
| `matrix-v1/nat-4242-village-taiga` `(4,-32)` 3×3 (`--pop-order … recorded --port-hm`) | **0 / 884,736**, `OCEAN_FLOOR` 0 col diffs (`0dd5a9d8abb350c3…`) |
| `matrix-v2/nat-4242-village-marktrace` `(4,-32)` 3×3 (`--pop-order … recorded --port-hm`) | **0 / 884,736**, `OCEAN_FLOOR` 0 col diffs (`0dd5a9d8abb350c3…`) |
| `t6b/nat-taiga-mlight` `(4,-32)` 3×3 (`--pop-order … recorded --port-hm`) | **0 / 884,736**, `OCEAN_FLOOR` 0 col diffs |
| `MARK_TRACE=all` on `village-taiga` | `MARK 82 69 -500 chunk=5,-32 src=above`; counts: `carver=1173`, `fill=7154`, `write=290`, `above=140` |
| negative controls on `village-taiga` (`--mode terrain`, `--seed 4243`) | `76,736 / 884,736` and `185,879 / 884,736` |
| seed-0 `MC_SPINE_WG_DUMP_N=8` (64 chunks) | `670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def` (25,165,824 B, **bit-identical**) |
| `tools/fixture_suite.py …/manifest-promotable.json` | **12 MATCH / 0 DIFF / 0 INVALID / 0 NOT_RUN**, exit 0 (`ow-4242-structures-4--32--64-384-n3` added) |
| `tools/fixture_suite.py …/manifest.json` | **9 MATCH / 0 DIFF / 0 INVALID / 0 NOT_RUN**, exit 0 |
| targeted tests (`template_structures:: jigsaw_structures:: worldgen:: mark_above_tests::`) | **100 passed; 0 failed; 0 ignored** |
| full test suite (`cargo test --offline --locked -j 2 -- --test-threads=8`) | **714 passed; 0 failed; 1 ignored** |
| `bash tools/layers_check.sh` | **PASS** (44 modules, 111 edges, 0 named errors) |
| `bash tools/debt_ratchet.sh` | **0 over limit** |
| `MUTATION_STABILITY=1 CARGO_BUILD_JOBS=2 bash tools/mutation_check.sh --offline` (4 filtered runs, 6 new entries) | **6 proved, 0 not a gate, 0 parked, 0 skipped** |
