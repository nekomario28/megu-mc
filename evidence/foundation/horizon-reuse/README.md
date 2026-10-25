# Horizon から借りるものと、借りないもの

参照: [ludovic111/horizon](https://github.com/ludovic111/horizon)、調査 revision
[`0564ab0bcd3235daa6001033261b79aef5f81de1`](https://github.com/ludovic111/horizon/tree/0564ab0bcd3235daa6001033261b79aef5f81de1)。
対象は Java Edition **26.3 / protocol 777**。mc-spine の **1.21.1 / protocol 767** の仕様を置き換える参照ではない。
MIT（Copyright (c) 2026 Ludovic、原本を [`third_party/licenses/horizon.txt`](../../../third_party/licenses/horizon.txt) に保存）。
Minecraft の jar・生成データまで MIT と解釈しない。Horizon の runtime crate は依存に追加していない。

## 採用判断

| 候補 | 判断 | mc-spine での使い道と境界 |
|---|---|---|
| `VanillaBiomeExport.java` | **ADAPT（実施）** | built-in preset を vanilla 自身に構築させる。1.21.1 の公式 mappings で名前を解決した別の transport を書き、既存ローダの RTree 形式へ出力する。26.3 の preset データはコピーしない |
| `tools/test_*_parity.py` の Java oracle + Rust probe | **ADAPT（次）** | `NormalNoise.getValue`、surface、aquifer を単位ごとに比較する。oracle は算術を再実装せず実クラスを呼ぶ。1.21.1 の jar、seed、RNG の種類、評価座標を固定する |
| `VanillaChunkGenerationOracle` / `test_chunk_generation_parity.py` | **BORROW_CONCEPT** | 保存後の world ではなく、同じ生成段階の block state・biome・block entity・scheduled tick・postprocessing を比較する。いまの保存 world 固定具の stage 混同を減らす方向 |
| `parity.py` / `parity_shim.py` | **BORROW_CONCEPT** | oracle 結果と依存ファイルのハッシュを結びつけ、Rust だけ変わったとき Java の再実行を省く。ただし mc-spine にはまだ導入しない |
| `structure/template.rs` / `structure/processor.rs` | **REFERENCE_ONLY（当面）** | `fossil` の前提になる NBT template、palette、座標変換、processor の設計候補。単一 feature のコピーでは足りない |
| `worldgen/sculk.rs` と sculk の runtime | **HOLD** | `sculk_patch` の候補。spreader と tick/update の依存、1.21.1 との差を確認してから |
| `horizon_core::jmath` / noise | **REFERENCE_ONLY** | Java と Rust、CPU アーキテクチャの丸め差を調べる入口。26.3 の float/noise 構築を 1.21.1 に移して一致を仮定しない |
| 26.3 の aquifer・density codec・material registry・packet ID | **直接移植しない** | 1.21.1 にはない `exclusion`、別の noise encoding、独立した material registry などがあり、現在のローダ不具合と同じ混在を再発させる |

上流の「281 harnesses」「100%」は上流の自己申告であり、この調査で全実行した結果ではない。
この revision には `tools/test_*_parity.py` が **326 ファイル**あるが、ファイル数も実行成功数ではない。
README 自身も「100% は harness が比較する範囲」と限定している。初期の設計文書には現在の README と
異なる pending 記述も残るため、候補は実装・oracle・比較器まで読む。

### 比較器も負の対照が必要

上流 `test_chunk_generation_parity.py::compare` だけを抽出して合成入力で確認した。
vanilla の1行を `1 2`、Rust 側を `1` とすると、`zip` が末尾を落とし、差の返値は **`(0, 0, 0, 0)`**。
空の reference 辞書でも同じ返値になる。これはその関数の入力検証の不足を示す。
通常の probe がこの欠損を出すことや、上流の実測結果が誤りだったことまで証明してはいない。
**比較器を移すなら、両側の chunk/section/row 長と正の比較数を先に検査する。**

## 今回直したもの：気候 router と、その入力ツリー

前回報告の「biome が `?` なのは tree が無いから」は誤りだった。
`multi_noise_biome_tree.json` は存在した。直接原因は climate router が `.str()` 経由でのみ解決され、
1.21.1 で inline object の `temperature` / `vegetation` が落ちていたこと。
既存の `router_df` を共用し、名前参照・数値・inline object を同じ入口で読むようにした。
失敗時は `LOAD_TRACE=1` が field 名を出す。各 climate field の失敗を列挙するため途中で打ち切らない。

さらに、旧ツリーは **7,594 葉 / 56 biome** で、1.21.1 に無い `dappled_forest`、`pale_garden`、
`sulfur_caves` を含んでいた。jar 内の `multi_noise_biome_source_parameter_list/overworld.json` は
`preset` 名しか持たず、ツリー本体はコードで構築される。JSON のコピーだけでは更新できない。

[`tools/biome_tree_from_jar.py`](../../../tools/biome_tree_from_jar.py) と
[`ExportBiomeTree.java`](../../../tools/biome-tree/ExportBiomeTree.java) は、固定済み jar の
`MultiNoiseBiomeSourceParameterList.knownPresets()` → `Climate.ParameterList.index` → `RTree.root`
を呼んで、**実際の区間と children の順序**を取り出す。tree builder を Rust でもう一度書かない。
1.21.1 の新ツリーは **7,593 葉 / 53 biome**、Nether は **5 葉 / 5 biome**。
jar の SHA-1 と exporter の SHA-256 をツリー内に保存する。

- 実行前に jar の SHA-1 を検査する。別バージョン・不正 jar は Java 起動前に拒否する。
- 空の branch、不完全な preset、7軸でない区間、jar に無い biome を拒否してから置換する。
- `tools/vanilla_pack_from_jar.sh` も同じ exporter を呼び、静的 JSON と built-in tree の更新漏れを防ぐ。
- transport は独自 Java package に置く。default package は署名済み難読化クラスと衝突し、
  `signer information does not match` で失敗した。署名を外すのではなく package を分けた。

再生成には JDK と手元の固定 jar が必要（ダウンロード・サーバー起動・GPU は不要）:

```sh
python3 tools/biome_tree_from_jar.py --jar /path/to/pinned/server.jar
python3 tests/biome_tree_test.py
cargo test --release --offline --locked -j 1 pinned_ -- --nocapture
```

## 検証と残り

`pinned_climate_router_resolves_inline_fields` は修正前に `climate router did not resolve` で失敗した。
`pinned_biome_tree_uses_only_biomes_from_its_jar` は旧ツリーで上記3 biome を名指しして失敗した。
新ツリーの provenance と exporter の失敗時非破壊性は Python テストで検査し、`tests/verify.sh` と CI に接続した。
実行結果とソースハッシュは [run.json](run.json)、負の対照ログは同じディレクトリに保存する。

- focused Rust は **3 passed**（新設2本と既存の RNG pin）、Python は **5 passed**。
- release build、`cargo clippy --all-targets -- -D warnings`、fmt は通過。
- 別の出力先で pack installer を実行し、2回目の実クラスからの export が**バイト一致**することを確認。
- 全 Rust suite は **171 passed / 25 failed / 1 ignored**。25件中15件は共有 test mutex の
  `PoisonError` 連鎖を含むので、25個の独立原因とは解釈しない。既存の feature パス固定、aquifer などが残る。
- 新ビルドで旧 `dappled_forest` 座標 `(3328,64,-1792)` は `sunflower_plains` を返し、`?` ではなくなった。
  これは実行経路の確認であり、その座標での vanilla 一致を示す比較ではない。
- `codex review --uncommitted` は actionable な回帰を指摘しなかった。reviewer は read-only sandbox
  のため Python の一時ファイル試験と Rust/Java 実行を検証できていない。上記は実装側での実行結果。

これは **biome の入力データと読み込みの修復**であり、全座標の vanilla biome 一致ではない。
RTree の tie/search cache、気候ノイズの精度、surface、aquifer は別の比較対象。
`surface_rule`、biome `carvers`、`weird_scaled_sampler`、aquifer 意味論はまだ WIP で、
世界の fixture 受け入れ・全ゲート緑・canonical への統合は主張しない。
次は surface の方言を閉じ、1.21.1 実クラスを呼ぶ小さな noise/aquifer oracle を使って残差を分ける。
