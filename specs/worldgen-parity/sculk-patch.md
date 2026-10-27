# `sculk_patch` の移植計画と実装状況

2026-10-27 起票 / 2026-10-28 実装（未検証）。`sculk_patch` は他の feature 型と違い、**1 つのサブシステム**
（`SculkSpreader` と `SculkBehaviour` 実装群）を伴う。ここに、次に着手する人が
知り直さなくて済むよう、実測した構造・分岐・罠・再利用できる部品を固定する。
コードはまだ書いていない（半端な subsystem を緑と記録しないため）。

## 対象クラス（固定 jar、公式 mappings）

| 役割 | クラス | obf |
|---|---|---|
| feature 本体 | `SculkPatchFeature` | `edi` |
| config | `SculkPatchConfiguration` | `eez` |
| 拡散器 | `SculkSpreader` | `dmu` |
| カーソル | `SculkSpreader$ChargeCursor` | `dmu$a` |
| sculk ブロックの振る舞い | `SculkBlock` | `dmq` |
| vein の振る舞い | `SculkVeinBlock`（`MultifaceBlock` 拡張） | `dmv` |
| 振る舞い interface + 既定実装 | `SculkBehaviour` / `SculkBehaviour$1` | `dmp` / `dmp$1` |
| multiface の道具（facing ビット・spreader） | `MultifaceBlock` | `dky` |

参照は `tools/vanilla_src.sh <official class>` で作れる（repo 外・SHA-1 検査つき）。

## 同梱 pack の 2 つの config と、その意味

| config | charge_count | amount_per_charge | spread_attempts | growth_rounds | spread_rounds | catalyst_chance | extra_rare_growths |
|---|---|---|---|---|---|---|---|
| `sculk_patch_deep_dark` | 10 | 32 | 64 | **0** | 1 | 0.5 | uniform 1..3 |
| `sculk_patch_ancient_city` | 10 | 32 | 64 | **0** | 1 | 0.5 | 0 |

**重要**: 両方とも `growth_rounds = 0` なので、pack の config では
`SculkBlock` の **growth 分岐（`spread_only == false`）は一度も実行されない**
（rounds = `spread_rounds + growth_rounds` = 1、`spread_only = i < spread_rounds` = true）。
growth 分岐まで検証するには、cases 用に `growth_rounds = 1` の config を用意する
（config はデータなので可能）。

## feature 本体（`edi.a(ecg)`）

1. `canSpreadFrom(level, origin)`:
   - `state.getBlock() instanceof SculkBehaviour` → true
   - それ以外: `!isAir && (!is(water) || !fluidState.isSource())` → false（つまり air か
     水源だけが許される）。その後 `Direction.stream()` の**6 近傍**のいずれかが
     `isCollisionShapeFullBlock` なら true。
2. `spreader = SculkSpreader.createWorldGenSpreader()`（`dmu.b()` =
   `new dmu(true, tag=bU, 50, 1, 5, 10)`。`d()`=50 charge 分母, `e()`=1, `f()`=5, `g()`=10）
3. `rounds = spread_rounds + growth_rounds` 回:
   - `charge_count` 回 `spreader.addCursor(origin, amount_per_charge)`
   - `spread_only = i < spread_rounds`
   - `spread_attempts` 回 `spreader.updateCursors(level, origin, rng, spread_only)`
   - `spreader.clear()`
4. catalyst: `rng.nextFloat() <= catalyst_chance` かつ `origin.below()` の
   `canSurvive` → `sculk_catalyst` を origin に置く。
5. `extra_rare_growths.sample(rng)` 回: `origin.offset(nextInt(5)-2, 0, nextInt(5)-2)` が
   air かつ below の `isFaceSturdy(UP)` → `sculk_shrieker[CAN_SUMMON=true]`。

罠: 5 の **2 つの `nextInt(5)` は sample の後に**来る（`sample` → ループ → x, z）。
`spreader.updateCursors` の `level.levelEvent(3006, ...)` は**モデル化しない**
（sound/particle は block ではない。end_gateway の block entity と同じ扱いで明記する）。

## 拡散器（`dmu`）

- `addCursor(pos, amount)`: `amount` を 1000 ずつに割って `ChargeCursor(pos, part)` を
  **最大 32 個**まで追加（超えたら捨てる）。
- `updateCursors(level, origin, rng, spread_only)`: 全カーソルを更新し、
  - 生き残ったカーソルを位置で**マージ**する。`!world_gen` なら合計 `<= 1000` のとき
    1 本にまとめる（world gen では常に別カーソルとして残す）。
  - 位置ごとの合計 charge を数え、`levelEvent`（no-op）を出す。
- `ChargeCursor.update`（`dmu$a.a`）:
  - `charge > 0` かつ（`spread_only` か `level instanceof ServerLevel`）のときだけ進む。
  - `update_delay > 0` なら減らして終わり。
  - 現在位置の state から振る舞いを取り、`spread_only` のとき
    `behaviour.attemptSpreadVein(...)` を呼ぶ（成功したら sound = no-op）。
  - `charge = behaviour.spreadOneCharge(...)`、0 以下なら `behaviour.decay(...)`。
  - そうでなければ `findNextSculk`（18 近傍をシャッフルして走査、
    `MultifaceBlock.pack` で facings を、vein の `attemptSpreadVein` の戻りで早期終了）で
    次の位置を決め、`isWorldGeneration` で原点から **15 ブロック**を超えたら charge=0。
  - `facings` は次位置の state が `SculkBehaviour` なら `MultifaceBlock.getFacings`。
  - `decay_delay = behaviour.decayDelay(...)`、`update_delay = behaviour.updateDelay()`。
- 18 近傍の並び: `BlockPos.betweenClosed(-1,-1,-1 .. 1,1,1)` から
  「少なくとも 1 軸が 0」かつ原点でないセル（=18）を `Vec3i` 化したもの。
  **この順序**で作ったリストを `Util.shuffledCopy` するので、順序がそのまま draw になる。

## 振る舞い（`dmp`）

- `SculkBlock`（`dmq`）:
  - `spread_one_charge`: `charge != 0 && rng.nextInt(50) == 0` のとき、原点からの
    距離と `clampedMap` で「育つか」を決め、`charge_count` 未満なら
    `sculk` を **`pos.above()`** に置く（`nextInt(11) == 0` なら shrieker
    `[CAN_SUMMON=world_gen]`、それ以外は sculk。`WATERLOGGED` は
    `isWaterAt` で立てる）。戻りは `max(0, charge - charge_count)`。
  - 育たないときは `rng.nextInt(10)` の値で charge を減らす（`decreaseCharge`）。
  - `decay`: `charge <= 0` のとき `sculk` を `sculk_vein` に置き換える（`decay` 実装）。
  - `isWorldGen` 由来の `isWorldGen()` = false なので、`ChargeCursor` の
    `d()` 分岐で state を取り直す点に注意。
- `SculkVeinBlock`（`dmv`）: `MultifaceBlock` の spreader を使う。**port 側は既存の
  `multiface_state` / `multiface_spread` / `multiface_shuffle` がそのまま使える**
  （`sculk_vein` は 6 面すべてを取れる multiface）。
- 既定（`dmp$1`）: `facings == null` のときは vein の spreader で
  `spreadAll`、`facings` が空でなければ `attemptSpreadVein`、
  非 air かつ非水源なら false。

## port の置き場所

`src/feature.rs` に `struct SculkSpreader` / `struct SculkCursor` と
`fn spc_*` を置く（同じ layer。モジュールを増やすと layering gate の登録が要る）。
必要になる新しい block predicate は 2 つだけ:

- `isCollisionShapeFullBlock`（既存の `is_solid_render` とは別物。`st_side` の全面か
  `boundingBox` から引けるか要確認）
- `isFaceSturdy(level, pos, dir)`

## fixture の設計（次のセッションでそのまま使う）

- cases を 2 つ: `sculk_patch`（pack の `sculk_patch_deep_dark`、growth なし）と
  `sculk_patch_growth`（`growth_rounds: 1`, `spread_rounds: 1` の合成 config）。
- scene: `air`（何も無し → 近傍に collision-full が無いので refuse）、
  `deepslate_floor`（`deepslate` の床 → 拡散の起点）、
  `deepslate_floor_water`（水源を挟む）、
  `sculk_start`（原点が `sculk` → SculkBehaviour 経由で開始）、
  `ceiling`（上に deepslate）。
- box は広めに（spread は 15 ブロックまで届く）: `[[-16,16],[-8,20],[-16,16]]`。
- gate 候補: charge 分母の `nextInt(50)`／18 近傍の順序／マージ上限 1000・カーソル上限 32／
  catalyst の `nextFloat` と `canSurvive`／shrieker の `CAN_SUMMON` と `isFaceSturdy`／
  `growth_rounds` 分岐の有無。

## 比較経路：whole-chunk capture（2026-10-27 に確立・実測）

feature 単体の oracle は使えない（下の「測った障害」）。**深層暗黒 biome の実チャンクを
生成して sculk セルだけを照合する**経路が確立した。

### 手順（再現コマンド）

```bash
# 1. 参照: 固定 1.21.1 の実サーバーで deep dark の 2x2 チャンクを生成
tools/chunk_world_fixture.sh dd3 -16 112 2 features      # seed 0、約3分
# 2. port の dump（1 秒）
MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_DUMP=/var/tmp/dd3.bin MC_SPINE_WG_DUMP_N=2 \
  MC_SPINE_WG_DUMP_X=-16 MC_SPINE_WG_DUMP_Z=112 MC_SPINE_SEED=0 ./target/release/mc-spine
# 3. sculk セルだけを照合
tools/sculk_gate.py /tmp/chunkfix-dd3/world /var/tmp/dd3.bin 2 -16 112 [--require-match]
```

`chunk_world_fixture.sh` に `SEED` / `view-distance` / `simulation-distance` の固定を足した:
参照が再現しないと 1 セルも証明できないため（既定 seed 0、距離 10）。

### チャンクの選び方（実測）

deep dark は origin 付近には無い。port の気候 router で走査して座標を決めた
（`MC_SPINE_WG_PROFILE=1 MC_SPINE_BIOME_AT=...`、chunk 中心を y=-63 で):
`container=minecraft:deep_dark` になるのは **chunk (-16,112) と (-15,112) 付近**
（±512 ブロックの走査で 3468 点中 108 点が deep dark、多くは z≈1536-2048）。
`container` を見るのは、feature 層の biome フィルタが読むのがそれだから
（`blended` と `container` が違う地点もある）。

### 参照（vanilla, seed 0, chunks (-16,112)-(-15,113)）の sculk セル

| block | cells |
|---|---|
| `minecraft:sculk` | **265** |
| `minecraft:sculk_vein` | **131** |
| `minecraft:sculk_catalyst` | **1** |
| 合計 | **397** |

**port は現在 0**（feature 未実装）。したがって `tools/sculk_gate.py` は
`--require-match` で exit 1 になり、これが実装の合格条件そのものになる。
sculk が 1 セルも無い capture は exit 2（「何も証明しない」）で、緑にならない。

同じ 4 チャンクの全セル比較（`tools/carver_diff.py`）では 1732 セルが違うが、その内訳は
別の層の既知の差（leaf distance ≈1000、deepslate/stone の物性 ≈350、carver の空気 497）で、
sculk ファミリに絞ることで主張が具体的になる。**領域一致は主張しない。**

### 実装に必要な残りの意味論（decompile 済み・次のセッション用）

- spreader は `dmu.b()`: chargeCount 50 / amountPerCharge 1 / chargeDecayRate 5 /
  noGrowthRadius 10、`isWorldGeneration = true`（**merge しない**、同一位置のカーソルは
  重複したまま残る）。
- `ChargeCursor.update` の順序と `closerThan(pos, 15.0)` は `distSqr < 225`（等号は
  範囲外＝charge 0）。`facings` の更新は `next != null` の外側でも走る。
- `SculkBlock`: `nextInt(5) == 0` の門、`getSculkState` は `nextInt(11) == 0` で
  shrieker（`CAN_SUMMON = worldGen`）、waterlogged は
  `!getFluidState(pos).isEmpty()`（水以外の流体でも立つ）。
- `SculkVeinBlock.attemptSpreadVein`: waterlogged は
  `!state.getFluidState().isEmpty()`（**空でないとき** true。逆に読むと全滅する）。
- vein の spread は `MultifaceSpreader`（既存の `multiface_spread` がそのまま使える）で、
  `spreadAll` は `from` 6 方向（enum 順・シャッフル無し）ごとに、`to` 6 方向を
  `Direction.allShuffled` でシャッフルして 1 セルまで置く。
- vein の `canSpread` 連鎖は `dkz$b` / `dkz$a` / `SculkVeinSpreaderConfig` の 3 段で、
  未確定の細部が残る: `SCULK/SCULK_CATALYST/SCULK_SENSOR` の拒否、
  `target.distManhattan(pos) == 2` のときの `isFaceSturdy` 側面判定、
  `FluidTags.WATER` 以外の流体の拒否、`FIRE` タグの拒否。
- **未確定として残した最大の点**: `MultifaceSpreader.spreadAll(state, level, pos, spreadOnly)` の
  配管。decompile (`dkz.java`) には (a) `from` ごとに `config.isOtherBlockValidAsSource(state,
  from)` で絞る、(b) `to` を乱数でシャッフルする、(c) `from` は `Direction.values()` の順
  （シャッフルしない）という3点が見えるが、どの overload がどの乱数を受け取るかが
  Vineflower の総称の混線で確定できなかった。既存の `multiface_spread`（`multiface_growth` で
  検証済み）は 6-arg の `spread(state, level, pos, from, spreadOnly, random)` に相当するので、
  vein では「`from` を 0..6 で回して、各 `from` で `multiface_spread` 相当を1回呼ぶ」形が
  候補だが、**`isOtherBlockValidAsSource` の条件を確定してから**書く。ここを推測で埋めると
  397 セルの draw 順がずれたまま「近い」結果になり、原因の切り分けに時間を溶かす。
- `isFaceSturdy` / `canAttachTo` は asset の側面フラグ（`is_solid && side_solid(face)`）で
  近似する。deep dark の対象セルは full cube / air / water / sculk 系だけなので、
  この範囲では結果が一致することを capture で確認する（範囲外では主張しない）。

## 測った障害：feature 単体の oracle は使えない（2026-10-27）

`FeatureOracle` で native を駆動しようとすると、記録済みの `place()` までは進むが
最初の `updateCursors` で落ちる:

```
java.lang.UnsupportedOperationException: public abstract duy dcz.a(int,int,dvz,boolean)
```

`LevelReader.getChunk(int,int,ChunkStatus,boolean)` である。呼び出し元は vein 側の
`Block.updateFromNeighbourShapes`（`SculkVeinBlock` の private `a(dmu,dcx,jd,ayw)` が
sculk を置いた直後に呼ぶ）で、block の形状更新が chunk を要求する。level proxy で
返せるのは `ChunkAccess` の実体だけで、それは「level を半端にモデル化しない」という
harness の原則（`native-oracle.md`）に反する。よって whole-chunk 経路を採る。

## 実装状況（2026-10-28）: port は入ったが **gate は赤**

`SculkPatchFeature` / `SculkSpreader` / `SculkBlock` / vein 経路を port した
(commit `8272b9e`)。`MultifaceSpreader.spreadAll` は **乱数を一切引かない**ことが
bytecode で確定した（`dkz.a(dtc,dcx,jd,boolean)` が `Direction.stream()` を2回 map して
配置数を数えるだけ。シャッフルは別 overload `spread(..., from, random, ...)` のもので、
sculk 経路はそれを呼ばない）。これで vein の順序は厳密になった。

**この作業で見つかって直した port の欠陥 2 件:**

1. `Gen::chunk_biome_set` の y 範囲（`fix(worldgen): the chunk biome set swept the wrong
   y range`）: y<0 だけに存在する biome が chunk の biome 集合から落ちていた。
   深層暗黒チャンクに `deep_dark` が無いため `sculk_patch_deep_dark` と `sculk_vein` が
   一度も走らず、参照 397 セルに対して port は **0** だった。
2. `closerThan(new Vec3i(origin.x, pos.y, origin.z), 15.0)` の y 項は自分自身なので相殺する
   （水平距離だけが bound）。y も比較していた最初の版は、層を跨いだ cursor を全部殺していた。

**現在の測定**（seed 0、chunks (-16,112)-(-15,113)）:

| | sculk | sculk_vein | sculk_catalyst | 合計 |
|---|---|---|---|---|
| 参照 | 265 | 131 | 1 | **397** |
| port | 337 | 92 | 0 | **429** |

`missing 209 / extra 241 / wrong-state 0`、`tools/sculk_gate.py --require-match` は **exit 1**。
チャンク別の共有セル数: (-15,112) 67/68、(-15,113) 59/93、(-16,112) 13/17、(-16,113) 15/87。
2 チャンクはほぼ一致し、機構は近いが配置が食い違う。

**次の一手（診断の順序）:**

1. step 7 は `sculk_vein`（multiface_growth, count 204..250）→ `sculk_patch_deep_dark` の順で、
   **同じ RNG stream を共有**する。どちらかの draw 数がずれれば後続が全部ずれるので、まず
   `sculk_vein` 単独の照合を行う（`multiface_1211.tsv` の fixture は既にあるが、この
   biome/座標での実行は未照合）。
2. 参照 datapack の `sculk_patch_deep_dark` の `count` を 1 にした capture を作れば、
   1 chunk 1 patch に絞った厳密な比較ができる（参照再生成 3 分 + port dump 1 秒）。
3. `biome` filter は port が blended（`Level.getBiome` 相当）を使う。vanilla の
   `BiomeFilter` が `WorldGenRegion.getBiome`（= `BiomeManager` の fiddled 探索）である
   ことは確認済みだが、候補セル単位での一致は未検証（候補の y は -64..256 に散る）。

## 未検証として残ること

- port は入ったが **未検証**（missing 209 / extra 241）。この文書は調査記録と比較経路・
  合格条件の記録であり、parity の主張ではない。
- 再開時は上の「実装に必要な残りの意味論」から始め、`tools/sculk_gate.py --require-match`
  が exit 0 になるまでを合格条件にする。
