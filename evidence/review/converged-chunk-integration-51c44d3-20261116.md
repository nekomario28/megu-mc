# 統合検証 — chunk の carve stage を単一所有者にする（2026-11-16）

Date: 2026-11-16. 候補 `51c44d3`（base `82799b4` + `e9214ac` の cherry-pick）。
作業は tool-created worktree（`t3-3d969a40-…`）と private audit dir
`<local>/audit/converged-chunk-integration-20261116T103726Z/` のみ。
push・tag・PR・network・download は無し。監査 dir は 93 MiB、raw log は全 phase 分を保持している。

本書は**この候補について実測した記録**である。恒久的な契約は `tests/verify.sh` と
`tools/mutation_check.sh`、設計の rationale は `specs/chunk-state-lifecycle/README.md` にある。

## 何が入ったか

| 項目 | 実測値 |
|---|---|
| base（既存 worldgen + harness） | `82799b4bf7ce8cc84120be82a6a5c129c8f1c087` |
| 取り込んだ commit | `e9214ac65ef3afc75cb4b2359dcccb9984f5e7ec`（parent は root の旧 HEAD `e7ead8e`） |
| 候補 | `51c44d3512400f01a64964385a01c617438ee646` |
| 変更 | 3 files, +469/−42: `src/worldgen.rs`, `tools/mutation_check.sh`, `specs/chunk-state-lifecycle/README.md` |
| `src/worldgen.rs` | `45714fb62951f9f716f73e24b98ad0556bb08cb44d0ec7ea2273eab134d903df` — e9214ac のものと同一 |
| mutation entries | 424 → **427**（旧 `begin_carve` entry を置換 + 3 追加） |

cherry-pick は `tools/mutation_check.sh` を auto-merge し、コンフリクトは無かった。
old entry の anchor は base 側の行移動に追従しており、e9214ac の diff（+31/−14）は
新 latch / 4096 eviction / 384 bucket の 4 entry として入っている。

## 意味の閉包

`Gen::carve_c` が carve stage の唯一の所有者で、2 つの出力は別の寿命を持つ。

* `wg`（post-carve の WG heightmap）— `CARVE_WG_BOUND = 4096` chunk まで保持。**384 の
  feature bound に切り詰められない**。再導出はできるが、落ちた block bucket からは読めない。
* `blocks_live` — その pass の block writes がまだ `feat_c` にあるか。`forget_feature_bucket`
  （384 の唯一の eviction 点）で落ち、`begin_carve` は pass を再実行する。

**厳密な claim は `blocks_live`**であり「record の存在 = latch」ではない。record は
`wg = Some` / `blocks_live = false` の状態で存在しうる（`set_wg_after_carve` を claim 無しで
呼ぶ fixture がまさにその状態を作る）。したがって record の有無ではなく
`begin_carve` の戻り値が claim の owner である。`evict_carve_record` は値と block output を
同時に落とすので、4096 を超えた chunk は「未 carve」に戻り、次の生成が両方を再導出する。

5 つの新テスト（`worldgen.wg_tests`）:

1. `carve_output_and_its_latch_share_one_lifetime` — 実 FEATURES entry point で (6,7) を回し、
   384 eviction 後に同一入力で再実行して override digest と WG 値が一致することを要求する。
2. `post_carve_wg_outlives_the_feature_cache_but_not_its_claim` — 384 が block output を落としても
   WG 値は残り、claim は落ちる。
3. `the_wg_retention_bound_drops_the_value_and_the_claim_together` — 4100 chunk で 4096 bound が
   実際に evict し、値・claim・block output が同時に落ち、同じ入力から再確立できる。
4. `repeated_stage_updates_keep_one_slot_per_chunk` — claim / 記録 / evict / 再挿入を 600 round
   繰り返しても `feat_ord` が重複せず `feat_c` と一致し、bucket を失った chunk は claim を失う。
5. `evicting_block_output_keeps_the_staged_payload` — block output の eviction は runtime payload
   を落とさない。

## 非 mutation の combined stage（実測）

実行したのは**コミット済み `tests/verify.sh` の非 mutation 部分**そのもの。終端の
`MUTATION_STABILITY=1 bash tools/mutation_check.sh` 行以降（12 行）を program 上で切り落とし、
コピーが tree の外にあるため cwd 所有者 1 行だけを差し替えた（`verify-prefix.committed.sh` との
diff は 2 行 = 1 行削除 + 1 行追加、他は byte 同一）。`set -euo pipefail`、pipe 無しの生 exit。

| phase | 実測 |
|---|---|
| gate exit contracts | 31 PASS（`gate_scripts.py` の自己テスト含む） |
| module layering | 42 modules / 108 edges / 0 named errors, `layers check: PASS` |
| debt ratchet | `0 over limit`（wall-clock reads 0, parked gates 1） |
| tests, serial | **549 passed / 0 failed / 1 ignored** (94.92 s) |
| tests, eight threads × 3 | 各 **549 passed / 0 failed / 1 ignored** (51.28 / 50.68 / 51.23 s) |
| wire smoke | `wire_smoke` + client settings / gameplay / containers / population plan / mineshaft dispatch すべて PASS |
| adapter checks | `--features fabric-probe` と `--features neoforge-events` の 2 つとも成功 |

合計 550 tests = 549 + 1 ignored（`tick_tests::profile_game_tick`、release-profile 専用）。
serial log に現れる `test result: FAILED` 1 件は panic 隔離の**子プロセス**
（`registry_tests::a_panicking_scene_fixture_leaves_nothing_to_restore`）で、外側の test は ok。

unit: `mcspine-conv-combined-1052.service`（`devin-compute.slice`, MemoryMax=8G, CPUQuota=550%,
RuntimeMaxSec=2400）で `Result=success`, exit 0, wall 376 s, CPU 8 m 33.7 s, peak 820.1 MiB。
sentinel `receipts/combined-sentinel.txt` は exit 0 のときだけ書かれ、`verify: PASS` は
切り落とした側にしか存在しない。

1 回目（`mcspine-conv-combined-1037`）は **stage 自体は exit 0（442 s）**だったが、この driver が
stage 後に `set -u` 下で未定義の `$n` を参照して exit 1 になり sentinel を書けなかった。
その log は `logs/combined-attempt1/` にそのまま残し、wrapper の欠陥だけ直して 2 回目を実行した
（stage の再実行はこの wrapper 欠陥が理由で、結果を見て回し直したのではない）。

## strict mutation（実測）

選択は main agent の決定（変更スコープの 16 entry + e921 の repair 4 entry）。20 名を
**名前一致**で選び、`MUTATION_STABILITY=1` で 1 campaign 実行した。

* **20 proved / 0 not a gate / 0 parked / 0 skipped**（`mutation check: 20 proved, …`）。
* 実行された名前集合は選択集合と完全一致（`receipts/name-set.diff` は空）。
* 各 entry は mutant red → 同一 filter の re-run も red → restore 後 green、を満たす
  （`receipts/strict-per-entry.tsv`、76 log = mutant 20 + re-run 20 + restored 20 + baseline 16）。
* compile error / zero-match / ignored は 0 件（anchor はすべて一意に一致した）。
* unit: `mcspine-conv-mutants-1100.service`（同 slice / 同 cap）で `Result=success`, exit 0,
  wall 409 s, CPU 20 m 33.0 s, peak 2.4 GiB。`CARGO_BUILD_JOBS=4`、test は `--test-threads=1`。
* campaign は private snapshot
  `~/.cache/mc-spine/mutation-runs/run.2l9L3bJG`（3423 files, identity
  `5f01fbf88f39b3daff3a72b80f8da9b0df120bf8fb0cee4d7bc2fd9fe3aa833c`）で走り、
  restore は entry ごとに sha256 で検証されている（`BROKEN` 0）。

20 entry（名前 → test filter）:

| # | entry | filter |
|---|---|---|
| 1 | the light packet assembles spill into its floors | `registry_tests::sky_light_assembly_lights_through_an_opening_and_stops_at_a_barrier` |
| 2 | the pinned facts separate the predicates vanilla separates | `state_facts_answer_the_predicates_vanilla_separates` |
| 3 | an empty collision shape is not a full one | 同上 |
| 4 | a fluid state is not a liquid block | 同上 |
| 5 | the light columns are emission then block, not one shifted | 同上 |
| 6 | the face planes are indexed by support type, not by direction | `face_support_answers_the_support_type_the_caller_asks_for` |
| 7 | a fact record is five bytes, one per state | `pinned_fact_tables_agree_with_the_older_oracle_assets` |
| 8 | mushroom canSurvive asks isSolidRender, not the dirt tag | `…::nether_mushrooms_survive_a_solid_render_support` |
| 9 | an uninitialized sky light reads 15, a dimension without one reads block light | 同上 |
| 10 | a traced column names the layer that answered it | `…::cel_column_trace_tags_the_layer_that_answered` |
| 11 | the count_on_every_layer column trace lists the layers the walk places on | `…::cel_layer_trace_lists_exactly_the_layers_cel_find_y_walks` |
| 12 | a disk walks its radius box in betweenClosed order (z slowest) | `…::disk_walks_the_radius_box_in_between_closed_order` |
| 13 | attached_to_leaves draws its probability before the required-empty check | `…::attached_to_leaves_draws_its_probability_before_the_required_empty_check` |
| 14 | blue ice runs exactly two hundred attempts | `…::blue_ice_spends_the_native_two_ints_per_attempt_stream` |
| 15 | the horizontal plane is walked north, east, south, west | `…::horizontal_plane_order_is_north_east_south_west` |
| 16 | cocoa binds its draws to the plane order, not the reversed array | `…::cocoa_binds_its_four_probability_draws_to_north_east_south_west` |
| 17 | a chunk claims its carver pass once | `…::a_chunk_claims_its_carver_pass_once` |
| 18 | a carve claim does not outlive the block output it stands for | `worldgen::wg_tests::repeated_stage_updates_keep_one_slot_per_chunk` |
| 19 | the feature bucket bound does not truncate the WG retention to 384 | `worldgen::wg_tests::post_carve_wg_outlives_the_feature_cache_but_not_its_claim` |
| 20 | an evicted post-carve WG value takes its claim with it | `worldgen::wg_tests::the_wg_retention_bound_drops_the_value_and_the_claim_together` |

残り **407 entry は実行していない**（skip でも PASS でもない）。選択した20件は変更経路への
直接の確認であり、非選択407件が変更の影響を受けないことまで証明したわけではない。
549件の通常テストの成功も、非選択mutationの検出力の再証明を代替しない。これは承認された
選択実行方針の限界として残す。

## 既知の限界

* **CI の pin ではない**。`.github/workflows/ci.yml` の `GXKIT_REV` は
  `d6b9ad8db4774919c8b4fb032342e53b2548b660`、本実行の `depsgx` はローカル editable
  `973b293a` の 1.0.0（`<home>/gxkit/.venv/bin` を PATH に置き `MC_SPINE_DEPSGX` は unset）。
  layers gate は inspector の probe（2 cycle / layer-violation / forbidden-dependency）で
  「fail できること」を確認してから 42/108 を判定している。
* **whole-world native CI は NOT RUN**（pin/native の world claim は無い）。
* **407 mutant は NOT RUN**（上記のとおりスコープ外）。
* mutation producer は選択のため **1 行だけ adapter** を入れた（`MUTATION_NAMES_FILE` による
  完全一致選択）。committed の `MUTATION_FILTER` 経路は無改変で、snapshot / manifest /
  stability / restore 検証はすべて commit 済みのコードである。adapter 込みの sha256 は
  `receipts/logs.sha256`、parity は `receipts/producer-parity.diff`（差は
  `tools/mutation_check.sh` 1 file のみ）。
* combined の 2 回目で adapter check 2 本が `Finished … in 0.01s` になったのは、1 回目が
  同じ `target/` に同じ 2 feature を build 済みだったため（cargo cache。check 自体は成功）。
* **測定時点のe9214ac由来の文言のずれ2件**（main agentが後続のコメント・文書修正で訂正。測定時の内容は以下）:
  (a) commit message / `specs/chunk-state-lifecycle/README.md` の「5 new tests red」は実際には
  **4 red + 1 既緑**（`evicting_block_output_keeps_the_staged_payload` は変わらない挙動の guard。
  これは main agent が e921 の raw log から読んだ結果で、本実行は pre-change の赤集合を
  再測定していない）。(b)「record の存在が latch」というコメント/README の言い方は
  `blocks_live` についてのみ正しい（record は `wg=Some`/`blocks_live=false` で存在しうる）。

## raw artifacts

`<local>/audit/converged-chunk-integration-20261116T103726Z/`

| 内容 | path |
|---|---|
| 文脈（base / SHA / root の FF 前提 / toolchain） | `00-context.txt` |
| 非 mutation の全 raw（2513 行）と phase 別 log | `logs/combined/raw.log`, `logs/combined/0*.log` |
| 1 回目の stage（wrapper 欠陥） | `logs/combined-attempt1/` |
| 20 entry の raw（76 file） | `logs/mutants/mutants-selected/…`（`logs/mutants/`） |
| 選択と実行の名前集合、entry 別の赤/緑 | `receipts/executed-names.txt`, `receipts/strict-per-entry.tsv` |
| driver と prefix の適応 diff | `run-combined.sh`, `run-mutants.sh`, `receipts/prefix-adaptation.diff` |
| 主要 log の sha256 | `receipts/logs.sha256` |

`logs/combined/raw.log` = `b71e6abd294cd68ca00751cff28fed695ae49bd9532f0ec3ee7ad1239fb22c05`,
`logs/mutants-selected/raw.log` = `64d94e631e7c6c529d753cfd92966e99881c41cdeadc36798d60b3ff155651f7`。
