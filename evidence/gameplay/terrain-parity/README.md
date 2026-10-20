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

| | 差分セル | 内訳 |
|---|---|---|
| 修正前 | 1221 / 98304 (1.24%) | deep 722、bedrock 310、feature 188、surface 1 |
| 修正後 | **194 / 98304 (0.20%)** | feature 193、surface 1 |

- 差分のトップ組は `dirt(10) -> grass_block(9)` 179 と `9 -> 10` 11。
  位置は y=54-55 の洞窟床で、vanilla は dirt、mc-spine は grass_block。
- 修正後は deepslate（`stone(1) <-> deepslate(24905)`）と bedrock の帯が一致する。
- feature を止めない比較（同 chunk、seed 0）は **11856 → 11034**
  （修正前の 11856 は直前の作業記録 b53dbbe の値、11034 は修正後の実測）。
  残差のトップ組は `deepslate <-> tuff`、`andesite/diorite/granite <-> stone` で、
  feature 層（同梱データの版差を含む）に残っている。地形密度ではない。

## 修正: vertical_gradient の random_name ハッシュは big-endian

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
feature 層（後述のデータ版差を含む）である。

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
- 194 セルの dirt/grass 差の原因（`WaterCondition` の waterHeight の与え方）は
  未確定。vanilla の条件式自体は `dzp$z$a` と一致している（`waterHeight == MIN`
  なら true、それ以外は `y + stoneDepthAbove >= waterHeight + offset +
  runDepth * mult`）。違うのは `Context.updateY(..., waterHeight, ...)` に渡る値。
