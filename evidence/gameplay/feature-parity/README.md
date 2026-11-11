# feature 層の vanilla 一致

2026-10-20。地形（density・surface rule・bedrock）が 0 セル一致した後の、
feature 層の残差を閉じるための記録。

> **更新 (2026-11-12): 同じ seed 0 / chunk (0,0) で残差は 0 セルになった。**
> 下の「11.1%」は 2026-10-20 時点の記録で、その後の codec・feature・template の修正前の値。
> 現在の実測は、pinned 1.21.1 server を隔離ディレクトリで動かして取った chunk (0,0) を
> `tests/data/overworld_chunk_0_0_1211.tsv.gz` として固定し、
> `worldgen::wg_tests::overworld_chunk_matches_native_capture_state_for_state` が
> **正規化 state（名前＋全プロパティ）で 32,518 / 32,518 セル一致**を要求する。
> 参照側は同一入力の2回実行で 4 セルばらつく（diorite ↔ andesite、座標は provenance に記載）ので、
> 「0 セル」は**そのばらつきの床に対して**の値。範囲は 1 chunk・1 seed・Overworld・
> ストラクチャ/エンティティ対象外で、11.1% が測った別シーンの残差を丸めたものではない。

## 出発点

feature/carver を止めない比較（seed 0、chunk (0,0)、wire 復号）は
**10939 / 98304 セル**（11.1%）。トップの状態組:

| a -> b | セル |
|---|---|
| deepslate -> tuff | 1646 |
| andesite -> stone | 1341 |
| tuff -> deepslate | 1068 |
| diorite -> stone | 770 |
| stone -> andesite | 657 |
| stone -> granite | 545 |

つまり差の大半は**石系のバリエーション（andesite/diorite/granite/tuff/deepslate）を
置く ore feature の位置**で、地形でも光でもない。

## 同梱データの版

`datapacks/vanilla` の biome は 1.21.1 の形式ではない（`attributes` を持ち
`spawners` を持たない = 1.21.2 以降）。1.21.1 の server jar と比べると:

- biome 64 個のうち feature リストが一致するのは 16、**48 が不一致**。
- 同梱 feature 定義に無い 1.21.1 の type が 16（`random_patch`, `flower`,
  `seagrass`, `kelp`, `dripstone_cluster` など）、同梱にしか無い type が 9
  （`fallen_tree`, `vegetation_patch` など新しい版のもの）。
- `ore_andesite_lower` / `ore_dirt` / `ore_gravel` の **placed_feature と
  configured_feature は jar と同一**（配置パラメータは一致している）。

## スライス1: biome の feature リストを 1.21.1 に差し替え（実施）

64 biome の `features` を 1.21.1 の server jar から取り直した。1.21.1 に無い
placed feature 名は 2 つだけで、どちらもリストに残した（定義が無い名前は
実行時に skip されるが、`gi` = step リスト内の位置は保たれるので、後続 feature の
seed は動かない）。

- feature ありの block diff: **10939 -> 10874**（-65 セル、-0.6%）。
- トップの状態組は変わらない（石系 ore の位置）。
- feature capture digest は `d1847cd981c73740ca39422c62ad5506` に更新。

**結論**: リストの差は残差の 0.6% だけ。石系 ore の位置ずれは
feature の**実行**（modifier の RNG 消費、height provider、target 判定）側にある。

## 確認したこと（vanilla の bytecode）

- decoration seed: `WorldgenRandom.setDecorationSeed(levelSeed, blockX, blockZ)` で、
  `ChunkGenerator.applyBiomeDecoration`（obf `duz.a(dds,duy,ddq)`）は
  `SectionPos.of(chunkPos, minSection).origin()` の x/z = **chunk の最小ブロック座標**
  を渡す。mc-spine の `pop_seed(wseed, sx*16, sz*16)` はこれと一致する。
- feature seed: `setFeatureSeed(pop, index, step)` の index は step リスト内の位置で、
  mc-spine の `gi` と一致する（前回の FeatureSorter 調査）。

したがって次の対象は、各 feature の **配置 modifier**（count / in_square /
height_range / biome フィルタ）と ore 本体の RNG 消費・target 判定。

## 次の一手

1. `ore_andesite_lower` を 1 つ取り、vanilla の `OreFeature.place` と
   `HeightRangePlacement` / `CountPlacement` の bytecode を読み、RNG の消費順と
   分布を mc-spine と突き合わせる。
2. 予測した最初の vein 中心を、両サーバの andesite セルから逆算して照合する
   （1 seed・1 chunk で位置が一致するか）。
3. 直ったら step ごとの差分表（R1c）へ広げる。

## 主張しないこと

- 10939/10874 は 1 seed・1 chunk の wire 値。feature 全体の一致ではない。
- `FEAT_PROFILE` の時間は診断用で、絶対値の主張には使わない。

## 石系 ore の位置ずれ（2026-10-20 の調査、未解決）

「biome リストか feature index が違う」という仮説を、次の順で確かめた。

1. **RNG の基本演算は一致**: `XoroshiroRandomSource(60007)` の
   nextLong / nextInt(16) / nextFloat / nextDouble を、vanilla の server jar の
   `dzz` を Java から直接呼んだ列と mc-spine の `Xor` で比較し、**8 個の組が
   完全一致**した（seed 60007 = step 6 の gi 7 + 10000*6）。`nextInt(bound)` は
   Lemire の乗算高位 + 棄却で、`RandomSupport` の実装と一致する。
2. **step 6 の並び**: mc-spine の `order[6]` は
   `0:ore_dirt 1:ore_gravel 2:ore_granite_upper 3:ore_granite_lower
   4:ore_diorite_upper 5:ore_diorite_lower 6:ore_andesite_upper
   7:ore_andesite_lower 8:ore_tuff ...`（34 個）。
3. **gi の総当たり**: `ore_andesite_lower` の gi を 0..23 で振り、chunk (0,0) の
   andesite セルを vanilla の 1908 セルと比較。最大の一致は gi=8 の 472（25%）。
   次に step 6 全体を -12..+12 シフトして測ると、+1 が最良で 412（22%）。
   どれも一致しない。
4. **biome の iteration 順**: vanilla の `OverworldBiomeBuilder` の
   parameter list 順（deep_frozen_ocean 始まり、53 biome）を bytecode から
   取り出して `OW_BIOMES` に使うと、chunk (0,0) の feature あり diff は
   10874 → 11070 と**悪化**した（gi は並びに鈍感で、DFS の結果が変わらない）。
   採用せず。

**結論**: 種（decoration seed / feature seed / RNG 演算）は一致している。
残差は ore の**実行**（modifier の適用順・biome フィルタ・vein のループ）にあり、
gi のずれでは説明できない。次の一手は、vanilla の `OreFeature.place` と
`PlacedFeature.placeWithContext` の stream 意味論（どの位置でどの順に RNG を
引くか）を、mc-spine の `gen_pf` と 1 対 1 で突き合わせること。mc-spine 側の
`FEAT_POS` のような位置ダンプと、vanilla 側の Java からの同種の呼び出しで
最初の 1 vein のセル集合を比較するのが最も速い。

この調査で使った一時的な計測（`FEAT_GI`、`FEAT_GI_SHIFT`、`FEAT_POS`、
step リストのダンプ）は、結論を出して**撤回**した（hot path に env 参照を
残さないため）。再現手順はこの節のとおり。

## 2026-10-20（続き）: modifier の適用順と block tag の版ずれ

### 見つけた欠陥1: placement modifier を「全位置に1つずつ」適用していた

vanilla の `PlacedFeature.placeWithContext` は

```java
Stream<BlockPos> stream = Stream.of(origin);
for (PlacementModifier m : placement) stream = stream.flatMap(pos -> m.getPositions(ctx, random, pos));
stream.forEach(feature::place)   // 位置が pull されるたびに feature を実行
```

なので `count: 2` の feature は `x1,z1,y1,<feature の draw>,x2,z2,y2,<feature>` の順に
同じ RandomSource を引く。mc-spine は「1つの modifier を全位置へ適用してから次へ」だったため
`x1,z1,x2,z2,y1,y2` を引き、**同じ seed・同じ gi でも vein の原点が変わる**。

chunk (0,0) の `ore_andesite_lower` で確認した:

| | vein 原点 |
|---|---|
| mc-spine（旧） | (7,15,13), (12,18,3) |
| vanilla の stream 順 | (7,49,13), (3,18,4) |

`tools/ore_oracle.py`（bytecode から書き起こした独立実装）で、旧順序を再現すると
mc-spine の原点と完全一致し、stream 順にすると上表のとおりになる。つまり RNG と seed は
一致していて、差は**引く順序**だけだった。gi の総当たりで一致しなかったのは、位置そのものが
ずれていたため。

修正後、同一条件（seed 0、chunk (0,0)、offline の full-chunk 比較、
`tools/vanilla_region_blocks.py` で復元した vanilla の region と比較）:

| | 差分セル | 割合 |
|---|---|---|
| 修正前 | 11017 | 11.21% |
| 修正後 | 10302 | 10.48% |

上位の状態組は変わらず石系 ore。残差はまだ大きく、別の原因が残っている。

### 見つけた欠陥2: 同梱 block tag が 1.21.1 のデータではなかった

`datapacks/vanilla` の biome は 1.21.1 に差し替え済みだったが、block tag は新しい版のまま
だった。1.21.1 の server jar と比べると 48 個が不一致、16 個が欠落:

- `deepslate_ore_replaceables`: jar = `[deepslate, tuff]`、同梱 = `[deepslate]`。
  tuff に deepslate 系 ore が置けない。
- `dirt`: jar は grass_block / podzol / mycelium / moss_block / mud 等も含む。
- `azalea_grows_on` / `azalea_root_replaceable` は `substrate_overworld`（新しい版の tag）を
  参照していた。

(当時) `tools/vanilla_tags_sync.py` で jar の block tag を書き戻した (このスクリプトは削除済み。タグは `gen_vanilla_data.sh` が jar から生成する)（16 追加 / 48 置換 / 120 は同一）。
chunk (0,0) の feature 出力はこの変更だけでは変わらなかった（ore の target は
`base_stone_overworld` で、差のある tag は地表側のため）。データの版ずれとしては直す。

### 追加した道具

- `tools/vanilla_region_blocks.py`: region file から直接 block 名を数える/セルを出す。
  vanilla を起動せずに比較できる。
- `tools/ore_oracle.py`: `OreFeature.place` と placement modifier を bytecode から
  書き起こした独立実装。gi を振って vanilla のセルと照合できる。
- (削除済み) `tools/vanilla_tags_sync.py`: 以前は jar の block tag を同梱データへ同期していた。現在は `gen_vanilla_data.sh` が生成する。

### 次

残差 10302 セルは石系 ore の位置のまま。次は `tools/ore_oracle.py` で vein 単位の
一致率を上げ、（a）`ctx.get` が前の feature の書き込みを見ているか、（b）高さ provider の
分布、（c）biome フィルタの範囲（chunk 内 vs 3x3）を 1 つずつ潰す。

## 2026-10-20（続き2）: 2本目の vein が合わない = 位置ストリームの draw 数

修正後、chunk (0,0) の `ore_andesite_lower` の原点は
`(7,49,13)` と `(5,6,2)`。`tools/ore_oracle.py`（bytecode からの独立実装、
`next_bound` の棄却しきい値を 64bit に直した後）も同じ 2 点を出すので、
Rust 側の実装は bytecode の意味論と一致している。

vanilla の世界では:

- `(7,49,13)` の vein は**存在する**（andesite の最大連結成分 582 セル、
  centroid (5.4, 48.8, 7.7)）。x/z/y が一致し、セルの 88% が andesite。
- `(5,6,2)` の vein は**存在しない**。そこの block は stone
  （biome は `minecraft:river`、y=3..9 は stone/deepslate が主）。

つまり 1 本目は完全一致、2 本目だけずれる。1 本目の vein が消費する draw は
`nextFloat` 1 + `nextInt(3)` 2 + `nextDouble` × size 64 = 67。2 本目の位置を
「1 本目の後に k 個余分に draw する」として k を振ると:

| 余分な draw | 2 本目の原点 | vanilla の andesite との一致 |
|---|---|---|
| 0（現状） | (5,6,2) | 263 / 1419 |
| 4 | **(1,20,8)** | **510 / 1418** |

k=4 のとき 2 本目は (1,20,8) になり、vanilla には centroid (1.2, 17.5, 8.0) の
263 セル成分がある（x/z が一致）。ただし chunk (1,0) と (0,1) では k を振っても
一致率が上がらない（最大 0.11 / 0.12）ので、k=4 がそのまま正しいとは限らない。
vanilla の `PlacedFeature` の位置ストリーム（`placeWithContext`）の bytecode を
読み、per-position の draw 数を確定させるのが次の一手。

確認済みの切り分け:

- seed（decoration seed / feature seed / step / gi）と RNG 演算は一致。
- `OreFeature.place` の幾何（中心は `Math.sin(double)`、半径は `Mth.sin` の表、
  点は double lerp、collapse 条件、走査順 x→y→z）も一致。1 本目の vein が
  その証拠。
- したがって残差は「位置ストリームの draw 数」か「位置ごとの feature 実行の
  タイミング」のどちらか。

## 2026-10-20（続き3）: biome フィルタの範囲は差を生まなかった（撤回）

vanilla は chunk 自身の 4x4x4 biome セルから `biomesInChunk` を作る。mc-spine は
3x3 neighborhood の chunk 中心（y=0）の biome をサンプルしていたので、
chunk ローカル（全 section の quart セル）へ変えて測った。

- chunk (0,0) の full-chunk 差分: **10302 → 10302（変化なし）**。状態組の内訳も同一。
- 費用: `feat_populate_perf` の `populate9` が **2.494s → 2.818s（+13%）**。
  biome 参照が 9 回から 1536 回/chunk に増えるため。

この chunk では cave biome が無く、差が出ない。効果が無く費用だけ増えるので撤回した
（`src/feature.rs` は元の 3x3 サンプルへ戻した）。cave biome を持つ chunk で差が出る
可能性は残るが、測っていない変更は入れない。

## 次の一手（未実行）

vanilla 側の draw を直接取る。`OreFeature.place` は `dds`（WorldGenLevel）を要求するが、
必要なのは `getHeight` / `getBlockState` / `setBlock` だけなので、Java の
`java.lang.reflect.Proxy` で `dds` を差し替え、`ecz.a(dds, ayw, eeq, ...)` を
直接呼べば「2 本目の位置」を vanilla の実装から取り出せる。そこから
`ayw`（RandomSource）の `nextLong` を数えれば、1 本目が何 draw 使うかが確定する。

## 2026-10-20（続き4）: 隣 chunk の書き込みを見せても改善しない（撤回）

vanilla の `level.getBlockState` は、既に decorate 済みの隣 chunk が書いた block も
返す。mc-spine の `FC::get` は自 chunk の `wr` と地形しか見ていなかったので、
`feat_c`（保存済みの上書き）も参照するよう変えて測った。

- chunk (0,0) の full-chunk 差分: **10302 → 10336（+34 セル、悪化）**。
- 状態組の上位はほぼ同じ。隣 chunk の可視性は population の順序にも依存するので、
  順序が vanilla と違えば「見える集合」も変わり、単純な改善にはならない。

測って悪化したので撤回（`src/generation_context.rs` は元に戻した）。

## 2026-10-20（続き5）: 単一 ore だけの vanilla 世界で vein を直接読む

推測を減らすため、**`ore_andesite_lower` 以外の ore をすべて `count: 0` にした
datapack**（`tools/vanilla_ore_isolate.py`）で vanilla 1.21.1 の新しい世界を作り、
chunk (0,0) の andesite をそのまま読んだ（`tools/vanilla_region_blocks.py`）。

- biome ファイルは jar のままコピーするので step リストの長さと順序は変わらず、
  `ore_andesite_lower` の index は 7 のまま。これを崩すと（リストから抜くなど）
  後続 feature の seed が全部ずれるので、mute は `count: 0` で行う。
- 生成された chunk (0,0) の andesite は **1961 セル、8 連結成分**
  （621 / 264 / 239 / 234 / 216 / 182 / 152 / 53 セル）。他の ore が無いので、
  このセルは全部この feature の出力。

予測（`tools/ore_oracle.py`、gi=7）との比較:

| 予測した原点 | in-box セル | そのうち vanilla で andesite | 比 |
|---|---|---|---|
| (7,49,13) | 474 | 262 | **0.55** |
| (5,6,2) | 497 | 1 | **0.00** |

1 本目は空気セルを除けば一致（0.55 は洞窟の空気ぶん）。2 本目は**存在しない**。
2 本目の位置を「1 本目の vein の後に k draw 余分」として総当たりすると、
k=4 で (1,20,8) になり、vanilla の 264 セル成分（centroid (1.2, 17.5, 8.0)）と
x/z が一致する（比 0.556）。ただし k を大きくすれば他の成分にも当たるので、
k=4 が正しいという証明にはならない。

**確定したこと**: seed・RNG・`OreFeature.place` の幾何は 1 本目で裏が取れている。
**残り**: 1 本目の vein が消費する draw が 67（`nextFloat` 1 + `nextInt(3)` 2 +
`nextDouble` × 64）で足りず、2 本目の位置がずれる。vanilla 側で draw 数と
2 本目の位置を直接取るのが次の一手（`dds` を `Proxy` で差し替えて
`ecz.a(...)` を呼ぶ）。

## 2026-10-20（続き6）: vanilla 自身のコードで draw 数を測る

推測を止めるため、1.21.1 の server jar のクラスを直接呼ぶ Java プローブを書いた
（`Bootstrap` → `OreConfiguration` → `OreFeature.place`、`RandomSource` は
呼び出しを数える Proxy、level は `java.lang.reflect.Proxy`）。

結果:

- **`OreFeature.place`（size 64）が消費する draw は 67**
  （`nextFloat` 1 + `nextInt(3)` 2 + `nextDouble` × 64）。`tools/ore_oracle.py` と
  `src/feature.rs` の前提と一致する。feature 側の draw 数は正しい。
- modifier も bytecode で確認: `InSquarePlacement`（obf `eis`）は
  `nextInt(16)` を x, z の順に 2 回だけ引く。`CountPlacement` は定数なら引かない。
  `HeightRangePlacement`（uniform）は 1 回。`BiomePlacementModifier` は引かない。
- つまり位置 1 = (7,49,13)（3 draw）、その vein が 67 draw、位置 2 = 次の 3 draw で
  (5,6,2) になる。**この導出は vanilla のコードと一致している。**

ところが隔離した vanilla 世界（`ore_andesite_lower` のみ）には
**(5,6,2) の vein が存在しない**（y=4..8 に andesite が 1 セルも無い）。
count=1 の世界では位置 1 の vein が一致し（in-box 比 0.55、残りは洞窟の空気）、
count=2 の世界で増える 789 セルも、こちらの予測する 2 本目の位置
（3x3 の各 chunk）とはほぼ重ならない（一致は 20 セル）。

**未解決**: 位置 2 の 3 draw がどこで消費されるか。候補は
(a) `BiomePlacementModifier` が位置 2 を落としている（biome は river で
`ore_andesite_lower` を持つので説明にならない）、
(b) `place` の中で更に draw がある（プローブでは 67 で確定）、
(c) 位置 1 の vein が実際は (7,49,13) ではない（成分 centroid は (5.6,49,7.8) で
z が 5 ずれる）。

次の一手はプローブを完成させ、**level の chunk 供給（`dcx.a(int,int)` →
`duy` → `dvj`）を差し替えて**、実際に書かれたセルと `placeWithContext` の
位置ストリームを vanilla から直接取ること。現状のプローブは
`level.hasChunkAt` が false を返すため書き込み経路に入らない（draw 数は取れる）。

## 2026-10-20（続き7）: vein の幾何は完全一致（harness で 666/666）

Java プローブ（`tools/vanilla-probe/OreCells.java`）で vanilla の
`OreFeature.place` を偽 level 上で走らせ、書き込みセルを取った。level は
`dcx.a(int,int)` → `duy` → `LevelChunkSection` を差し替え（`Unsafe` で確保）、
`RandomSource` は呼び出しを数える Proxy。

seed 60007・位置 (7,49,13)・size 64 で:

| | セル数 | 一致 |
|---|---|---|
| vanilla `OreFeature.place` | 666 | — |
| `tools/ore_oracle.py` | 666 | **666/666（完全一致）** |

セル一覧は `ore_andesite_666.txt`（seed 60007、(7,49,13)、size 64）。

これで **vein の幾何は確定**: `Mth.sin` の表による半径、`Math.sin` の中心、
double の `Mth.lerp`、collapse、r=1 の楕円体判定、x→y→z の走査はすべて正しい。
残差は**位置だけ**に絞られた。

位置については、チェーン（count 0 draw / in_square 2 / height 1 / feature 67）を
bytecode どおりに再現すると位置2 = (5,6,2) になるが、隔離した count=2 の
vanilla 世界にはその vein が無い（in-box 497 セルのうち 1 セルだけ一致）。
位置1 (7,49,13) は一致する（in-box 474 のうち 262、残りは洞窟の空気）。
したがって「位置1の vein の後、位置2の in_square までに draw が入る」以外に
説明がなく、次はプローブで `ejb`（CountPlacement）と `eis`（InSquarePlacement）の
`getPositions` を直接呼び、draw 数と位置を vanilla から取る。

## 2026-10-22: 残差 40975 → 1035（この節が現在地、詳細は carvers）

この日、feature 層を 40975 セル（2×2、10.4%/chunk）から **1035 セル**まで減らした。
step 単位の分解・生ドローの突き合わせ・bytecode の読みは
[carvers §25〜§42](../../gameplay/carvers/README.md) が持つ（同じ fixture と agent を使うため）。
ここには**原因の一覧**と**次にどこを見るか**だけ置く。

### 原因（すべて「宣言的なものが黙って落ちる」型）

| # | 原因 | 見つけ方 | 効果 |
|---|---|---|---|
| 1 | `minecraft:uniform` の height provider は 1 draw（`eia`）。trapezoid `ehv` と取り違えて 2 draw 引いていた | サーバーの生ドロー（`dzx.c(JII)` + `dyz.c(I)`）と port の `CARVE_RAW` を先頭から比較 | cave 617 → 0 |
| 2 | 地形段階の ore vein（`ore_veins_enabled` + router の vein 3 関数）を 1.21.2 の material rule で実装していた | 公式 mappings + Vineflower で `OreVeinifier` | 48 → 0 |
| 3 | vein の乱数が `factory` ではなく `fromHashOf("minecraft:ore")` の派生 | サーバー内で toggle/ridged/gap を実測 | 10 → 0 |
| 4 | `WorldgenRandom` は `dyn` の legacy draw（modern 形では ore origin が 242/242 ずれ） | 生ドロー列 | 40975 → 34679 |
| 5 | height provider 4 種と `below_top` の −1、`next_inbetween` の draw 抜け | 公式 mappings + 逆コンパイル | 34679 → 33853 |
| 6 | `setDecorationSeed` は Xoroshiro + **加算**（`(x*l + z*m) ^ seed`）。chunk (0,0) だけは 0 なので露見しない | サーバーの `setFeatureSeed` ログと port の index 集合を step ごとに比較 | 33853 → 3472 |
| 7 | feature の並び順が依存する biome 集合は biome source の **53 件**（64 件の registry 順ではない） | `deg.a` の第 1 引数を文字列で出力 | 3472 → 2510 |
| 8 | `underwater_magma` は origin が水であること + 包囲判定 | 逆コンパイル | 1236 → 0 |
| 9 | `placed_feature/` の版ずれ（`trees_birch_and_oak` が無く木が全部スキップ） | 1.21.1 jar と名前を突き合わせ | — |
| 10 | rule-based state provider の `type` 欠落、`"fluids"` の単一値（disk 系が 0 書き込み） | `CF_MISSING=1` / `PF_UNKNOWN=1` | 2510 → 1856 |
| 11 | 入れ子の **placed feature 参照**が modifier を落としていた（`would_survive` が走らず全候補で木が生える） | 木の本数 53 vs 38 → 生ドローの形 | 1856 → 1035 |
| 12 | spring の上下が逆（below を見るべきところ above、走査は down でなく up） | 逆コンパイル | 木の位置の理解（水は tick で広がる） |
| 13 | biome を 2 種取り違え（chunk container を読んでいたが、`BiomeFilter` は `BiomeManager.getBiome` を読む） | `dcz.t` の default が `F_().a(pos)` を呼ぶことを bytecode で確認 | 1035 → **940**、木 45 → 42 |

### 残差 1035 の読み方

ブロックごとの集合比較（`vanilla-only` / `ours-only`）が次の一単位を決める:

| ブロック | ours-only | vanilla-only | 読み |
|---|---|---|---|
| diorite / andesite / granite / tuff | 419 | **0** | ore は vanilla を包含（上位集合）。差は後続の置換 |
| gravel / dirt / water | ~0 | **379** | `ore_dirt` / `ore_gravel` のセル欠け + spring の水 |

- 葉の `distance` 380 は **tick 由来**（`LeavesBlock.updateShape` は tick を予約するだけ）。
  `tools/carver_diff.py` は既定で除外する。
- 樹冠の幾何 ~470 は木の本数差（45 vs 38）。未解決の `BiomeFilter` の false を先に割る。

### 次の一単位

1. 木の残差を chunk 別に潰す（(0,1) 8 vs 5 が最大）。同じ計装
   （`InSquarePlacement` の生候補・`BiomeFilter` の可否・`would_survive`）を
   chunk (0,1) に当てる。
2. `ore_dirt` / `ore_gravel` の blob が vanilla の**部分集合**（121 + 96 セル）。同じ
   ore 機構で他の 4 種は一致しているので、この 2 つ固有の条件（run 順が先頭、
   `size:33`）を突き合わせる。

## 63. `geode` を実装した（`Cf::Geode`）と、まだ一致しない形

`tools/feature_type_coverage.py` の計測で、overworld の biome から到達される未実装型は 11 件、
`geode` は **56 biome** から到達される最大の欠落だった。`p_cf` の腕が無い型は `Cf::Unknown`
に落ちて**何も置かない**ため、`CF_MISSING`（configured feature の解決失敗）にも出ず、
「その biome に geode が無い」のと区別できなかった。

### 実装

`tools/bytecode_probe.sh` で難読名を引き（`GeodeFeature` = `eck`、設定は `eej` / `dyv` / `dyx` / `dyw`）、
vineflower でソースに落として移植した（手順は `tools/README.md`）。要点:

- `distribution_points.sample` 個の点を引き、各点を `outer_wall_distance` で3軸オフセット。
  air か `invalid_blocks` に落ちた点が `invalid_blocks_threshold` を超えたら**配置ごと中止**。
- `d0 = points / outer_wall_distance.max`、層の閾値は `1/sqrt(layer + d0)`（filling / inner /
  middle / outer）。クラックは `base_crack_size + nextDouble()/2`（点が3個超なら `+d0`）。
- 立方体の各ブロックで `d6 = Σ (1/sqrt(distSqr + pointOffset) + noise)`、**noise は点ごとに足す**。
  `d6 < d4` は外、`d1` が filling（中空＋クラック時は隣接6方向をその流体に）、`d2` が inner
  （`use_alternate_layer0_chance` の同じドローが結晶を吊るかの判定にも使われる）、`d3` が middle、
  それ以外が outer。**書かない場合でも provider は呼ぶ**（ドローはそこで起きる）。
- 結晶は `inner_placements` から1つ引き、6方向を順に試して `FACING` を向け、
  `WATERLOGGED` は対象セルの流体から、air か**水源**の最初の方向に置く。
- ノイズは `NormalNoise.create(rng, -4, 1.0)`。bytecode で `create(RandomSource,int,double...)` は
  `NoiseParameters(firstOctave, amplitudes)` を作るだけ（＝1オクターブ）と確認したので
  `normal_new_l(level_seed, -4, &[1.0])` とした。`WorldgenRandom(LegacyRandomSource)` の
  `nextInt(bound)` は LCG では委譲される（bytecode の `instanceof dyz` 分岐）ので、素の LCG で足りる。

### ゲート

- `tick_tests`…ではなく `generation_contract_tests::geode_layers_are_concentric_bands_around_its_points`:
  ノイズ0・クラック0・点1個の設定で、**各セルの属すべき層をテスト側で閾値から導出**して全セルを照合する
  （分岐の入れ替え、閾値、`d0`、`distSqr` の `pointOffset` 抜けが落ちる）。
- `generation_contract_tests::geode_abandons_itself_when_a_point_lands_in_air`:
  `invalid_blocks_threshold` は閾値であってフィルタではなく、超えたら何も置かない。
- `tools/geode_fixture.sh`（既定 seed 12 / chunk (0,0)、参照 chunks 4、geode セル 614）:
  vanilla とセル単位で比較し、両側に geode が無ければ失敗する。

### まだ一致しない（測定値）

| ブロック | ours | vanilla | shared | IoU |
|---|---|---|---|---|
| amethyst_block | 162 | 157 | 110 | 0.53 |
| calcite | 202 | 201 | 103 | 0.34 |
| smooth_basalt | 237 | 256 | 74 | 0.18 |
| 3層合計 | 601 | 614 | 287 | **0.31** |

最良の平行移動（+1,0,0）でも IoU 0.389 なので、**ずれではなく形が違う**。y=-24〜-30 のスライスを
見る限り層の並び・大きさは同じで、境界が1ブロック前後ずれる。ノイズを 0 にすると IoU 0.267、
3倍にすると 0.074、6倍で 0.006 と単調に悪化する（＝ノイズの**向きは合っているが場が違う**）。
次に触るなら、**port と vanilla agentのドロー列を geode の添字で突き合わせる**
（`FSEED_ARM=cx,cz,i,step` は step 2 / i 2 が geode）か、`normal_new_l` の1オクターブ正規化を
参照値と比べる。ここを飛ばして形を合わせにいくのは §CV1/§CV2 の失敗になる。

### 63.1 次の測定をやった：配置のドローは vanilla と完全一致、残差はノイズの場

`tools/carve-agent`（`-Dfseedchunk=0,0 -Dfseedarm=2,2`）で vanilla 側の geode のドローを出し、
port の `FSEED_ARM=0,0,2,2` と並べた（seed 12）:

| # | vanilla (`RDRAW`) | port (`DRAW`) |
|---|---|---|
| 0 | nextFloat = 0.0014750957 | nextFloat = 0.0014750957489013672 |
| 1 | nextInt(16) = 5 | nextInt(16) = 5 |
| 2 | nextInt(16) = 3 | nextInt(16) = 3 |
| 3 | nextInt(89) = 26 | nextInt(89) = 26 |
| 4 | nextInt(2) = 0 | nextInt(2) = 0 |
| — | nextDouble = 0.5666987405771309 | nextDouble = 0.17697429956041832 |
| — | nextDouble = 0.9393808564749294 | nextFloat = 0.33311378955841064 |
| — | nextDouble = 0.4303670590541948 | nextInt(3) = 2 |

rarity（1/24 を切る 0.001475 ✓）、in_square の (5,3)、height の 26（→ y = -32）、
`distribution_points` の `nextInt(2)=0`（→ 3点）まで**値まで一致**。つまり
**原点・点数・pos は同一**で、その先の差はノイズに帰属する（§63 のセル比較・ノイズ倍率の単調悪化と整合）。

ただし vanilla 側の 3 つの `nextDouble` は**同じ RNG の続きではない**: geode は
`NormalNoise.create(new WorldgenRandom(new LegacyRandomSource(level.getSeed())), -4, 1.0)` を
*配置のたびに*作るので、その構築ドローが同じ `f=2,2` として混ざって出る（agent は
`FEAT` スレッドローカルでラベル付けするだけで、RNG の出所は見ていない）。しかも agent は
noise 構築の `nextInt(bound)` を hook していない（`RBOUND` が 1 行も出ていない）ので、
このログでは index を合わせた比較ができない。

→ **次に必要なのは noise の値の oracle**: agent に `-Dnoisedump=x,y,z`（`eoh.a(DDD)D` を
instrument して値を出す）を足し、port の `normal_new_l(12, -4, &[1.0])` と座標ごとに比べる。
`tools/carve-agent/README.md` のビルド手順（ASM 9.8 + javac）で足せる。ここを飛ばして
（例えば倍率を合わせにいくのは）§63 の測定が示すとおり悪化するだけ。

## 64. 装飾順はスケジューラ由来だった（「順序非依存フィクスチャ」の前提が崩れる）

§62.3 では port の装飾順を固定具の領域に対して辞書順に寄せても残差が減らなかった。その
「固定具の順序」自体を測るために、agent の `-Ddecoorder`（`applyBiomeDecoration` に入った
chunk 座標を出す）で vanilla 側の順序を取った（seed 0、`forceload add 1264 -16 1359 79`）:

```
-3 1  -3 -1  -3 0  -2 -1  -2 0  -2 1  -1 -1  -1 0  -1 1  0 -1  0 0  0 1
-2 -2  -1 -2  0 -2  -3 -2  -4 -2  -4 -1  -4 0  -3 2  -2 2  -1 2  -4 1  -4 2  0 2
-5 -1  -5 0  -5 1  -5 -2  -5 2  -4 -3  -3 -3  -2 -3  ...
```

**辞書順でも半径順でもない**。forceload が作った chunk がどの順で生成されたか（＝サーバーの
ticket と worker のスケジュール）がそのまま出ている。つまり

- vanilla の装飾順は**固定具のプロパティではなく実行の副産物**で、再実行ごとに変わりうる
  （§62.1 の「参照の実行列ノイズ床 2〜7 セル」がこれ）。
- だから「vanilla の順序に合わせた固定具」は原理的に作れない。順序に依存しない固定具が
  要るなら、順序を合わせるのではなく**干渉源を取り除く**しかない。

具体的には、kelp の残差は同 step の**先行 feature（seagrass）が書いた状態**を見るかどうかの
クラスなので（§62.1 の 31 列＋未説明 61 列）、固定具は「seagrass を 0 にした pack で kelp だけを
残す」形にする（`tools/vanilla_nofeat_pack.py --keep` の系統）。そうすれば kelp 実装そのものの
正しさが、seagrass の parity と混ざらずに測れる。§62.4 の heightmap 修正で kelp のセル数が
動かなかったのも同じ理由で説明できる（高さではなく先行状態の問題だった）。

次に触るならこの固定具。`-Ddecoorder` の実測（この節）と `tools/kelp_fixture.sh` が入口。

## 65. `sea_pickle` を実装した（`Cf::SeaPickle`）と、到達測定が塞がっている理由

未実装の到達型 10 件のうち最も小さい `minecraft:sea_pickle`（`edj`、`SeaPickleFeature`）を移植した。

- 設定は `CountConfiguration`（`{"count": 20}`）。`count` 回、`nextInt(8) - nextInt(8)` を x と z に
  それぞれ2回引いて ±8 のオフセットを作り、その列の `OCEAN_FLOOR` 高さを取り、
  **その後に** `nextInt(4) + 1` を引いて pickle 数にする（＝拒否される試行もドローを消費する）。
- 生存条件は2つ: 対象セルが `minecraft:water` であること、下のブロックが
  `SeaPickleBlock.mayPlaceOn`（`dmw.b`: UP の衝突面が空でない、または magma）を満たすこと。
  port では seagrass と同じ `side_solid(below, 1)` に magma を加えた形。
- 置く状態は `SeaPickleBlock` の既定（`pickles=1 waterlogged=true`）に `pickles` だけを上書きする
  （bytecode の `this.k(this.E.b().a(c, 1).a(d, true))` で確認。既定は waterlogged=**true**）。

ゲート: `sea_pickle_places_on_water_over_a_solid_top_face_and_draws_first`（海面パッチで2個置かれ、
`pickles` が 1..4、`waterlogged=true`、y が列の OCEAN_FLOOR、下が固体上面、そして
**RNG が試行あたり5ドロー×2 だけ進む**こと）と
`sea_pickle_skips_cells_that_are_not_water`（air パッチでは何も置かず、それでも10ドロー消費）。
型カバレッジの未実装到達型は 10 → **9**。

### 到達/parity の測定が塞がっている（2つの理由、どちらも実測）

1. `sea_pickle` は `warm_ocean` の step 9 にしか無い。spawn 近傍（±800）を seed 0,1,2,3,4,5,6,7,8,9,10,11,13,17,21 で
   走査して warm_ocean は1つも無く、seed 0 で見つかったのは (-250,-2750)（spawn から172 chunk）だった。
2. その遠さだと **port の offline dump が返ってこない**: `MC_SPINE_WG_DUMP_X=-16 …_Z=-172` は
   wall 240 秒で `user 0m0.319s`（＝計算していない、待っている）。近い領域（kelp 固定具の (79,-1)、
   79 chunk）は 5 秒で終わる。port の hot region の外では `populate` が待ちに入るらしく、
   **dump ベースの固定具は hot region の外では作れない**（これは今回の変更とは別の、それ自体報告に値する事項）。

次に測るなら、① hot region の外でも dump が進むようにする（`populate` の待ちを診断）か、
② warm_ocean が spawn 近くにある seed を探して `tools/geode_fixture.sh` の形（ブロック4種を
`sea_pickle` 1種に変えるだけ）で parity を取る、のどちらか。

## 66. `ice_spike` を実装した（`Cf::IceSpike`）

`IceSpikeFeature`（`ecq`、config は `{}`）を逆コンパイルして移植した。

- 空気のあいだ下に降り、最初の非空気ブロックが `snow_block` であることを要求（駄目なら**ドローせず** false）。
- `above(nextInt(4))`、`nextInt(4) + 7` 段、半径 `height / 4 + nextInt(2)`。半径が 1 を超えるときだけ
  `nextInt(60) == 0` を引き、当たれば `above(10 + nextInt(30))` の**浮いた**spike になる。
- 各段で `f = (1 - h/height) * radius`、`r = floor(f)`、`|dx|,|dz| <= r` のうち
  「中心か `fx^2+fz^2 <= f^2`」かつ「辺でないか `nextFloat() > 0.75` でない」セルに packed_ice を置く。
  **`nextFloat()` は辺のセルでだけ引かれる**（Java の `||` 短絡）ので、翻訳でもそのまま短絡させている。
- 下向きには `h != 0 && r > 1` のとき `pos.offset(dx, -h, dz)` も同じ条件で埋める。
- 仕上げに半径 `clamp(radius - 1, 0, 1)` の範囲で `above(-1)` から下へ掘る。掘る条件は
  air/leaves/snow_block/**ice**/packed_ice（`Blocks.dO` は `powder_snow` ではなく `ice` だった）、
  予算 50 を使い切ったら `below(nextInt(5) + 1)` だけ**下に飛ばして**予算を引き直す。ループは `y > 50` で止まる。

移植中に踏んだ罠2つ（どちらも bytecode で確認して直した）: `BlockPos.c(int)` は `above(int)` ではなく
**`below(int)`**（上に飛ばすと `y > 50` に到達せず無限ループになる。実際 60 秒以上回って気づいた）、
`dga.dO` は `powder_snow` ではなく `ice`。clippy の `neg_cmp_op_on_partial_ord` は*直さない*:
Java の `!(a > b)` は NaN で真、Rust の `<=` は偽なので、否定のまま変数に名前を付けて残した。

ゲート: `ice_spike_builds_a_tapering_spike_on_snow`（雪の台の上で、`nextInt` 列を再生して求めた
基準 y と高さから tip を導出、tip が1列であること、下向きに snow を置換していること、
置いたブロックが packed_ice のみであること）と `ice_spike_does_nothing_without_snow`
（雪でなければ何も置かず**1ドローも消費しない**）。mutation 2本を追加（高さと材質）で 58 proved / 0 not a gate。
型カバレッジの未実装到達型は 9 → **8**。

## 67. 残り8型の実測トリアージ（実装コストと、塞いでいるもの）

未実装の到達型 8 件を逆コンパイルして行数と依存を数えた（`--only=` は公式 mappings で引く）。
「小さい＝安い」ではないので、**新規インフラが要るか**を並べる。

| 型 | 行数 | 引くサポート | 判定 |
|---|---|---|---|
| `fossil` | 58 | `enu`/`ent`/`enp`（StructureTemplate, StructurePlaceSettings, プロセッサ）, テンプレート ID のリスト（config） | **要テンプレート基盤**。port に構造物パイプラインが無く、58行に見えて一番高い |
| `sculk_patch` | 60 | `dmu` = SculkSpreader（`dmu.b()`） | **要SculkSpreader移植**（charge/cursor の確率ウォーク、別クラスで100行超） |
| `desert_well` | 99 | `dga` のみ ＋ `dqj.N` の **block entity** に archaeology の loot table（`ern.bd`） | 構造は純ロジックだが suspicious_sand の loot table を落とすと部分的（block は置けるが brush で何も出ない）→ 条件付き |
| `root_system` | 109 | `duz`（BlockStateProvider）, `awk`（FluidTags）, `dmb`? ではなく **placed feature 参照**（`Holder treeFeature`）, BlockPredicate | **純ロジック**。port にある部品だけで書ける。設定は12フィールド |
| `dripstone_cluster` | 173 | `dga` ＋ `awk` | 純ロジックだが大きい |
| `large_dripstone` | 195 | `dga` ＋ `awk` | 同上 |
| `iceberg` | 231 | `dga` のみ（`nextDouble` を4回引いて形を決める二形状） | 純ロジックだが最大。ヘルパ5本 |
| `speleothem_cluster` | — | 公式 mappings にクラスが無い | **1.21.1 の型ではない**（このリポジトリの同梱 pack は 1.21.2+ 変換版）。1.21.1 を参照にしている以上、忠実な移植は原理的に不可 |

次に触るなら `root_system`（純ロジック最小、木の placed feature 参照は `gen_pf` で既にある）、
その次が `dripstone_cluster`/`large_dripstone`（クラスタ系）。`fossil` は構造物基盤を入れるまで着手しない。
`desert_well` は block entity の loot table まで含めて扱えるときに。

## 68. `root_system` を実装した（`Cf::RootSystem`）— ゲートは規則の単位テストまで

`RootSystemFeature`（`edg`、config は 12 フィールド）を逆コンパイルして移植した。

- 原点が空気でなければ即 false（**ドロー前**）。上へ1つずつ動かしながら
  `allowed_tree_position` と `required_vertical_space_for_tree` を通る位置を探し、
  足元が溶岩流体でなく `isSolid` であることを確認してから、入れ子の placed feature（木）を走らせる。
  木が置けたら `origin.y` からその位置までの**根の柱**を作り、最後に**垂れ根**を周囲に撒く
  （垂れ根は「木が置けたときだけ」＝vanilla の `if (placeRootsAndTree(...))`）。
- 根の柱は 1層ごとに `root_placement_attempts` 回、中心から `±root_radius` の dx,dz を引いて
  `root_replaceable` タグのセルだけを provider の状態に置き換える（タグに合わなければ**ドローしない**）。
- 垂れ根は 3軸オフセット（各 `nextInt(span) - nextInt(span)`）を引き、空気セルで、かつ
  **上のブロックが下面 solid** のときだけ置く（`HangingRootsBlock.canSurvive` と同じ条件なので1つのテストで足りる）。

移植前に公式 mappings で解決した難読名（推測すると全部間違える種類）:
`BlockPos.d()`=`above()`, `e()`=`below()`, `BlockState.i()`=`isAir()`, **`BlockState.e()`=`isSolid()`**
（`isAir` ではない）, `LevelReader.u(pos)`=`isEmptyBlock`, `FluidTags.a`/`b`=`WATER`/`LAVA`。

移植中に踏んだ罠: **`ctx.wr` はマップなので、既存セルへの上書きは `len()` を変えない**。
「木が置けたか」を `wr.len()` の増分で判定していたため、置けていても0と読めた。マップ全体の比較
（`let before = ctx.wr.clone(); ...; ctx.wr != before`）に変えて意味が通るようになった。

### ゲート（現時点で主張できる範囲）

- `root_system_vertical_water_allowance_follows_the_config`: `isValidVerticalState` の規則
  （空気は常に可／水は `i + 1 <= allowed_vertical_water_for_tree` まで／その他は不可）と
  `FluidTags.WATER`/`LAVA` の述語を単体で固定。
- `root_system_does_nothing_when_its_origin_is_not_air`: 埋まった原点では何も置かず、**1ドローも消費しない**。
- 型カバレッジの未実装到達型は 8 → **7**（`fossil` `iceberg` `sculk_patch` `desert_well`
  `dripstone_cluster` `large_dripstone` `speleothem_cluster`）。clippy 0。

### 未解決（正直に）

**木→根の柱→垂れ根の end-to-end は未検証**。この固定具では入れ子の placed feature が何も書かず
（inline `disk` と inline `simple_block` を、それぞれ候補位置で直接走らせても書き込み0）、
その原因（harness 側か、port の `gen_cf` の当該アームの前提か）を切り分けていない。
そのため parity も未測定で、上の2つのテストは「規則」と「入口」だけを固定している。
次に触るなら、まず harness で入れ子 feature が書く最小例を作る（実在の `minecraft:oak` も
この合成地形では0だったので、地形の作り方＝床/天井の高さや `ctx` の状態が疑わしい）。

## 69. `desert_well` を実装した（`Cf::DesertWell`）— 形と2ドローまで固定

`DesertWellFeature`（`ebw`、config は `{}`）を移植した。テンプレートも block entity も要らない
純ブロックロジックで、残りの中で最小（99行）。

- 原点の**1つ上**から、空気のあいだ下へ降りる（`isEmptyBlock` かつ `y > min_y + 2`）。
  着地したブロックが `sand` でなければ false（config の `BlockPredicate.matchesBlocks(SAND)`）。
- 足元の柱が「1つ下と2つ下が両方空気」なら中止（井戸の足場に地面が要る）。
- 以降は固定: slab の底板（dx,dz ±2 / dy −2..0）、水柱（原点と水平4方向）、その下に sand（同じ5セル）、
  y+1 の縁、四隅の支柱、y+4 の屋根（中央 = sandstone、周 = slab）、四隅の砂岩柱（y+1..3）。
- 最後に `suspicious_sand` を2つ、`[origin, east, south, west, north]` から `nextInt(5)` で2回選んだ列の
  +1 と +2 に置く。

mappings で先に解決した名前: `Blocks.I/J/aV/jF/G` = SAND / **SUSPICIOUS_SAND** / SANDSTONE /
SANDSTONE_SLAB / WATER、`BlockPos.d()`/`e()` = `above()`/`below()`、
**`.f()/.g()/.h()/.i()` = north/south/west/east**（この並びが2つの suspicious sand の位置を決める）、
`LevelReader.u(pos)` = `isEmptyBlock`。

### ゲート

- `desert_well_builds_its_shape_on_sand`: 着地セルからの固定オフセットで水柱・砂床・底板・屋根・支柱・柱を
  1セルずつ照合し、**2つの suspicious_sand が `nextInt(5)` の再生結果と一致すること**、合計ちょうど2個であること。
- `desert_well_needs_sand_and_draws_nothing_otherwise`: 石の上では何も置かず**1ドローも消費しない**。
- mutation 2本（屋根中央の材質、`nextInt(5)` の bound）で 60 proved / 0 not a gate。

**既知の欠落（port の chest と同じ扱い）**: ブロックは置くが suspicious_sand に
archaeology の loot table を付けない（block entity の loot を持たないため、brush しても何も出ない）。

型カバレッジの未実装到達型は 7 → **6**（`fossil` `iceberg` `sculk_patch` `dripstone_cluster`
`large_dripstone` `speleothem_cluster`）。

## 70. `iceberg` を実装した（`Cf::Iceberg { state }`）— 2つの形と signed distance

`ecr`（`IcebergFeature`）231行を移植。config は `{state}` で、bundled pack の `iceberg_packed` は
`packed_ice`、`iceberg_blue` は `blue_ice`。

- 原点はまず **海面に移される**（`ChunkGenerator.getSeaLevel()`）。テストは原点を海面より12上から
  渡し、この再アンカーが効いていることを固定する。
- 頭のドローが形を決める: `nextDouble > 0.7`（fire）、`nextDouble * 2π`（angle）、`nextInt(5)`、
  `nextInt(3)`、**2つ目の `nextDouble > 0.7` が tall/flat を選ぶ**、height（tall 6..11 / flat 3..17、
  flat では `nextDouble > 0.9` のときさらに `nextInt(19) + 7`）、`min(height + nextInt(11), 18)`（i1）、
  `min(height + nextInt(7) - nextInt(5), 11)`（j1）、半径スケール k1（tall は `11 - nextInt(5)` / flat 11）。
- 上向きループは dx 外・dz 中・y 内の3重（**この順が draw 順**）。半径は tall が
  `heightDependentRadiusRound`（ドローなし）/ flat が `heightDependentRadiusEllipse`
  （`nextFloat` + 条件付き `nextInt(5)`/`nextInt(6)`）。
- 書き込みゲート `placeIcebergBlock` は air/snow_block/ice/water にだけ書き、`!tall || nextDouble > 0.05`
  のロール、`fire` なら `depth <= nextInt(max(height/3,1)) + height*0.6` で snow_block に切り替える
  （**`&&` と `||` の短絡がドロー数を決める**ので順序ごと保存）。
- 彫り込みは signed distance: flat は円（`IcebergFeature.signedDistanceCircle`、1コールにつき
  `nextFloat`）、tall は `angle` で回した楕円（`signedDistanceEllipse`）。`d0 < 0` のセルだけ書き、
  `d0 > d1 && nextDouble > 0.9` のセルは飛ばす（d1 は tall −0.5 / flat `-6 - nextInt(3)`）。
- 下向きループは tall のとき `floor(k1 * (1 - y²/(i1*8)))` で細り、`heightDependentRadiusSteep`
  （1コール1ドロー）を使う。`smooth` が孤立セルと「下が空気」のセルを air に戻す。
- 最後に `nextDouble > 0.1`（tall）/ `> 0.7`（flat）で頂部パス。ここは `nextBoolean` 4回と
  `nextInt` 数回で中心を決め、**ループ条件に `nextInt(5)` が入る**（反復ごとにドロー）ので
  `while y > -max_height + rng.next_bound(5)` としてそのまま保存した。

mappings で先に解決した名前: `Blocks.a/G/L/dN/dO/dP/iC/mW` = AIR / WATER / GRAVEL / SNOW / ICE /
SNOW_BLOCK / **PACKED_ICE** / **BLUE_ICE**（`isIcebergState` = packed_ice || snow_block || blue_ice）、
`ChunkGenerator.f()` = `getSeaLevel()`、`RandomSource.h()` = `nextBoolean()`、`.i()` = `nextFloat()`、
`.j()` = `nextDouble()`、`Mth.a(FFF)` = `clamp`。

### ゲート

- `iceberg_builds_a_shape_around_sea_level`: 海を掘って原点を海面+12で渡し、(a) config の state
  （packed_ice）が実際に置かれること、(b) 置かれたセルの dx/dz が再生した k1 の範囲内、
  (c) dy が再生した i1/l の範囲内、(d) **水面より上と下の両方に氷がある**（iceberg の定義）を固定。
  頭のドロー列を同じ seed で再生して境界に使うので、draw 順がずれれば落ちる。
- mutation 2本（海面への再アンカー、config state の材質）で 62 proved / 0 not a gate / 0 skipped。

**測っていないこと**: セル単位の vanilla 一致。`iceberg` は cold/ocean 系バイオーム限定で、
fixture に使える近傍の ice biome が未特定（`sea_pickle` の warm_ocean と同じ到達性の問題）。
したがって形の一致は未検証で、ここで固定したのは draw 順・範囲・海面跨ぎまで。

型カバレッジの未実装到達型は 6 → **5**（`fossil` `sculk_patch` `dripstone_cluster`
`large_dripstone` `speleothem_cluster`）。

## 71. `dripstone_cluster` を実装した（`Cf::DripstoneCluster`）— `Column` と2つの tip

`eby`（`DripstoneClusterFeature`）173行を移植。クラスタは半径を x/z 別に引く楕円で、各列は

1. **`Column`（`dyo`）**で自分のいる空気/水のスパンを求める。上方向スキャンの終点が **ceiling**、
   下方向スキャンの終点が **floor**（`create(floor, ceiling)` の引数順がそのままフィールド順）。
   どちらも「空気でも水でもない最初のセル」で、`range` 回までしか動かない。
2. `nextFloat < wetness` で「濡れた列」かを引き、濡れていて `canPlacePool` が通れば
   **floor セル自体を水にして floor を1つ下げる**（後続の石筍がそのぶん深く立つ）。
3. 天井側・床側それぞれ `nextDouble < chance` → thickness プロバイダ → `isLava` ガード →
   `getDripstoneHeight`（`nextFloat > density` なら 0、そうでなければ距離バイアス付きの
   clamped normal）。
4. 両 tip が届く場合は **合流点を1ドローで決めて双方を短縮**し、`nextBoolean` で
   `tip_merge` にするか決める。

draw 順は列ごとに固定（`dx` 外・`dz` 内の順）。`wetness` は clamped normal（Gaussian 1回）、
`density` は uniform float（`nextFloat` 1回）なので、**短絡も含めた順序がそのまま形を決める**。

mappings で先に解決した名前: `dyo`=`Column`（`getCeiling()->b` / `getFloor()->c` /
`withFloor()->a` / `getHeight()->d`）、`bpq`=`ClampedNormalFloat`、`bqa`=`TrapezoidFloat`、
`bqb`=`UniformFloat`、`bps`=`ConstantFloat`、`ebz`=`DripstoneUtils`、`dtz`=`DripstoneThickness`、
`dlq`=`PointedDripstoneBlock`、`dga.H/su/st`=LAVA/DRIPSTONE_BLOCK/POINTED_DRIPSTONE、
タグ `awe.bf/bt`=`base_stone_overworld`/`dripstone_replaceable_blocks`、`ji.a/b`=DOWN/UP、
`jd.h(int)`=**`atY(int)`**（`above(int)` は `b`）、`Mth.a/b(FFF...)`=`clampedMap(float/double)`。

### 新しく入れた層: float provider（`FP`）

`dripstone_cluster` が pack で最初の float provider 利用だったため、`bpu` の4型を全部書いた:
`constant`（ドローなし）/`uniform`（`nextFloat` 1回、**`min_inclusive`/`max_exclusive`**）/
`clamped_normal`（Gaussian 1回、f64 で計算して f32 へ切り戻し）/`trapezoid`（`nextFloat` 2回）。
`clamped_normal` は **config の f32 を f64 に昇格**して計算する（`0.1f32 as f64` が値の一部）ので、
`fp_clamped_normal` を feature と共有している。ドロー数と値は
`float_providers_draw_and_clamp_like_the_vanilla_types` が双子 RNG で固定（4型すべて）。

### 固定具が見つけた欠陥: `(dcz)` キャストは多重定義の識別

最初の実装は天井/床のガードを `canBeAdjacentToWater` で書いていた。症状は
**乾いたギャラリーで何も生えない**（`tips=0 blocks=0`）。`ebz` には `a(...)` が2つあり、

- `a(dcz, jd)` = **`isLava`**（`LevelReader`）
- `a(dcx, jd)` = `canBeAdjacentToWater`（`LevelAccessor`）

逆コンパイル出力は `this.a((dcz)$$0, ...)` と **明示キャスト**を出しており、それが識別子だった。
`isLava` なら石の天井は通る（正しい）。`canBeAdjacentToWater` だと石の天井が自分の鍾乳石を拒否する。
**逆コンパイルの明示キャストは多重定義の識別マーカーとして読む**、が今回の教訓。

### ゲート

- `dripstone_cluster_hangs_stalactites_below_and_stands_stalagmites_above`（乾いたギャラリー）:
  (a) 全書き込みが引いた半径の内側、(b) `vertical_direction` ごとの run が **スパンの隣接セルから
  始まる**（down は `ceiling-1`、up は `floor+1`）、(c) 層順が base → middle* → frustum → tip、
  (d) dripstone block はスパン内に出ない、(e) dry の golden（tips=209 / blocks=675 / 64列）。
- `dripstone_cluster_pools_lower_the_stalagmite_into_the_water`（**水で満たしたスパン**）:
  `canPlacePool` は四方と上が base stone/水であることを要求するので、乾いた洞窟では決して通らない。
  濡れた列では floor が1つ下がり、石筍が**プールの水を置き換えて waterlogged=true** で立つ。
  ここを固定（プールのセルと tip が同一セルを共有しないことも）。
- mutation 2本（`isLava` ガード、tip の向き）で **64 proved / 0 not a gate / 0 skipped**、clippy 0。

**測っていないこと**: セル単位の vanilla 一致。`dripstone_caves` のチャンクを固定具にする必要があり、
水スパン側は実地形の地下湖が要る。golden は「seed 31 + pack config の回帰ピン」であって
vanilla との比較ではない。float provider の値も vanilla dump ではなく式の双子との一致まで。

型カバレッジの未実装到達型は 5 → **4**（`fossil` `sculk_patch` `large_dripstone`
`speleothem_cluster`）。

## 72. `large_dripstone` を実装した（`Cf::LargeDripstone`）— 2つの spike と風

`ecu`（`LargeDripstoneFeature`、195行）を移植。原型は `dripstone_cluster` と同じ `Column` だが、
こちらは **両端のある Column（`dyo.b`）で高さ 4 以上**を要求する＝洞窟そのものを探す。

- 半径は `(int)(洞窟高 * max_column_radius_to_cave_height_ratio)` を `column_radius` の範囲に
  clamp してから `randomBetweenInclusive` で1回引く。
- 天井側（`ceiling-1`、`up=false`）と床側（`floor+1`、`up=true`）に spike を作り、それぞれ
  bluntness と height_scale を引く（**この順**）。両方が `valid`（半径と bluntness のしきい値）なら
  `WindOffsetter` が作られ、風速と角度の2ドローが消費される。
- `find_base` は洞窟から**離れる向き**（鍾乳石は上へ、石筍は下へ）に `min(10, getHeight(0))` 歩き、
  円がほぼ石で埋まっているセルを根として採用する。見つからなければ半径を半分にして繰り返す。
- `build` は**空のセルに** dripstone block を置き、既に何か置いた後に base stone に当たったら止まる
  （= 洞窟の中に spike が生える）。天井側は y 上限なし、床側は `WORLD_SURFACE` で頭打ち。

mappings で解決した名前: `eel`=`LargeDripstoneConfiguration`（9フィールド）、
`ecu$a`=`LargeDripstoneFeature$LargeDripstone`、`ecu$b`=`...$WindOffsetter`、
`Mth.sin/cos/sqrt(float)`=`a/b/c`、`dyo.b` の `ceiling()/floor()/height()`=`e/f/g`。

### 固定具が見つけた欠陥: 引数の数でしか区別できない多重定義

最初の実装は build の判定を `isDripstoneBase` で書いていた。症状は**スパン内に何も置かれない**
（岩の中に置こうとするので、テストの「span の中にあること」で落ちる）。逆コンパイルは

```java
if (ebz.b($$0, $$11)) { … }
```

で、`ebz.b` には **2引数**（`b(dcx,jd)` = `isEmptyOrWaterOrLava`）と **1引数**（`b(dtc)` =
`isDripstoneBase`）がある。引数の数が唯一の手掛かりで、`isEmptyOrWaterOrLava` が正しい
（空セルに置く＝spike が生える）。`dripstone_cluster` のときの `(dcz)` キャストと同種の罠で、
**「似た名前の多重定義は引数の数と明示キャストまで読む」**を skill §28 に追記。

### 参照ビルドの死んだコード（記録のみ・移植せず）

同じ jar の `ecu.a(dds,jd,dyo$b,ecu$b)`（mappings 名 `placeColumn`）は
`Blocks.DIAMOND_BLOCK` / `GOLD_BLOCK` / `CREEPER_HEAD` を置くが、**誰も呼んでいない**:

- `place`（`ecu.a(ecg)`）はこの4引数メソッドを呼ばず、内部クラスの2つだけを呼ぶ
- javap で `ecu` 全体を走査しても、このメソッドのディスクリプタを参照する呼び出しが **0件**

定数の奇妙さを確かめた手順も記録に値する: mappings（公式 `server.txt`）と **jar 自身の
`<clinit>`**（`ldc "diamond_block"` → `putstatic dga.cz`）が独立に一致した。生きている経路が
使う `dga.su`=`dripstone_block` は同じ `build` の判定（`state.is(dga.su)`）とも整合する。
つまり vanilla 1.21.1 は未使用のデバッグ用らしきメソッドを出荷しており、**移植対象は生きた経路**。

### ゲート

- `large_dripstone_hangs_and_stands_a_tapered_spike`（スパン7）: 置かれたセルが全て `dripstone_block` で
  **スパンの中**（岩の中ではない）、引いた半径の内側、そして各列のセルが
  **天井側と床側それぞれの「端の隣セル」から連続する2本の run とちょうど一致**することを固定。
  半径は再生値（=3）。golden 37セル / 9列。
- `large_dripstone_sways_when_both_spikes_clear_the_wind_gate`（スパン15、seed は
  wind gate を満たすものを探索し、満たさなければ落ちる）: 同じ seed で wind speed だけを
  `Uniform(0.0,0.0)`（**同じドロー数**で変位ゼロ）に差し替えた run と比べ、
  **各セルが `floor(wind * (origin_y - y))` だけ水平に動いた集合と厳密に一致**することを固定
  （= 風の式そのもの）。golden 126セル、seed 7。
- mutation 2本（build の判定、風の変位）で **66 proved / 0 not a gate / 0 skipped**、clippy 0。

**測っていないこと**: セル単位の vanilla 一致（`dripstone_caves` のチャンク固定具が必要）。
それは同時に、死んだ `placeColumn` が実地形で本当に呼ばれないかを確かめる唯一の手段でもある
（そういう run を取るまでは「未呼び出し」は bytecode の観測であって地形の観測ではない）。

型カバレッジの未実装到達型は 4 → **3**（`fossil` `sculk_patch` `speleothem_cluster`）。

## 2026-10-28: biome 集合の y 範囲修正と、その影響範囲

`Gen::chunk_biome_set` が `qy in 0..height/16*4` で走査していたため、**y < 0 だけに存在する
biome が chunk の biome 集合から落ちていた**（`biome_quart` は block y ÷ 4 を取る）。
修正は `min_y / 4` から走査する（commit `b13b65f`、guard テスト
`chunk_biome_set_covers_the_negative_y_range`）。

実測した影響:

- 深層暗黒の chunk (-16,112)-(-15,113)（seed 0）では `deep_dark` が集合に入らず、
  `sculk_patch_deep_dark` と `sculk_vein` が**一度も走っていなかった**（port の sculk 系
  セル 0 / 参照 397）。修正後は同区画で 429 セルを書く。
- 一方、**forest (-64,32) 3x3 の生成は 1 セルも変わらない**（修正版と旧版の port dump を
  `cmp` で比較して 0 セル）。したがって上の feature capture digest
  `d1847cd981c73740ca39422c62ad5506` を含め、この区画で記録済みの値は修正の影響を受けない。
- `tools/chunk_world_fixture.sh forest -4 2 3 features`（tick 後に保存する saved-world
  比較）は 2566 セル差だが、これは R1 の **paused native** 0 差とは別の fixture であり
  （script の docstring のとおり runtime 側）、sculk feature はこの区画の biome 集合に
  入らないため、この 2 つの変更（sculk port と biome 集合修正）はどちらも寄与しない。
