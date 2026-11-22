# H: 樹木と配置が使う契約を下位へ置く

Status: 実装済み。入口は `9c43051`。残る実ソースの指摘はfeature/tree循環とtreeの上向き依存だった。
この1スライスで共有契約の所有者を切り替え、既存アルゴリズムを変えずに両指摘を閉じた。
完了状態の正本は [README](README.md) のH。画面/KDEの検査はユーザー指示により対象外。

## 次の実装者への指示

まず以下の凍結対照を確認する。ソース移動を始める前に、移す定義と実際の全消費者を列挙せよ。
JSON helperの互換wrapperや旧モジュールの再exportは残さず、同一パスで切り替える。
下位化に伴う必要最小限のcrate内可視性以外は、式・分岐・反復順・資産の解釈を変えない。
検証後にはこの入口とREADMEを更新し、H以外の未完了項目を完了へ書き換えない。

## 保存する契約

- ブロックID/metadataの対応、propertyの意味、タグ、ドロップ、物理分類は同じregistryを使う。
- プロバイダの値、乱数の呼出順と消費、predicateの評価順は変えない。
- 生成文脈のread-through/上書き/heightmap/biomeキャッシュと、Genへの可変借用は1所有者のまま。
- 完了印、近傍の巡回順、書込みのflush、既存のcache bound/Mutex/遅延生成はそのまま。
- 保存形式、CLI引数、サーバーの設定モード、Java callback、スケジューラには触らない。
- 世界やRNGの並行ストア、保持callback、trait object、互換facadeを追加しない。

## APIの切れ目

`feature`は配置の解釈と実行、`tree`は樹木の解釈と実行を引き続き所有する。
双方に必要な契約を次の下位所有者へ置く。既存の定義を移し、新しいアルゴリズムを作らない。

| 所有者 | ここに置く責務 | 置かないもの |
|---|---|---|
| `block_registry` | 状態/metadata/タグのregistry、propertyと名前空間の変換、状態だけを見る分類 | 配置起動、Genや生成文脈への依存 |
| `generation_context` | Genの借用、局所上書きと地形/heightmap/biome照会、方向offset | provider/config型、feature/tree実装への依存 |
| `generation_providers` | 整数/高さ/ブロック状態provider、predicate/ruleのデータ・parse・評価、配置可否 | 配置レジストリやfeature/tree実装の起動 |
| 既存`json` | 共有のJSONアクセス関数 | ブロックregistry、地形、providerへの逆依存 |

`FC`の初期化は文脈の所有者に置く。可変Gen参照とflushに必要な上書きだけを実消費者へ渡し、
他のキャッシュを外から組み立てる第2の初期化を残さない。
providerは文脈とregistryを使い、文脈はproviderを参照しない。
地形は先のスライスで既に独立しており、この変更で配置実装への参照を戻さない。

依存の向きは `feature → tree → providers → context → worldgen/registry → json/math`。
同じ値に対する既存の参照は直接の所有者へ付け替え、親facadeへ逃がさない。
新所有者をsource policyへ明示分類し、全てgenerationの明示パス制約の対象に含める。
解消した指摘をaccepted findingsから削除する。目標図だけを0にしてはならない。

## 凍結済みの実行対照

変更前の独立worktreeで、同一probeをCargoのテスト入口から2回、新しいプロセスで実行した。
probeはseed 0、負/正座標・隣接領域への移動・再照会を含む入力列について、返却値、
**全feature上書きの座標と値**、完了領域をソートしてバイト列にする。両回は完全一致した。
これは既存結果の保存対照であり、vanilla oracleではない。

- 基準commit: `9c43051ffa203824ee53085abbdcc48b321d2139`
- 所有worktree: `<HOME>/.cache/megu-audit/H-owner-baseline`
- probeの正本: [generation_capture.rs](../../evidence/foundation/module-layers/generation_capture.rs)
- probe SHA-256: `2ffc81799773ac2dc70c322d7837f1ada476913dd4ec574168d6c95f9e2fed38`
- 正準バイト列: `<HOME>/.cache/megu-audit/H-owner-baseline-1.bin`
- バイト列SHA-256: `0ab88652a45f7d0917c21c8d09f6b610c856f7315c954a9eb81fcde689e3277c`

worktreeのworldgenへ足したのはtest限定のprobe importだけで、製品コードは基準commitのまま。
候補でも同じprobeを使って同じ入力・資産で比較し、test限定importは候補の通常ツリーから外す。
Cargo自身にテスト実行ファイルを選ばせ、既存の`target/debug/deps`から推測して起動しない。
別の実行環境で再開するときは、これらのキャッシュを存在すると仮定せず基準から再生成する。
所有checkoutのworldgen内にtest限定の `generation_equivalence_capture` 子モジュールとして
上のprobeを読み込み、`MEGU_CAPTURE_OUT`を所有出力パスへ設定して、
`cargo test --offline --locked -j 2 worldgen::generation_equivalence_capture::capture_generation_outputs -- --exact --nocapture --test-threads=1`
をcheckoutの根から実行する。資産も同じ基準commitを使う。

## 検証と受入

1. 移動前後の各定義を機械比較し、式の変更と新しい状態の追加がないことを確認する。
2. 上の凍結バイト列に候補を照合する。不一致は期待値の更新で処理しない。
3. 共有primitive・地形への逆依存の負例を保持する。treeからfeatureを参照する負例、
   新下位所有者から上位を参照する負例を、正常系→対象の指摘→復元正常系で確認する。
4. compiler展開由来の現行グラフを**循環0・層違反0**にする。
   検査器の独立2循環negative probeや、辺の追加/削除のratchetは緩めない。
5. registry、樹木、実配置と直接配置の全値比較、遅延lookup順序/再利用の試験を実行する。
6. 定義と共に移動する変異のファイル/anchorを更新する。必須変異のSKIP/未一致/compile failureを許可しない。
7. `tests/verify.sh`の全検査と独立レビューを行い、source hash付きreceiptと対象headのCIを確認する。
   レビューで既存サーバーをprimary cwdから起動させない。実行が必要なら所有fixtureとCargoの出自を要求する。

Hを閉じてもT/F/N/Q、既存ignoredテスト、代表負荷や全vanilla準拠は別の未完了契約として残る。

## 実装結果

- 新所有者: `block_registry`（状態/metadata/タグ/JSON accessor補助）、`generation_providers`（IP/HP/BSP/Pr/RT/PM）、
  `generation_context`（`FC`）。汎用JSON accessorは `json` が所有。`tree` は `feature` を参照しない。
- 現行コンパイラ展開グラフ: **16モジュール・39辺・ソース由来の指摘0**。`policy.json` の `accepted_errors` は空。
- 保存対照:
  - 変更前worktreeの全上書き/完了領域バイト列 SHA-256 `0ab88652…`（独立プロセス2回一致）。
  - 実population出力のMD5 `8afe1b4f2b9cebd5e86f2b5a56371ea9` を `worldgen`
    の常設テストが比較（共有変更が両側同時に入っても隠れない）。
  - provider/predicateの値と後続RNG消費を固定した fixture `provider-baseline.bin`
    （SHA-256 `70f3d70b…`）を `generation_contract_tests` が比較。`Pr::Solid` と `wr`→`rc`、
    `IP::Uniform` の変異で赤くなることを確認。
- `tests/verify.sh` PASS: 各Rust実行 118 passed / 1 ignored（既存ignoredは残件）、ソース制御17、
  変異 **26 proved / 0 skipped**。
- 独立レビュー（pi、read-only、静的）: 修正対象の欠陥なし。P3の指摘は可視性の戻し・
  不要な `pub(crate)` の縮小・population digestの変異追加・`Unobstructed` の複数歩検査に反映済み。
- 対象head `e821bc9` のhosted CI（run 38680194413、rust+mutation）は成功。
- 未確認として残すもの: vanilla全体の準拠、代表負荷性能、並行保存、T/F/N/Q。
