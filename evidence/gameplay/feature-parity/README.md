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
