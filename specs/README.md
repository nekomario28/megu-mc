# 現在状態

このページは、現在の作業状態と次の判断の入口です。ブランチ名・HEAD・コミット数のように短期間で変わる値は記録しません。測定値と再現条件の詳細は各証拠ページを正本とし、ここでは結論と限界だけを要約します。

## 文書の役割

| 質問 | 正本 |
|---|---|
| いま何ができ、何が未確認か | このページ |
| 目標・合格条件 | [PLAN.md](PLAN.md) |
| 次に進む順序と各段階の出口条件 | [ROADMAP.md](ROADMAP.md) |
| なぜその設計を選んだか | [choices](close-defects/choices.md) と各spec |
| 数値・fixture・再現条件 | 該当する [`evidence/`](../evidence/) ページ |
| 過去の個別監査項目 | [close-defects](close-defects/README.md)（現在の全体完了判定ではない） |

READMEや他の計画文書に状態表を複製しません。更新時はこのページと根拠となる証拠を一緒に直します。

## 状態

- **全体目標は未達。** M2（同じ入力で遊べる範囲のvanilla一致）とM3（同じ仕事での軽量性）は達成していません。完了条件は[PLAN](PLAN.md)を参照。
- **worldgenは進行中。** terrain-only比較と cave / cave-extra / canyon carver は限定した2×2固定具で0セル差を確認しています。これは全seed・全chunk・featureを含む一致を示しません。
- **1.21.1 自前データへの移行は WIP。** 気候 router の inline 関数を読むようにし、built-in biome tree を同じ固定 jar から再生成しました。surface・aquifer などは未完で、旧統合線の緑を移行ブランチに引き継ぎません。[Horizon の再利用判断と検証範囲](../evidence/foundation/horizon-reuse/README.md)を参照。
- **feature層は未確定。** 以前の差分値の一部は、生成stage・参照chunkの完成度・skip数が揃っていないため、現行feature残差として扱いません。保存world全体の差をfeature差と呼ばないこと。
- **kelpは実装済みだがvanilla parityは未検証。** 合成回帰テストはありますが、既存のvanilla fixtureでは両側ともkelpが0個で、挙動を通った証拠になりません。正の参照セルを持つ完全生成chunkが必要です。
- **構造物は未実装。** jigsaw、template pool、NBT経由の構造物配置は未対応です。
- **M3は未達。** 地形が一致しない条件で得た旧x0.63値は性能根拠に使いません。比較条件・最新の測定と限界は[生成器の費用](../evidence/gameplay/worldgen-cost/README.md)を参照してください。
- **コード検証（旧統合線の履歴）:** `tests/verify.sh` 通過時の記録は159 passed、8スレッド×3回、wire smoke、adapterチェック、mutation 53 proved / 0 skipped、layers 16 modules / 40 edges / 0 named errors、debt ratchet 0 over limit。これは現在のデータ移行ブランチの合格結果ではありません。現在の実行結果は上記の移行証拠を参照。
- **clippy削減の履歴:** 457 → 83 → **0**（`--all-targets`・release、当時テスト176本green）。現在の移行ブランチでも警告0は確認しましたが、全テストgreenではありません。
  前半は機械的に等価な変換（`&*`/`&mut *` の冗長、`x % n == 0` → `is_multiple_of`、不要な `as`、
  `sort_by` → `sort_by_key` など）。残る判断が要るクラスは次のように閉じました:
  - `unnecessary_unwrap` 14: `is_some()` を見て `unwrap()` は `if let`/`match` に。3つのスロット
    クリック経路は `cv.1 == 0` で先に分岐し、Optionを「使う一本道」でだけ consume する形に。
  - `type_complexity` 24: レジストリの形を名前で呼びました（`BlockPos`/`ChunkKey`/`Registry<K,V>`/
    `LockedMap<K,V>`）、記録的なタプルは用途名（`SignKey`/`SignLines`/`ChunkBlockEntity`/`SpawnJob`/
    `ThrowHit`/`SmeltRecipe`/`TradeRow`/`PosRot`/`Advancements`、テストの参照データはローカル別名）。
  - `too_many_arguments` 6 は `#[expect]`（`#[allow]` ではなく、不要になったら鳴る形）で6件すべて
    fulfilled。引数列は参照側のもの（`Mth.lerp3` の11引数、`WorldCarver.carveEllipsoid` の入力、
    幹の入力）で、まとめると参照に無い型を発明することになります。
  - 死んでいたものは消しました: `FC.bc`（共有biomeキャッシュに置換済み）、`default_state`。
    生成プローブ用のアクセサ5つは `#[cfg(test)]`（コンパイラが「本番呼び出し無し」を認めた形）、
    `Trample.distance` は `java-compat` フィーチャのadapterだけが読むのでその条件で `allow`。

## 保留・外部依存（値ではなく事実だけ）

- **hosted CIはジョブが起動しません。** GitHub の注記は「The job was not started because recent account payments have failed or your spending limit needs to be increased」。**コード失敗ではない**ので、緑の判定は当面ローカルの `tests/verify.sh` が持ちます。復旧後に同一SHAで再取得します。
- **比較の前提（固定具の基点）は再構成できます:** `tools/fixture_base.sh` が `CARVER_FIXTURE_BASE`（既定 `/tmp/carverfresh`）を組みます。jar は sha1 で固定（`59353fb4…`、既存の1.21.1導入ディレクトリを donor に使う）。基点が無いまま固定具を回すと比較は空振りします。
- **層ゲートの検査器は固定revision:** `.github/workflows/ci.yml` の `GXKIT_REV`。同一バージョン文字列の古いビルドは独立した2循環を1つしか報告しないため、`tools/layers_check.sh` が `MC_SPINE_DEPSGX` → ローカルの固定ビルド → PATH の順で解決します。
- **被ダメ無敵は vanilla の `invulnerableTime` に載せ替えました**（slice 25、2026-10-24）。環境ダメージが timer を立てる点までは実装、環境ダメージ自身の差分適用は簡略（同 slice に明記）。
- **開放中の項目:** LICENSE 未追加（#11）、独立レビュー（#7）。
- **draft PR #2/#4/#6 は決着しました（2026-10-24）。** `main` に同一patchが無かった62コミットを読み、
  現行線に無い4点だけを取り込みました: play frameの上限をvanillaの2MiBへ、非有限・世界外の移動値の拒否、
  tickの実測（作業時間・TPS・予算超過）、チャンクバッチのACKペーシング。残りは現行線に新しい形であるか、
  vanillaに無いpacket rate limitか、そのブランチ専用のCI修正なので捨てました（理由は取り込みコミットに記載）。draft 3本は閉じ、同名ブランチは削除しました。

## 次の判別作業

**正の kelp 参照を含む fixture はできました**（`tools/kelp_fixture.sh`。seed 0 の lukewarm ocean を spawn から離れた場所で forceload し、正セル数をゲートに要求）。初回測定は `kelp` 494/484（shared 398）、`kelp_plant` 2761/2689（shared 2292）で、差は **同じ植物の高さ・head の位置**に集中しています（[carvers §62](../evidence/gameplay/carvers/README.md)）。

**draw 列は一致しました**（chunk (81,0) の `kelp_warm`、先頭20ドローが port/参照で完全一致。port は `FSEED_ARM=81,0,91,9`、参照は `-Dfseedarm=91,9`。`gi` は両側とも全体通し番号で 91）。

**列ごとの帰属まで進みました**（[carvers §62.1](../evidence/gameplay/carvers/README.md)）: 参照484列のうち **390 は茎まで一致**、**31 は「vanilla が同 step の seagrass の書き込みを見て棄却した列」**、**53 は「両側の `getHeight(OCEAN_FLOOR)` が違う列」**。`KelpFeature.place` の逆コンパイルで**実装差1件**（位置を毎回上げる）も見つけて修正しました（§62.2、負の対照つきテストで経路の実行を証明）。

次は 53 列の y の差（`hm_oc` の読み vs vanilla の `getHeight`）を同じ列で両側ダンプして比べることです。装飾順を固定具に寄せる実験は**負の結果**でした（§62.3）。

**型カバレッジ**は計測できるようにしました（`tools/feature_type_coverage.py`）: pack の 60 型中 26 型に `p_cf` の腕が無く、**overworld の biome から到達されるのは 6 型**（`fossil` `iceberg` `sculk_patch` `dripstone_cluster` `large_dripstone` `speleothem_cluster`）。
- **実装した型**: `geode`（残差はノイズの場）、`sea_pickle`、`ice_spike`、`root_system`（規則と入口まで）、`desert_well`（形と2ドローまで。loot table は port の chest と同じく未実装＝§69 に明記）、`iceberg`（draw 順・範囲・海面跨ぎまで。セル単位の一致は未測定＝§70 に明記）、`dripstone_cluster`（Column の両端・tip の向きと層順・プールの降下まで。セル単位の一致は未測定＝§71 に明記）、`large_dripstone`（2つの spike の根と taper・風の変位式まで。死んだ `placeColumn` は記録のみ＝§72 に明記）。未実装の到達型は 11 → **3**（ただし `speleothem_cluster` は 1.21.1 の feature ではなく、同梱 pack が新方言へ変換された際に入ってきたもの。§73 で計測: 真の未実装は `fossil` `sculk_patch` の2つ、かつ同梱データ自体が参照ビルドのものではない）。
- `geode`（**56 biome** から到達、最大の欠落）は**実装しました**（2026-10-24、`Cf::Geode`）。1.21.1 の `GeodeFeature`（`eck`）を逆コンパイルして層の閾値・点ごとのノイズ項・クラック・結晶の向きまで移植し、`tools/geode_fixture.sh`（seed 12 / chunk (0,0) に geode が出ることを port 側の trace で実測して選んだ固定具）で vanilla とセル単位に比較しています。**セル数はほぼ一致**（amethyst 162 vs 157、calcite 202 vs 201、smooth_basalt 237 vs 256）ですが**形はまだ一致しません**（3層合計 IoU 0.31、最良の平行移動でも 0.39）。残差はノイズの場に帰属させ、数値・固定具・次に測ることを evidence に残しました （[feature-parity](../evidence/gameplay/feature-parity/README.md) §63）。
  両側0個、全chunk skip、別stageの差分は成功扱いにしません。

以後の順序は[ロードマップ](ROADMAP.md)に従います。比較規律は[DESIGN.mdの約束](../DESIGN.md#比較の約束)を参照してください。
