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

| chunk | 2 修正前 | 2 修正後 | 残差の内訳 |
|---|---|---|---|
| (0,0) | 194 / 98304 | **5 / 98304 (0.005%)** | 2 列の 1 ブロック段差と water/dirt 1 |
| (0,1) | 243 / 98304 | **36 / 98304** | 12 列 × 3 セルの 1 ブロック段差 |
| (-1,-1) | 228 / 98304 | **0** | — |
| (-2,0) | — | **0** | — |

- 2 修正前の (0,0) は 1221（VERT 修正前）→ 194（VERT 修正後）。
- 残差は terrain の表面が 1 ブロック低い列に集中している（vanilla は y=63 に
  grass、mc-spine は y=62）。surface rule ではなく density の境界（`interpolated` の
  順序）の候補で、feature を止めない比較の差もそこへ乗る。
- feature を止めない比較（同 chunk、seed 0）は **11856 → 10947**
  （11856 は VERT 修正前の作業記録 b53dbbe の値）。残差のトップ組は
  `deepslate <-> tuff`、`andesite/diorite/granite <-> stone` で、feature 層
  （同梱データの版差を含む）にある。地形密度ではない。

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

## 却下: `squeeze` の外側をブロック毎に評価する変更

vanilla の `final_density` は `min(squeeze(interpolated(...)), noodle)` で、
`interpolated` は `mul(...)` の内側だけを包む。mc-spine は `final_density` 全体を
セル隅で評価してから三線形補間しており、`lerp(squeeze(X))` になっていた。
`interpolated` を専用ノードにし、入力だけをセル隅評価して外側をブロック毎に
評価する実装を試した（当時の差分は `~/.cache/mc-spine-work/interp-order-attempt.diff`）。

- 結果: feature なし 7727 → 7723、feature あり 11856 → 11857（**4 セル**）
- 費用: `chunk_cols` 50.9 → 64.4 ms/chunk（同一ビルド・同一 `WGPERF` 条件、+27%）
- 判断: vanilla の順序ではあるが、観測できる差が 4 セルで費用が大きいため**撤回**。
  密度値そのものを比較する検証を入れるときに再適用する。

この測定で「7.9% 残差の主因」という以前の記述は否定された。残差の大半は
feature 層（後述のデータ版差を含む）である。なお 2 修正後、残差 5/36 セルは
この順序の差（density の符号が境界で反転する）に一致する形をしている。

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
- 残差 5（(0,0)）と 36（(0,1)）は 1 ブロックの表面段差。density の補間順序の
  候補であり、原因は未確定（「却下」節の変更を再適用して測る価値がある）。
- 水面の probe は sea level から下向きにしか探さない（既存の `wtop` と同じ）。
  sea より上の湖・池の底面はまだ「水なし」として扱う。
