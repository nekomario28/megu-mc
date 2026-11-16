# 現在状態

このページは、現在の作業状態と次の判断の入口です。ブランチ名・HEAD・コミット数のように短期間で変わる値は記録しません。測定値と再現条件の詳細は各証拠ページを正本とし、ここでは結論と限界だけを要約します。

## このページの読み方

| 文書の種類 | 置き場所 | 現在の扱い |
|---|---|---|
| 現在の作業・できていること・限界 | このページの「状態」 | 正本。数値・条件は各 `evidence/` を参照 |
| 受け入れ済みの契約 | 各specのREADMEと、そこが指す `evidence/` | 対象fixture・対象stageの範囲だけで有効 |
| 提案・別ブランチの設計 | 例: [chunk-parallelism](chunk-parallelism/README.md) の設計案 | 提案全体を実装済みとは扱わない。[carve stageの所有権・破棄の整合性](chunk-state-lifecycle/README.md)は取り込み済みだが、chunk lifecycle全体の改革は未完 |
| 歴史的記録 | [close-defects](close-defects/README.md)、[done/](done/)、旧checkpoint | 現在の合格判定に使わない |
| 次に進む順序 | [ROADMAP](ROADMAP.md) | 順序と出口条件の正本。このページに複製しない |

文書と実装が食い違う場合は、実装とテストを確認してこのページを直します。追跡データと方針文書の
食い違い（未解決）は [CONTRIBUTING](../CONTRIBUTING.md) と [開発ガイド](../docs/development.md) に記録しています。

別ブランチで完了した計測・profileの修正は、このツリーに取り込まれるまで**現行ベースの結果ではありません**。
取り込み済みかは `git log` と、各 `evidence/` が対象revisionをどう記録しているかで確認してください。

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

直近の実行順は [ROADMAP](ROADMAP.md)、限定実装と残る出口は
[実行台帳](implementation-checklist.md)、効率的な検証方法は
[因果差分の調査手順](worldgen-parity/investigation-workflow.md)を参照する。
過去のcheckpoint・履歴節を現在の全体合格や次の作業と混同しない。

- **全体目標は未達。** M2（同じ入力で遊べる範囲のvanilla一致）とM3（同じ仕事での軽量性）は達成していません。完了条件は[PLAN](PLAN.md)を参照。
- **worldgenは進行中。** 過去のterrain-only比較と cave / cave-extra / canyon carver は限定した固定具で一致しましたが、自前データ移行後の受け入れは未完です。旧固定具の緑を全seed・全chunk・featureを含む現在の一致へ広げません。**2026-11-12 追記：** seed 0 の chunk (0,0) を pinned 1.21.1 server（隔離実行・プレイヤー無し・features あり）から採取し、`tests/data/overworld_chunk_0_0_1211.tsv.gz` として固定しました。`overworld_chunk_matches_native_capture_state_for_state` が**正規化 state（名前＋全プロパティ）で 32,518 / 32,518 セル一致**を要求します。参照側は同一入力の2回実行で 4 セルばらつく（座標は provenance）ため、0 差は名前を特定したcaptureとの比較結果です。観測したrun間差を許容マスクや普遍的な床にはしません。範囲は **1 chunk・1 seed・Overworld・ストラクチャ/エンティティ対象外**で、全seedや他次元へは広げません。
- **1.21.1 自前データへの移行は WIP。** 気候 router の inline 関数を読むようにし、built-in biome tree を同じ固定 jar から再生成しました。inline surface rule・ブロック状態・温度条件を接続し、[samplerとnative noise](../evidence/foundation/weird-sampler/README.md)、[aquiferの限定した実クラス比較](../evidence/foundation/aquifer/README.md)、[biome carverのAIR list](../evidence/foundation/biome-carvers/README.md)、[biome spawn tableと夜間hostileの接続](../evidence/foundation/biome-spawners/README.md)も追加しました。surfaceのsteep・条件式・合成buildSurface scanは[限定比較済み](../evidence/foundation/surface-context/README.md)ですが、real-noise地形へのsurface全適用・aquiferの全seed/chunk・spawn_costsの適用・NaturalSpawner全体は未完です。[surface の部分検証](../evidence/foundation/surface-loader/README.md)を参照。旧統合線の緑を移行ブランチに引き継ぎません。[Horizon の再利用判断と検証範囲](../evidence/foundation/horizon-reuse/README.md)を参照。
- **Horizon の再利用を gameplay にも拡大。** 通常スロットとチェスト転送から、1.21.1 実クラス参照と本番 wire 検査を対にして進めています。[移植順序](horizon-gameplay/README.md)、[チェストの検証範囲](../evidence/gameplay/containers/README.md)、[アイテム回収の部分実装](../evidence/gameplay/inventory-pickup/README.md)を参照。エンティティ全般やチェストの開閉・同期まで移植済みという意味ではありません。
- **Valence は部品単位で評価。** 旧版 framework や registry は導入せず、entity角度・section palette・NBT文字列書き出し・有限AABB判定を既存 owner で修復しています。[再利用判断](../evidence/foundation/valence-reuse/README.md)、[角度](../evidence/gameplay/entity-angles/README.md)、[binary/geometryの検証範囲](../evidence/foundation/valence-primitives/README.md)、[分割した契約](valence-primitives/README.md)を参照。entity全体・任意NBT・worldのparityではありません。
- **feature生成境界は限定一致、runtimeは未達。** seed 0・structures=falseのforest 3×3を、nativeと同じ明示source順・tick停止状態で通常バイナリからdumpし、884,736セルすべて0差。leaf属性や流体の除外はありません。元の保存参照との差1,803セルは残り、新しいnative tick有効controlとwriter traceで境界を確認しました。生成順は入力であり、vanilla ticket schedulerは未実装です。[feature contracts](../evidence/gameplay/forest-r1/feature-contracts.md)を数値・範囲の正本とします。
- **kelpは実装済みだが移行後のvanilla parityは未確定。** 正の参照セルを持つ固定具とdraw列・列単位の比較は[過去の記録](../evidence/gameplay/carvers/README.md)にあります。古い一時worldを前提にせず、自前データの意味論を修正した後、同じseed・座標・stageで参照を再構成して比較します。
- **構造物は未実装。** jigsaw、template pool、NBT経由の構造物配置は未対応です。
- **M3は未達。** 地形が一致しない条件で得た旧x0.63値は性能根拠に使いません。比較条件・最新の測定と限界は[生成器の費用](../evidence/gameplay/worldgen-cost/README.md)を参照してください。
- **ローカルsuiteと限定native一致は、全体合格ではありません。** 個別oracle・mutation・全体回帰のreceiptを確認し、hosted CI、whole-world、M2/M3を別々に判定します。以前のleaf-distance属性除外によるno-tick 0は受け入れ根拠から撤回しました。次の境界は[ROADMAP](ROADMAP.md)にあります。
- **コード検証（旧統合線の履歴）:** `tests/verify.sh` 通過時の記録は159 passed、8スレッド×3回、wire smoke、adapterチェック、mutation 53 proved / 0 skipped、layers 16 modules / 40 edges / 0 named errors、debt ratchet 0 over limit。これは現在のデータ移行ブランチの合格結果ではありません。現在の実行結果は上記の移行証拠を参照。
- **clippy削減の履歴:** 457 → 83 → **0**（`--all-targets`・release、当時テスト176本green）。現在の警告数とsuite・層ゲートの件数は実行のたびに変わるので、このページには書きません（数値と条件は各 `evidence/` の receipt が持ちます）。
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
- **開放中の項目:** 独立レビュー（#7）。LICENSEは存在し、未追加のissue #11はclosedです（remoteを再確認）。
- **draft PR #2/#4/#6 は決着しました（2026-10-24）。** `main` に同一patchが無かった62コミットを読み、
  現行線に無い4点だけを取り込みました: play frameの上限をvanillaの2MiBへ、非有限・世界外の移動値の拒否、
  tickの実測（作業時間・TPS・予算超過）、チャンクバッチのACKペーシング。残りは現行線に新しい形であるか、
  vanillaに無いpacket rate limitか、そのブランチ専用のCI修正なので捨てました（理由は取り込みコミットに記載）。draft 3本は閉じ、同名ブランチは削除しました。

## 次の判別作業

次に実行する最小sliceと順序・出口条件は[ロードマップ](ROADMAP.md)だけに置きます。過去の途中診断を、現在の「次の作業」として複製しません。

kelpの列・draw列・heightmapの診断履歴は[carvers](../evidence/gameplay/carvers/README.md)、個別featureの実装と残差は[feature-parity](../evidence/gameplay/feature-parity/README.md)、同梱packと固定jarの方言・出自の差は[自前データ監査](../evidence/foundation/audit-snapshots/README.md)を参照してください。いずれも自前データ移行後の全体受け入れを代替しません。比較規律は[DESIGN.mdの約束](../DESIGN.md#比較の約束)を正本とします。
