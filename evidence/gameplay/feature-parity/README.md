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
