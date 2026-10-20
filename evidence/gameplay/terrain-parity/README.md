# 地形（feature/carver なし）の vanilla 一致

2026-10-20。`chunk_block_diff.py` の残差を「地形」「surface rule」「feature」に
分離して測るための固定具と、その結果。

## 固定具

`tools/vanilla_nofeat_pack.py <server.jar> <world-dir>` が 1.21.1 の server jar
（ネストした `server-1.21.1.jar` でもよい）から biome を読み、`features` を空、
`carvers` を空にした datapack を `world-dir/datapacks/nofeat` に書く。
**新しい world（`level-name`）で起動する**（生成済みチャンクは再生成されない）。
mc-spine 側は `MC_SPINE_NOFEAT=1` で feature 層を止める。

両者の `chunk (0,0)` を `tools/chunk_block_diff.py` で比較すると、残るのは
density・surface material rule・aquifer だけになる。

リポジトリの `datapacks/vanilla` から biome を取ってはいけない。同梱データは
1.21.1 ではない（下記）。

## 結果（seed 0、chunk (0,0)、feature/carver なし）

world overlay（以前のセッションが残したブロック編集）を外した状態で測り直した。
overlay があると mc-spine 側だけに数セルの差が乗る。

| chunk | 3 修正前（2 修正後） | 3 修正後 |
|---|---|---|
| (0,0) | 5 / 98304 | **0** |
| (0,1) | 36 / 98304 | **0** |
| (-1,-1) | 0 | **0** |
| (-2,0) | 0 | **0** |

- (0,0) の履歴: 1221（3 修正前）→ 194（vertical_gradient 修正後）→
  5（surface rule 修正後）→ 0（interpolated 順序修正後）。
- feature を止めない比較（同 chunk、seed 0）は **11856 → 10939**
  （11856 は VERT 修正前の作業記録 b53dbbe の値）。地形が一致してもこの数は
  ほとんど減らない。残差のトップ組は `deepslate <-> tuff`、
  `andesite/diorite/granite <-> stone` で、feature 層（同梱データの版差を含む）
  にある。地形密度ではない。

## 修正1: vertical_gradient の random_name ハッシュは big-endian

`SurfaceRules$VerticalGradientConditionSource`（obf `dzp$y`）の条件
（`dzp$y$a.a()`）を `javap` で確定した:

1. `y <= trueAtAndBelow` → true、`y >= falseAtAndAbove` → false
2. その間は `Mth.map(y, true, false, 1.0, 0.0)` を `d` とし、
   `PositionalRandomFactory.at(x, y, z).nextFloat() < d`
3. その factory は `RandomState.getOrCreateRandomFactory(name)` =
   `worldRandom.forkPositional().fromHashOf(name).forkPositional()`
4. `fromHashOf(String)` は `StringHash.fromString` = md5 の先頭 16 バイトを
   **Guava `Longs.fromBytes`（big-endian）** で 2 つの long にし、factory の
   seedLo/seedHi と XOR する

mc-spine は同じ連鎖を持っていたが、md5 の読み出しだけ `from_le_bytes` だった
（`Split::of_str` は `from_be_bytes` で正しい）。`src/worldgen.rs` の
`Cond::VGrad` 構築を big-endian に直した。deepslate と bedrock floor は同じ
`Cond::VGrad` を使うため、両方の帯が同時に一致した。

回帰は `worldgen::wg_tests::vertical_gradient_seed_reads_the_name_hash_big_endian`
が固定する。feature 入力が変わるため feature capture digest も更新した
（`51d6e2fbb958f1ae42a998bc39a15f74`）。

## 修正2: `not_underwater` は条件そのもの、waterHeight は実流体の面

`SurfaceRules$WaterConditionSource$1WaterCondition`（obf `dzp$z$a`）は

1. `waterHeight == Integer.MIN_VALUE` → true
2. それ以外は `blockY + stoneDepthAbove >= waterHeight + offset + surfaceDepth * multiplier`

で、名前のとおり**すでに「not underwater」**である。grass_block の分岐はこれを
そのまま使う。mc-spine は `not(water(...))` と二重否定にしており、さらに `fluid` を
「海面以下の density<=0 の最初のセル + 1」で近似していたため、**水中の海底が
grass、乾いた地面が dirt** になっていた（(0,0) の 194 セルは水中の海底）。

修正:

- `not_underwater` を `Cond::Water { off: 0, mult: 0, sd: false }` そのものにする。
- 列の水面を、既存の heightmap 用 aquifer probe（`wtop`）から先に求める。
- 表面窓のスキャンで vanilla の `buildSurface` と同じ流体追跡をする:
  流体セルは `waterHeight` を一度だけ設定し、空気セルは MIN に戻す。
  地形より上は probe した水面までが水、地形より下は aquifer が water/lava を
  返したセルだけを流体として扱う。

回帰は `worldgen::wg_tests::not_underwater_is_grass_only_above_the_water_surface`
（水中の海底 = dirt、乾いた表面 = grass）。feature capture digest も更新した。

## 修正3: `interpolated` の入力だけを補間し、外側はブロック毎に評価する

vanilla の `final_density` は `min(squeeze(interpolated(X)), noodle)` で、
`interpolated` は `X` だけを包む。mc-spine は `final_density` 全体をセル隅で
評価してから三線形補間しており、`lerp(squeeze(X))` になっていた。

一度は撤回している。surface rule がまだ間違っていた時点で試すと、残差
7727 → 7723（feature なし）と 4 セルしか動かず、`chunk_cols` は
50.9 → 64.4 ms/chunk（+27%）だったため。surface rule 修正後の残差 5/36 が
ちょうど「1 ブロックの表面段差」だったので、同じ変更を再適用した。

実装:

- `DF::Interp` を追加し、`interpolated` を `Cache` と区別する。
- 評価器に `cc: Option<&ChunkCols>` を通す。`cc` があるとき `DF::Interp` は
  チャンクの格子から三線形補間した値を返し、無いとき（格子を作る隅の評価）は
  入力の評価と同じ。
- `cols()` は `final_density` から到達できる `DF::Interp` ごとに 5×5×ny の
  格子を作る（`Gen::interp_ids` の到達性探索）。`ChunkCols::density` の代わりに
  `Gen::block_density` が外側の演算をブロック毎に評価する。

結果（feature/carver なし）:

| chunk | 3 修正前 | 3 修正後 |
|---|---|---|
| (0,0) | 5 | **0** |
| (0,1) | 36 | **0** |
| (-1,-1) | 0 | **0** |
| (-2,0) | 0 | **0** |

費用: `chunk_cols` 48.8 → 70.8 ms/chunk（同一 `WGPERF`、+45%）。
**M3 の x0.63 は地形が違う状態の値**なので、R3 で代表負荷を測り直す。
地形パリティを優先して採用し、費用は別に扱う。

## 残差の次: 同梱 datapack が 1.21.1 ではない

`datapacks/vanilla` の biome は 1.21.1 の形式ではない:

- 1.21.1 の biome は `carvers`(dict)・`spawners`・`creature_spawn_probability` を持ち、
  `attributes` を持たない。同梱データは `attributes` を持ち `spawners` を持たない
  （1.21.2 以降の形式）。
- 1.21.1 の biome 64 個と feature リストを比べると、**16 個だけ一致、48 個が不一致**。
- 1.21.1 にあって同梱 feature 定義に無い type が 16（`random_patch`, `flower`,
  `seagrass`, `kelp`, `basalt_columns` など）、同梱にしか無い type が 9
  （`fallen_tree` など新しい版のもの）。`patch_dry_grass_badlands` /
  `patch_firefly_bush_near_water` は 1.21.5 の feature。
- feature リストが違えば `FeatureSorter` の step 内 index と
  `setFeatureSeed(seed, index, step)` が変わり、配置位置がずれる。

したがって feature 層の一致を測る前に、worldgen データを 1.21.1 の server jar
から取り直して固定する必要がある。`noise_settings` の `final_density` は
ファイル分割・式の書き換えがあるが、展開すると等価であることは確認済み
（y の lerp 連鎖と 0.64 の位置）。

## 主張しないこと

- 1 seed・1 chunk・wire の復号値。クライアント画面やGUIは見ていない。
- feature/carver を止めた比較であり、通常のゲーム挙動の一致ではない。
- 水面の probe は sea level から下向きにしか探さない（既存の `wtop` と同じ）。
  sea より上の湖・池の底面はまだ「水なし」として扱う。
- 費用の +45% は、地形が一致した状態での M3 の再測定（R3）で扱う。
