# 同じ 1.21.1 データの surface rule を既存評価器につなぐ

Horizon の material condition / biome temperature と実クラス oracle の分離を参考に、
既存 `Gen` の surface owner を修復した。参照 revision は
[`0564ab0`](https://github.com/ludovic111/horizon/tree/0564ab0bcd3235daa6001033261b79aef5f81de1)、
MIT 原本は [`third_party/licenses/horizon.txt`](../../../third_party/licenses/horizon.txt)。
26.3 のレジストリ・閾値・浮動小数点の幅をそのままコピーしていない。

## 修復した契約

- 1.21.1 の inline `noise_settings.surface_rule` を、既存の `rule_node` / `eval_rule` に接続する。
  retained converted dialect の参照 `material_rule` も同じ入口を使う。
- block rule と default block/fluid は既存 `bs_parts` で `Name` / `Properties` を読み、
  state の property offset は既存 `BReg` に任せる。surface 専用の state 表は増やさない。
  このため依存に `worldgen → block_registry` を1本追加した。両者は同じ下位層で、逆依存は増やさない。
- sequence の子が未解析なら全体を拒否する。`filter_map` でその子だけ削除しない。
  unknown rule/condition、解決できない参照は `LOAD_TRACE=1` で名前を出す。
  失敗した名前参照を成功した placeholder としてキャッシュしない。
- `temperature` 条件は noise-router の気候座標ではなく、選ばれた biome の weather を読む。
  biome JSON の `temperature` / `temperature_modifier` を同じ leaf identity に結びつける。

温度の数学は Horizon の `worldgen/biome_temperature.rs` を適応したが、1.21.1 は高度境界が
**固定の y=80** であり、新しい `sea_level + 17` 引数は無い。公式 mappings と pinned bytecode
`Biome` (`ddw`) / `TemperatureModifier.FROZEN` (`ddw$d$2`) / surface temperature helper
(`dzp$g$d`) を確認した。noise は seed 1234 `[0]`、3456 `[-2,-1,0]`、2345 `[0]` の
Legacy/WorldgenRandom 経路。noise の中間値は f64、座標の `/8`・高度補正は bytecode の位置で f32 にする。
`coldEnoughToSnow = !(temperature >= 0.15f)` の否定も保持する。

既存 `OSimp` の複数 octave 経路には、octave zero を `-first` に置き、入力周波数を layer 数から
始める誤りがあった。`PerlinSimplexNoise` (`eoj`) は zero を **highest-octave index** に置き、
周波数を **2^highest_octave** から始める。既存の単一 octave `[0]` 消費者も同じ owner を使う。

## 何を実クラスで比較したか

[`tools/surface_oracle.py`](../../../tools/surface_oracle.py) は既存の authenticated Java runtime を使う。
実行前に server jar の SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` を検査し、
インストール済み overworld settings が同じ jar の JSON か照合する。ダウンロード・サーバー起動はしない。

[`vanilla.json`](vanilla.json) の **97 ケース**:

| 対象 | 実行範囲 |
|---|---|
| 1 surface source | `SurfaceRules.RuleSource.CODEC` で jar の inline tree 全体を decode |
| 29 block states | `BlockState.CODEC` → `Block.getId`。tree の全 result state と非既定 axis / snowy / water-level |
| 9 simplex values | 実 `Biome` の static height/frozen/info フィールドを3座標ずつサンプル。f64 bit pattern を比較 |
| 58 temperatures | 実 `Biome` の private `getTemperature` と public `coldEnoughToSnow`。f32 bit pattern と bool を別々に比較 |

温度 probe は実 `ClimateSettings` と実 `Biome` の**通常コンストラクタ**を呼ぶ。
mob/effect/generation の3フィールドだけ null にしている。上記メソッドがそれらを読まないことを
pinned bytecode で確認した。Player の probe と異なり constructor bypass / Unsafe allocation は無い。
Biome 全体の構築、キャッシュの eviction、world の実行を検証したという意味ではない。

既定 CLI は毎回 Java を再実行して入力・結果・provenance 全体を照合する。
意図した記録だけ `--output` を使う。空、末尾欠落、追加ケース、入力変更、結果欠落、source/settings hash の
変更は失敗する。別 jar では既存の出力を置換しない。日常の Rust/Python fixture-consumer checks に JDK は不要。
`tests/surface_oracle_test.py` を既存 `tests/verify.sh` と CI の provenance step に接続した。

## 負の対照が示した範囲

最初の回帰テストは `inline surface_rule did not resolve` で赤になった。
温度 oracle は旧 `OSimp` で、frozen / base=0 / `(17,150,-31)` の
**−0.08976249 と +0.11023751** の違いを検出した。

ただし温度58ケースだけでは、**octave の配置だけ**を壊す mutant が緑に残った。
ノイズ値が変わっても frozen modifier の分岐結果が同じなら、温度だけの比較では見えないためである。
この試行は成功扱いしない。そこで実 `Biome` の3 noise field の値を直接比較に加えた。
中間値を比較するのは重複検査ではなく、この失敗を検出するためである。

中断した mutation run は source backup を残した。次の実行前に既存の backup と事前 SHA-256 から
復元・照合した。キャンセル後も自動復元されるとは仮定せず、backup 不在と source hash の照合を必須にする。

## 再現

```sh
cargo test --release --offline --locked -j 1 pinned_surface -- --nocapture
cargo test --release --offline --locked -j 1 surface_sequence -- --nocapture
python3 tests/surface_oracle_test.py
python3 tools/surface_oracle.py
MUTATION_FILTER=surface_ MUTATION_RELEASE=1 CARGO_BUILD_JOBS=1 \
  bash tools/mutation_check.sh --offline
```

重い実行は resource admission 後の bounded unit で行う。実行した source/fixture/binary identity と
unit の invocation は `run.json`、各ログはこのディレクトリに保存する。

## この区切りの実行結果

- 新設 Rust **4本**、別プロセスで既存 material-rule **2本**が通過。
  material-rule の既存テストでも実際に chunk の material 結果を構築した（[columns.txt](columns.txt)）。
- native **97ケース**は再実行して入力・結果・provenance 全体が一致。
  source/fixture の guard は **3本**、既存 biome **5本**、container **4本**が通過。
- 改訂した corpus で canonical mutation script を再実行し、**6 proved / 0 not a gate / 0 parked / 0 skipped**。
  各 gate の baseline → mutant → restored control と、実行前後の source hash を検査した。
  これは `MUTATION_FILTER=surface_` の選択結果で、全 campaign の結果ではない。
- all-target clippy `-D warnings`、fmt、Python/shell checks、gate self-tests **15本**、
  layer-gate tests **17本**、layers **19 modules / 44 edges / 0 named errors**、debt **0 over limit** が通過。
- mutation 復元後に production binary を再ビルドし、container / gameplay wire が両方 **PASS**。
- full serial release suite は **185 passed / 24 failed / 1 ignored**。
  前回保存済み pickup run の **180 / 25 / 1** に対して新設4本と既存 provider capture 1本が pass になり、
  新しい失敗名は増えていない。[失敗名の照合](suite-comparison.json) を保存した。
  **24件中15件は `PoisonError`**。残る9テストも共通の欠損入力を含むため、独立原因数とは数えない。
  material-rule 2本も full suite では poison に巻き込まれるが、上記の新しいプロセスでは通る。
- 独立 `codex review --uncommitted` は actionable な問題を指摘しなかった。
  **静的レビューのみ**で、Rust/Java の実行結果は実装側によるもの。

[run.json](run.json) は base commit、全 Rust source と oracle の hash、pack 入力 snapshot、fixture、
測定した binary、各 unit の invocation を結びつける。完了判定は reaped unit の default 値でなく
[unit.jsonl](unit.jsonl) の invocation-bound marker / accounting による。
テキストログの行末空白・末尾空行だけは Git の whitespace gate に合わせて正規化した。
[log-normalization.json](log-normalization.json) に raw / committed の hash を残し、結果値は変更していない。
検証 unit の peak は **1,060,712,448 bytes**、mutation unit は **1,270,898,688 bytes**
（swap peak **52,498,432 bytes**）。どちらも fresh light admission と 2 GiB cap、Cargo jobs 1 で実行した。

## 証明していないこと

surface source の **decode は `apply` ではない**。深部の bedrock/deepslate と条件 dispatch の回帰は
確認するが、vanilla の全 `SurfaceRules.Context` を構築した surface execution oracle ではない。
温度条件テストは biome leaf を明示しており、world の biome 選択の正しさを主張しない。
block-state の比較は上記の有効入力に限定し、任意の不正 property の Codec エラー一致ではない。

`weird_scaled_sampler`、preliminary surface/aquifer router、biome carver/spawner 方言、
surface の scan context・biome lookup、同じ生成段階の全 chunk 比較は残る。
古い capture は再記録していない。forest の stone +4505 / deepslate −3397 をこの修正で解消したとも言わない。
全 migration suite / `tests/verify.sh` / whole mutation campaign の受け入れ、push、canonical merge は別判定である。
