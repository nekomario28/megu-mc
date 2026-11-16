# Readiness review: `89ac2124` (test profile) + `fc9dfdf` (mushroom/basalt attribution)

Scope: 独立した integration/readiness review。canonical統合の承認ではなく、promotionでも公開でもない。
対象は READY とされた2 commitだけで、Jungle残差と state-facts / heightmap-light / collection-order の
3 trackには触れていない。

## 1. 入力の同定

| | 値 |
|---|---|
| canonical repo | `/home/yuu/mc-spine`、clean `perf/worldgen-parallel` = `e7ead8e66466b0b6467450cd3ea31d86a0e39cd3`（review時点で未変更） |
| base | `e7ead8e66466b0b6467450cd3ea31d86a0e39cd3` |
| review worktree | `/home/yuu/.t3/worktrees/mc-spine/t3-1f0488a5-da686e44cfc6095d`、branch `t3/mc-spine-readiness-review`、開始時 clean |
| 入力1 | `89ac21244b022f3d6d281c83478187a344c911d2`（parent = `e7ead8e`、`t3/401f5f2c`） |
| 入力2 | `fc9dfdf8f2ebf7af25924a6f759d08ed04b966d4`（parent = `e7ead8e`、`t3/27d57348`） |
| candidate（cherry-pick後） | `00ae47c0b32c2815d65de290d98366caa8789a05` → `1d98301a24484066467b4f92ef1b7f6511d9d98d` |
| 本 review の owned commit | この receipt と §8 追加、evidence の qualifier（下記 §6） |

両入力は base 直下の兄弟で、`Cargo.toml`/`evidence` と `src/*`/`tools` を分けるため
`git cherry-pick --no-gpg-sign` は2回とも無衝突。canonical側は一切編集していない。

## 2. 実際に検証したこと（`89ac2124`）

> **2026-11-16 追記（この節の一部は未拘束。遡って合格にしない）。** 当時の `list.txt` は
> start/finish も source SHA も持たず、同じ runner の `main.out` は `list_exit=127`、
> `bin=` / `sha256=`（空）、`tests_total=0` を残している。つまり binary 解決と `--list` の
> bookkeeping は失敗しており、下の「533 tests」「74,670,184 B / sha256 ac4528b1…」は
> **同じ invocation の成功として拘束できない**。数値自体を消さず、無効試行として残す。
> source-bound な再測定は
> [readiness-30460b7-20261116.md](readiness-30460b7-20261116.md) §3（533 tests、binary
> sha256 `23af568b…`、HEAD `6a2e88f`）であり、この節の数値を置き換えるものではない。

- **profile が何を渡しているか（実測、rustc命令行から）**
  `cargo test --offline --locked -j1 --bin mc-spine --no-run -v` の test unit:
  `opt-level=1` / `debuginfo=line-tables-only` / `debug-assertions=on`。
  `-C overflow-checks` は cargo が渡しておらず rustc の既定（= `debug-assertions`）に従うため on。
  cargo 自身の表示も `Finished \`test\` profile [optimized + debuginfo]`。
  log: `/home/yuu/.cache/mc-spine-readiness/logs/build_v.log`（36行目）。
- **test discovery が減っていないこと**: 同 binary `--list` で **533** tests。
  base は著者 receipt の `530 passed / 1 ignored` = 531、`fc9dfdf` が2本追加 → 533 と一致。
  （base 側 531 は本 review では再計測していない = 著者報告。）
- **binary サイズ**: `74,670,184 B`（`sha256 ac4528b1e7ada831…a662eb64967eb309`）。
  著者記載 74,644,504 B と同じ桁で、`line-tables-only` の効果自体は追認できる。
  ただし **速度（623.72 → 92.14 s）は再計測していない**ので、本 review の測定値として扱わない。
- **速度以外の設定が変わっていないこと**: `[profile.test]`/`[profile.dev]` に
  `overflow-checks`/`debug-assertions` の明示は無く、`[profile.release]`、`tests/verify.sh`、
  gate script、policy.json は diff に現れない（`git show --name-only` は `Cargo.toml` + evidence 2件のみ）。
- **限界の自己申告**: `architecture-survey-20261116.run.json` は `base_commit` を `e7ead8e` と
  明記し、`acceptance_limits` に「Horizon harness 未実行（ソース読解）」「`tests/verify.sh` 未実行」
  「6.7x はこの host の suite 1本」を列挙している。LGPL移行は §1 で明示され、
  `git show --name-only` に upstream のコードは1ファイルも入っていない（文書のみ）。

## 3. 実際に検証したこと（`fc9dfdf`）

- **production code の差分は comment-only**: `src/generation_providers.rs` の
  `+`/`-` 行から `//` 行と hunk header を除くと **0行**。したがって
  mushroom の `canSurvive` は挙動不変で、「before/after 32 -> 32」と整合する。
  (`src/feature.rs` 側は `cel_empty` 抽出 + `CEL_TRACE` 計装で、`trace` が `Some` のときだけ
  `cel_layer_ys` を呼ぶ。env 未設定時は `OnceLock` 参照のみ。)
- **負の対照（mutant が実際に落ちること）**: focused mutation 6件すべて `ok`
  （**2026-11-16 追記: この6件は保存された mutant log を持たないため UNVERIFIED。**
  当時の log は `build.build_v/list/main.out` のみで、main.out は緑の focused test であって
  mutant の失敗 assertion ではない。件数だけでは対照にならない。新規の6件は
  `fresh-30460b7-20261116/mutants.tsv` と各 log に assertion 付きで残っており、
  この旧6件の追認ではなく別 run である）
  （`MUTATION_FILTER` で対象testに限定、`CARGO_OFFLINE=1`）:
  - `nether_mushrooms_survive_a_solid_render_support`: `isSolidRender` → `true`、
    `uninitialized_raw_brightness` → 常に `15` の2件。
  - `cel_layer_trace_lists_exactly_the_layers_cel_find_y_walks`: `bedrock` 除外を外す1件。
  - `count_on_every_layer_walks_motion_blocking_layers`: fluid/bedrock/re-sample の3件
    （`cel_empty` 抽出に合わせて anchor が更新されており、更新後の anchor が実際に bite する）。
- **新 test 本体**: `nether_mushrooms_survive_a_solid_render_support` は Nether preset で
  `has_skylight()==false` を assert した上で netherrack/basalt/soul_soil/blackstone を真、
  water/lava を偽とする。`cel_layer_trace_lists_exactly_the_layers_cel_find_y_walks` は
  bedrock+stone 3枚の柱で `[15,12,9]` を要求し、`cel_find_y` の各 layer と1対1で突き合わせる。
  どちらも実装の写経ではなく、片側を壊すと落ちる（上の mutant で確認）。
- **著者の「5 layer / 4 layer」の port 側**: 残存する `/tmp/t6b/cel22.log` は
  pass `(-25,22)` で layer `0..4` を要求している（149 columns、layer 3 止まり 38）。
  つまり *portが5層を要求している* ことは既存logで追認できる。
  **reference 側（16 draws / 4 layers）は追認できていない**（下記 §5）。

## 4. 実行した compute

| | |
|---|---|
| gate | `/home/yuu/Documents/project-incubator/scripts/resource_gate.py --class light --need-mem-gb 2` → `GO class=light avail=16.1GB swap_used=23.1%` |
| scope | `systemd-run --user --scope -p MemoryMax=3G -p CPUQuota=100% -p Slice=app.slice`（`devin-compute.slice` 外、named scope 2本） |
| 実行 | `cargo test --offline --locked -j1 --bin mc-spine`, `--test-threads=1`, `CARGO_TARGET_DIR=<worktree>/target-readiness` |
| job1 | 09:53:00 → 09:54:33（cold build を含む。`/usr/bin/time` 不在で timing/log 取りが1度失敗、同じ scope 内で再実行） |
| job2 | 09:54:52 → 09:58:26（`-v` 取り直し + focused mutation 6件） |
| OOM | なし（3G 宣言で足りた） |
| 未実行 | 全 suite 1本（≈92 s と著者報告）、`tests/verify.sh`、native server 起動、全 410 mutation、golden 更新 |

## 5. 反論・未解決

1. **LIGHT attribution は仮説**。`getRawBrightness` の返り値は測られていない。
   fire 距離 2 の4セルは「lit だった」ことの *事後* 相関で、`15 - d >= 13` の定量一致は
   4点への当てはめ。因果鎖の3つの環（reference が実際に返した値、同一 pass の `patch_fire`
   書込みが FEATURES 中に light engine を更新するのか、read 時点の support state）は未計測。
   → **rewrite は HOLD**。本 commit は rewrite を含まないので、commit 自体は止めない。
2. **basalt の first divergence は未拘束**。delta の分解式が合計に一致しない
   （`156+2*156+2*73` = 614 ≠ 780、`82+2*82+2*77` = 400 ≠ 800）。
   `four of 200` も残存 `CEL_TRACE` log（38 columns stop at layer 3）からは出てこない。
   780/800 は「156 vs 160 positions × 5 draws」とは整合するが、それは著者の記述とは別の主張。
3. **`fc9dfdf` には machine receipt が無い**（`89ac2124` には `.run.json` がある）。
   参照している `/tmp/rt-4242/*`・`/tmp/t6b/*` は source/build SHA を持たず、
   `cel22.log` は commit 時刻（08:50）より後の 09:31 で、別 lane の再実行である可能性を排除できない。
   新契約（`specs/worldgen-parity/investigation-workflow.md` §8）では「未拘束」と書くべきもの。
4. `cel_layer_ys` は `cel_find_y` と同じ境界判定を **複製** している（drift 源）。
   片方向の pin（1本の test + 1 mutant）だけなので、境界規則を触る次の lane は両方を直すこと。
5. `89ac2124` の速度数値は本 review の測定値ではない。profile の内容・test数・binary サイズのみ追認。

## 6. 本 review が加えた変更（自己承認しない）

- `specs/worldgen-parity/investigation-workflow.md`: §8「報告と検証の契約（2026-11-16 承認）」を追加。
  並列の policy 文書は作っていない。
- `evidence/gameplay/feature-parity/mushroom-brightness-and-basalt-columns-20261116.md`:
  観測値を消さずに 3 箇所だけ qualifier を追加（review status ブロック、§1 の light 因果を
  仮説と明示、§2 の分解式の算術不一致）。**数値は書き換えていない。**
- この receipt。

これらは実装の修正ではないが、reviewer 自身の edit なので独立 review の対象から外さないこと。

## 7. 判定

**READY FOR PARENT REVIEW**（範囲限定、2026-11-16 時点）

> 追記: 本 receipt の §2/§3 には未拘束・UNVERIFIED の項目がある（上記）。source-bound な再測定は
> [readiness-30460b7-20261116.md](readiness-30460b7-20261116.md)。その producer 自体は著者の変更なので、
> 新しい独立レビューが要る。「READY」は本 receipt の記述範囲に対してであり、producer の採否ではない。

- 受け入れる: `89ac2124` の test profile 変更（速度は著者報告）、`fc9dfdf` の
  `CEL_TRACE` 計装・`cel_empty` 抽出・Nether contract test・`canSurvive` 読みの訂正・
  basalt「column-placement」attribution の撤回。
- 保留: mushroom の lit-read を前提とした `canSurvive`/light model の書き換え、
  basalt 残差の原因断定、draw 数の引用。
- 依存として残る: Jungle 残差、state-facts / heightmap-light / collection-order の3 track
  （本 review では読んでいない・待っていない）。
