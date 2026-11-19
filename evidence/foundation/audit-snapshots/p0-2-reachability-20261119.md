# P0-2 Reachability Audit — 2026-11-19（`Cond::Biome` u64→u128 同期版）

`tools/p0_2_audit.py` / `tests/p0_2_audit_test.py` を `d918eac`（`Cond::Biome` が
`u64` → `u128` にアップグレード済み、src/worldgen.rs:707）に同期した後の、固定
1.21.1 データパック（`datapacks/vanilla/`、protocol 767）と Rust パーサたどり着ける
領域の監査結果。

## 記録

| 項目 | 値 |
|---|---|
| audit 対象 HEAD | `d918eacaee795387031ef6446e68c685e59b9c11`（d918eac） |
| ターゲット データパック | `datapacks/vanilla/`（1.21.1、protocol 767） |
| 固定 jar SHA-1 | `59353fb40c36d304f2035d51e7d6e6baa98dc05c` |
| 監査実行 | `python3 tools/p0_2_audit.py --json` — exit 0、`failures: []` |
| `Cond::Biome` アンカー | `src/worldgen.rs:707`、`enum Cond`（702）内 `Biome(u128)`；audit の `parse_cond_biome_anchor` が確認（`u128` 以外なら AuditError） |
| パーサ数 | df=43、surface_rule=5、surface_cond=12、placement_modifier=18、configured_feature=63 |

## 1. Density functions の到達可能性（`noise_settings/{overworld,nether,end}` の `noise_router`）

| 項目 | Overworld | Nether | End |
|---|---|---|---|
| `bleaf_count` / `< 128` `Cond::Biome(u128)` | 53 / 128 | 5 / 128 | 5 / 128 |
| 実装済み df 型（`df_types`、parser arm 全てあり） | 27 | 12 | 9 |
| 実装済み df 型一覧 | abs、add、blend_alpha、blend_density、blend_offset、cache_2d、cache_once、clamp、cube、flat_cache、half_negative、interpolated、max、min、mul、noise、old_blended_noise、quarter_negative、range_choice、shift_a、shift_b、shifted_noise、spline、square、squeeze、weird_scaled_sampler、y_clamped_gradient | add、blend_density、cache_2d、flat_cache、interpolated、mul、old_blended_noise、shift_a、shift_b、shifted_noise、squeeze、y_clamped_gradient | add、blend_density、cache_2d、end_islands、interpolated、mul、old_blended_noise、squeeze、y_clamped_gradient |
| 明示 const/zero-fold 型（`df_zero_folded_reachable`） | `blend_alpha -> DF::C(1.0)`、`blend_offset -> DF::Zero` | —（reachable でない） | —（reachable でない） |
| Unsupported 到達型（`df_unsupported`） | 0 | 0 | 0 |
| Missing density_function refs（`df_missing_refs`） | 0 | 0 | 0 |
| Missing noise refs（`noise_missing_refs`） | 0 | 0 | 0 |
| 到達 df refs（`df_refs_count`） | 19 | 3 | 2 |

実装済み df 型の個別カウント（`df_types`）:
- Overworld: abs=8, add=44, blend_alpha=4, blend_density=1, blend_offset=1, cache_2d=6, cache_once=8, clamp=5, cube=2, flat_cache=8, half_negative=1, interpolated=8, max=5, min=5, mul=31, noise=27, old_blended_noise=1, quarter_negative=2, range_choice=10, shift_a=1, shift_b=1, shifted_noise=5, spline=3, square=1, squeeze=1, weird_scaled_sampler=3, y_clamped_gradient=8
- Nether: add=4, blend_density=1, cache_2d=2, flat_cache=2, interpolated=1, mul=3, old_blended_noise=1, shift_a=1, shift_b=1, shifted_noise=2, squeeze=1, y_clamped_gradient=2
- End: add=10, blend_density=1, cache_2d=2, end_islands=3, interpolated=1, mul=5, old_blended_noise=1, squeeze=1, y_clamped_gradient=4

## 2. Surface rules & conditions の到達可能性（`noise_settings/<dim>.json` の `surface_rule`）

| 項目 | Overworld | Nether | End |
|---|---|---|---|
| surface_rule 型（`surface_rule_types`） | bandlands=2、block=100、condition=137、sequence=48 | block=22、condition=41、sequence=12 | block=1 |
| surface_rule Unsupported（`surface_rule_unsupported`） | 0 | 0 | 0 |
| surface_cond 型（`surface_cond_types`） | above_preliminary_surface=1、biome=39、hole=3、noise_threshold=33、not=4、steep=5、stone_depth=23、temperature=1、vertical_gradient=2、water=20、y_above=10 | biome=5、hole=3、noise_threshold=11、not=10、stone_depth=7、vertical_gradient=2、y_above=13 | （無し） |
| surface_cond Unsupported（`surface_cond_unsupported`） | 0 | 0 | 0 |
| `biome_is` 参照数（`surface_biome_is_count`） | 27 | 5 | 0 |
| bleaf に存在しない biome（`surface_biome_is_missing_from_bleaf`） | 0 | 0 | 0 |

## 3. Placement modifiers（`p_mod`）& Configured features（`p_cf`）の到達可能性

（biome → placed_feature（`features`）→ configured_feature をたどり、各 ディメンションで到達可能）

| 項目 | Overworld | Nether | End |
|---|---|---|---|
| p_mod 型（`placement_modifier_types`） | biome=151、block_predicate_filter=63、carving_mask=1、count=97、environment_scan=11、height_range=55、heightmap=92、in_square=149、noise_based_count=4、noise_threshold_count=4、random_offset=11、rarity_filter=44、surface_relative_threshold_filter=3、surface_water_depth_filter=24 | biome=37、block_predicate_filter=5、count=22、count_on_every_layer=8、height_range=27、heightmap=2、in_square=28、rarity_filter=4 | biome=5、count=2、fixed_placement=1、height_range=1、heightmap=2、in_square=3、random_offset=1、rarity_filter=2 |
| p_mod Unsupported（`placement_modifier_unsupported`） | 0 | 0 | 0 |
| p_cf 到達数（`configured_features_reachable`） | 143 | 31 | 5 |
| p_cf 未実装到達（`configured_feature_unimplemented_reachable`） | 0 | 0 | 0 |
| missing placed_feature（`missing_pf`） | 0 | 0 | 0 |
| missing configured_feature（`missing_cf`） | 0 | 0 | 0 |

## 4. Biome `carvers` / `spawners` / `spawn_costs`（`biomes_and_structures`）

| 項目 | 値 |
|---|---|
| `biomes_total` | 64 |
| `biomes_with_air_carvers`（`carvers` が非空の biome 数） | 58 |
| `biomes_with_spawners`（`spawners` が非空の biome 数） | 62 |
| `biomes_with_spawn_costs` | `minecraft:soul_sand_valley`=4、`minecraft:warped_forest`=1 |

スコープ誠実な記録:
- **carvers**: この監査は `carvers` が非空かどうかのみを数える（58）。carver の設定値・ターゲット解決はこの表に含めない。
- **spawners**: この監査は spawner リストが非空の biome 数を数える（62）。entry（entity タイプ、`weight`、`minCount`/`maxCount`）の解釈は実行系パーサ（`src/feature.rs`）に在り、この監査は到達可能性の確認のみ。
- **spawn_costs**: この監査は biome ごとのコスト entry 数を数える（soul_sand_valley=4、warped_forest=1）。`charge` / `energy_budget` の値は実行系で `src/feature.rs:5253`（コメント「1.21.1 `spawn_costs`: biome -> entity id -> (energy_budget, charge)」）、`pub energy_budget: f64`（5273）、`jnum(j, "energy_budget")?`（5330）で解析済みだが、この監査のテーブルには含めない。
- **`NaturalSpawner`**（`src/feature.rs:5621`、`src/main.rs:16507`）は監査範囲外（deliberately narrower と同等の意図）。

## 5. Structures / structure sets（`biomes_and_structures`）

| 項目 | 値 |
|---|---|
| `structures_total`（定義数） | 34 |
| `structure_sets_total` | 20 |
| structure 型（16 種） | buried_treasure=1, desert_pyramid=1, end_city=1, fortress=1, igloo=1, jigsaw=10, jungle_pyramid=1, mineshaft=2, nether_fossil=1, monument=1, ocean_ruin=2, ruined_portal=7, shipwreck=2, stronghold=1, swamp_hut=1, woodland_mansion=1 |

## 6. ツール同期の変更点（`tools/p0_2_audit.py` / `tests/p0_2_audit_test.py`）

- `tools/p0_2_audit.py`:
  - `audit_density_and_surface` の bound を `bleaf_count >= 64` → `>= 128`、failure メッセージを `overflows Cond::Biome(u64)` → `overflows Cond::Biome(u128)` に変更。
  - `parse_cond_biome_anchor(wg_src)` を追加: `enum Cond` と `Biome(u128)` variant の存在を確認し、型が `u128` 以外なら `AuditError`（fail-closed）。`run_audit` がこれを呼び、report に `cond_biome_anchor: "u128"` を追加。
  - docstring（`< 64` `u64` mask guard → `< 128` `Cond::Biome(u128)`）、`format_report` の `bleaf=53/128` 表示を更新。
- `tests/p0_2_audit_test.py`:
  - `test_unsupported_density_surface_or_bleaf_overflow_fails_closed`: nether に 128 個の fake biome を注入（`range(128)`）し `overflows Cond::Biome(u128)` を assert。
  - `test_pinned_datapack_and_rust_parsers_pass_audit`: `cond_biome_anchor == "u128"`、`bleaf_count < 128`、オーバーワールド/ネザー/エンドごとの failures 空を assert。
  - `test_cond_biome_anchor_fails_closed_on_revert_or_mismatch`: 新規（revert `Biome(u64)`、`enum Cond` 不在、anchor 無しの3ケースで `AuditError`）。

## 7. 検証ゲートの結果

| Gate | 結果 |
|---|---|
| `python3 tools/p0_2_audit.py` | exit 0 — `p0_2_audit: PASS (0 unsupported reachable parser nodes, 0 missing refs)` |
| `python3 tests/p0_2_audit_test.py` | OK — 4 tests（pinned datapack pass、overflow fail-closed(u128)、anchor fail-closed、placement fail-closed） |
| `bash tools/layers_check.sh` | exit 0 — `layers check: PASS (current debt is named, not resolved)` |
| `bash tools/debt_ratchet.sh` | exit 0 — `debt ratchet: 0 over limit` |
| `python3 tools/provenance_check.py` | exit 0 — `provenance: clean` |
