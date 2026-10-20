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
