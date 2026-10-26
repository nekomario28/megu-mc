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
- **worldgenは進行中。** 過去のterrain-only比較と cave / cave-extra / canyon carver は限定した固定具で一致しましたが、自前データ移行後の受け入れは未完です。旧固定具の緑を全seed・全chunk・featureを含む現在の一致へ広げません。
- **1.21.1 自前データへの移行は WIP。** 気候 router の inline 関数を読むようにし、built-in biome tree を同じ固定 jar から再生成しました。inline surface rule・ブロック状態・温度条件を接続し、[samplerとnative noise](../evidence/foundation/weird-sampler/README.md)、[aquiferの限定した実クラス比較](../evidence/foundation/aquifer/README.md)、[biome carverのAIR list](../evidence/foundation/biome-carvers/README.md)、[biome spawn tableと夜間hostileの接続](../evidence/foundation/biome-spawners/README.md)も追加しました。surface 全評価・aquiferの全seed/chunk・spawn_costsの適用・NaturalSpawner全体は未完です。[surface の部分検証](../evidence/foundation/surface-loader/README.md)を参照。旧統合線の緑を移行ブランチに引き継ぎません。[Horizon の再利用判断と検証範囲](../evidence/foundation/horizon-reuse/README.md)を参照。
- **Horizon の再利用を gameplay にも拡大。** 通常スロットとチェスト転送から、1.21.1 実クラス参照と本番 wire 検査を対にして進めています。[移植順序](horizon-gameplay/README.md)、[チェストの検証範囲](../evidence/gameplay/containers/README.md)、[アイテム回収の部分実装](../evidence/gameplay/inventory-pickup/README.md)を参照。エンティティ全般やチェストの開閉・同期まで移植済みという意味ではありません。
- **Valence は部品単位で評価。** 旧版 framework や registry は導入せず、entity角度・section palette・NBT文字列書き出し・有限AABB判定を既存 owner で修復しています。[再利用判断](../evidence/foundation/valence-reuse/README.md)、[角度](../evidence/gameplay/entity-angles/README.md)、[binary/geometryの検証範囲](../evidence/foundation/valence-primitives/README.md)、[分割した契約](valence-primitives/README.md)を参照。entity全体・任意NBT・worldのparityではありません。
- **feature層は未確定。** 以前の差分値の一部は、生成stage・参照chunkの完成度・skip数が揃っていないため、現行feature残差として扱いません。保存world全体の差をfeature差と呼ばないこと。
- **kelpは実装済みだが移行後のvanilla parityは未確定。** 正の参照セルを持つ固定具とdraw列・列単位の比較は[過去の記録](../evidence/gameplay/carvers/README.md)にあります。古い一時worldを前提にせず、自前データの意味論を修正した後、同じseed・座標・stageで参照を再構成して比較します。
- **構造物は未実装。** jigsaw、template pool、NBT経由の構造物配置は未対応です。
- **M3は未達。** 地形が一致しない条件で得た旧x0.63値は性能根拠に使いません。比較条件・最新の測定と限界は[生成器の費用](../evidence/gameplay/worldgen-cost/README.md)を参照してください。
- **ローカルsuiteは緑ですが、全体合格ではありません。** 直近は[biome spawner slice](../evidence/foundation/biome-spawners/README.md)の **236 passed / 0 failed / 1 ignored**。以前のredはcarver list欠落とown-jar feature pathに帰属して解消し、並行PR #21/#24のworldgen修正・trace遅延化も取り込みました。whole-world・native対照・hosted CIは未実行で、部品の実クラス一致やsuite緑を全体合格の代用にしません。
- **コード検証（旧統合線の履歴）:** `tests/verify.sh` 通過時の記録は159 passed、8スレッド×3回、wire smoke、adapterチェック、mutation 53 proved / 0 skipped、layers 16 modules / 40 edges / 0 named errors、debt ratchet 0 over limit。これは現在のデータ移行ブランチの合格結果ではありません。現在の実行結果は上記の移行証拠を参照。
- **clippy削減の履歴:** 457 → 83 → **0**（`--all-targets`・release、当時テスト176本green）。現在の移行ブランチでも警告0を確認し、直近のローカルsuiteは236 passed / 0 failed / 1 ignored、層ゲートは20 modules / 47 edges / 0 named errorsです。
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

- **hosted CIの過去の未開始はコード失敗とは別です。** 当時の注記は「The job was not started because recent account payments have failed or your spending limit needs to be increased」。現在の作業ブランチへのpush自体はworkflowのpush対象ではなく、今回hosted CIは実行していません。必須ローカルゲートにも未実行の範囲があり、CI・ローカルとも未実行を緑にしません。
- **比較の前提（固定具の基点）は再構成できます:** `tools/fixture_base.sh` が `CARVER_FIXTURE_BASE`（既定 `/tmp/carverfresh`）を組みます。jar は sha1 で固定（`59353fb4…`、既存の1.21.1導入ディレクトリを donor に使う）。基点が無いまま固定具を回すと比較は空振りします。
- **層ゲートの検査器は固定revision:** `.github/workflows/ci.yml` の `GXKIT_REV`。同一バージョン文字列の古いビルドは独立した2循環を1つしか報告しないため、`tools/layers_check.sh` が `MC_SPINE_DEPSGX` → ローカルの固定ビルド → PATH の順で解決します。
- **被ダメ無敵は vanilla の `invulnerableTime` に載せ替えました**（slice 25、2026-10-24）。環境ダメージが timer を立てる点までは実装、環境ダメージ自身の差分適用は簡略（同 slice に明記）。
- **開放中の項目:** LICENSE 未追加（#11）、独立レビュー（#7）。
- **draft PR #2/#4/#6 は決着しました（2026-10-24）。** `main` に同一patchが無かった62コミットを読み、
  現行線に無い4点だけを取り込みました: play frameの上限をvanillaの2MiBへ、非有限・世界外の移動値の拒否、
  tickの実測（作業時間・TPS・予算超過）、チャンクバッチのACKペーシング。残りは現行線に新しい形であるか、
  vanillaに無いpacket rate limitか、そのブランチ専用のCI修正なので捨てました（理由は取り込みコミットに記載）。draft 3本は閉じ、同名ブランチは削除しました。

## 次の判別作業

次に実行する最小sliceと順序・出口条件は[ロードマップ](ROADMAP.md)だけに置きます。過去の途中診断を、現在の「次の作業」として複製しません。

kelpの列・draw列・heightmapの診断履歴は[carvers](../evidence/gameplay/carvers/README.md)、個別featureの実装と残差は[feature-parity](../evidence/gameplay/feature-parity/README.md)、同梱packと固定jarの方言・出自の差は[自前データ監査](../evidence/foundation/audit-snapshots/README.md)を参照してください。いずれも自前データ移行後の全体受け入れを代替しません。比較規律は[DESIGN.mdの約束](../DESIGN.md#比較の約束)を正本とします。
