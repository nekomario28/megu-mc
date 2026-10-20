# feature 層の vanilla 一致

2026-10-20。地形（density・surface rule・bedrock）が 0 セル一致した後の、
feature 層の残差を閉じるための記録。

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

`tools/vanilla_tags_sync.py` で jar の block tag を書き戻した（16 追加 / 48 置換 / 120 は同一）。
chunk (0,0) の feature 出力はこの変更だけでは変わらなかった（ore の target は
`base_stone_overworld` で、差のある tag は地表側のため）。データの版ずれとしては直す。

### 追加した道具

- `tools/vanilla_region_blocks.py`: region file から直接 block 名を数える/セルを出す。
  vanilla を起動せずに比較できる。
- `tools/ore_oracle.py`: `OreFeature.place` と placement modifier を bytecode から
  書き起こした独立実装。gi を振って vanilla のセルと照合できる。
- `tools/vanilla_tags_sync.py`: 1.21.1 jar の block tag を同梱データへ同期。

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
