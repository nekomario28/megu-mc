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

## 分離診断の結果（2026-10-28、`tools/vanilla_sculk_pack.py` + fixture の `sculk_veins` / `sculk_one_patch` モード）

step 7 は `sculk_vein` → `sculk_patch_deep_dark` の順で同じ decorator stream を共有するので、
片方だけに絞った参照を作れるようにした（両側に同じ pack を入れる必要がある。port 側は
`datapacks/vanilla/...` を一時的に差し替えて復元する）。

| 診断 | 結果 |
|---|---|
| **1. `sculk_vein` 単独**（patch を biome の step 7 から外す） | 参照 8 セル / port 8 セル、**missing 0 extra 0 → PASS**。vein feature は完全一致 = patch に入る時点で stream は正しい |
| **2. `count: 1` の patch 単独** | 参照に `sculk` が 1 セルも出ない（唯一の候補が石の中に落ち、`canSpreadFrom` が拒否）。**診断として使えない** → 次は `height_range` を深層暗黒の空洞帯（例: absolute -64..-30）に絞った variant が要る |
| **3. biome filter を container 読みに変更** | 悪化（port 444 セル / extra 252、現行の blended は 429 / 241）。blended のまま据え置き |

**セル単位の内訳**（現行 port vs 参照、4 chunks）:

| 方向 | 内訳 |
|---|---|
| missing 209 | `sculk`→deepslate 105、`sculk_vein`→air 97、`sculk`→tuff 5、他 2 |
| extra 241 | `sculk`→deepslate 147、`sculk_vein`→air 58、`sculk`→tuff 35、他 1 |

つまり port は **sculk を多く育て、vein を少なく残す**（sculk:vein 比 3.7 対 参照 2.0）。
どちらも patch 由来（vein feature は 8 セルだけ）なので、次の容疑は
`spc_vein_convert`（vein→sculk 変換）の通り過ぎ、または vein の spread/decay の弱さ。

**bytecode 照合で直したが metric が変わらなかったもの**（CV1 の教訓として記録）:
`SculkBlock.canPlaceGrowth` は `pos.above()` を見る（`jd.d()` = above、成長も `pos.above()` に書く）
のに port は below を見ていた。修正しても 429/209/241 は**ビット同一**で、
この fixture の cursor は上下とも空気のセルに居るため差が出ない。修正は残す（bytecode 準拠）。

## 分離診断 ① 洞穴帯 one-patch（2026-10-28）

`--count` / `--height` を `vanilla_sculk_pack.py` に足し、fixture に `sculk_cave_patch`
モードを追加（既定 `SCULK_COUNT=4 SCULK_HEIGHT=-64:-30`）。`height_range` を洞穴帯に絞らないと
唯一の候補が石に落ちて patch が 1 度も走らない（それが plain `one-patch` の失敗理由）。

参照（seed 0、chunks (-16,112)-(-15,113)、count 4、height -64..-30）: **62 セル**
（sculk 23 + sculk_vein 39）。port: **32 セル**（sculk 13 + sculk_vein 19）、
missing 43 / extra 13。セル単位で見ると:

- chunk (-16,113): vanilla 26 / port 25 が**同じ場所**（x -245..-243, z 1820..1823）にあり、
  数セルが ±1 ずれる（port が西に 1 セル余分に伸びる）。
- chunk (-15,113): vanilla 35 は x=-240/-239 の縦の列、port は 6 セルで x=-227/-226 の別位置。
  → 最初のパッチが数セルずれた時点で **draw 数が変わり、以降の候補位置がずれる**
  （cell の違いが分岐条件を変え、`nextInt(50)`/`nextInt(11)`/`nextInt(10)` の消費が変わる）。

## 分離診断 ② 変換チェーンの質量（2026-10-28、env で 1 条件ずつ切って測った）

| 条件 | port のセル | 結論 |
|---|---|---|
| 基準（full capture） | sculk 337 + vein 92 = **429** | 参照は 397 |
| `canPlaceGrowth` を常に false（growth 経路を無効化） | **ビット同一 429** | **growth 経路はこの fixture で 1 セルも寄与していない**。337 sculk は全部 `spc_vein_convert` 由来 |
| 変換後の vein spread（`this.e.a(sculkState, ...)`）を無効化 | **16**（sculk 6 + vein 8 + catalyst 2） | この呼び出しがパッチ全体を支えている。無効化で 429 → 16 に崩壊 = 変換→vein spread→新しい vein→変換の連鎖が出力のほぼ全部 |

したがって**探索範囲は変換チェーンに絞られた**（growth 経路と `findNextSculk` の骨格は
質量を持たない）。次にやることは 1 つ: 変換の「最初のずれ」を特定する。port 側の書き込み順を
trace して（`ctx.set` を chunk で絞って順に出す）、参照セル集合と突き合わせ、最初に集合から
外れる書き込みの近傍を見る。そこが最初の分岐で、以降は stream のずれとして増幅される。

## 書き込み順 trace で最初のずれを特定した（2026-10-28）

`FC::set` に `FEATURE_WRITE_TRACE=<cx>,<cz>` を追加（その chunk の feature 書き込みを
**順に**出す）。digest は「どこが違うか」しか言わないが、**順序**は「最初の違い」を出す。

さらに `spc_set` ラッパで sculk 系の書き込みに**呼び出し元のタグ**を付けた
（`convert` / `spread-post-convert` / `spread-cursor-1type` / `spread-cursor-3type` /
`attempt` / `decay`）。`SPC_TRACE_CELL=x,y,z` で 1 セル、`SPC_TRACE_ALL=1` で全件。

洞穴帯 one-patch 参照の chunk (-16,113) で測った結果:

- 参照と port の**最初のずれ**は sculk でも vein の新規配置でもなく、
  **port が vanilla の残す vein を air に decay した**こと
  （`-242,-44,1823`: vanilla の最終状態は `sculk_vein`、port は `air`）。port 自身の順序では
  そのセルに vein を書いた直後に decay で消している。
- port 順序での**最初の「余分」な書き込み**は `-246,-42,1823` の vein で、
  呼び出し元タグは **`spread-post-convert`**（変換後の vein spread）。

したがって最初の分岐は「変換 → 変換後の vein spread → decay」の3つの並びの中にあり、
global な症状（port は sculk 過多・vein 過少）とも一致する: 余分な sculk が周囲の vein の
面を落とさせ（decay は隣が sculk の面を落とし、面が尽きた vein は air/water になる）、
結果として vein が減る。

次の一手は 1 つ: `spread-post-convert`（`this.e.a(sculkState, level, newPos, worldGen)`）の
述語を、その最初の余分セル 1 つに絞って vanilla と突き合わせる。`SPC_TRACE_CELL` があるので
「そのセルに書いた呼び出し元」は 1 秒で出る。

## 述語レベルの trace と、**捕捉領域のマージン**という方法論の修正（2026-10-28）

`spc_spread` に述語トレース（`SPC_TRACE_CELL` のセルに達した候補について
`target/pos/from/to/type/face/state/passes/tag` を出す）を追加。特定した余分セル
`-246,-42,1823` に効いた書き込みは:

```
target=(-246,-42,1823) pos=(-246,-43,1824) from=2(north) to=1(up) type=2(WRAP_AROUND)
face=0(down) state=air passes=true tag=spread-post-convert
```

つまり `pos`（変換で sculk になったセル）が **chunk 114** にあり、そこから chunk 113 に
vein を伸ばしていた。**前回の 2x2 捕捉は小さすぎた**: パッチの起点が隣 chunk にあると、
こちらの chunk に書き込む一方でその起点の参照が捕捉領域の外に落ちる。R1 の規律
（source footprint は入力）そのもの。

そこで **1 chunk のマージンを持つ 4x4 捕捉**を取った（chunks (-18,110)+4x4, mode features）:

| | sculk | sculk_vein | catalyst | shrieker | 合計 |
|---|---|---|---|---|---|
| 参照 | 3881 | 1188 | 9 | 4 | **5082** |
| port | 4001 | 822 | 11 | 0 | **4834** |

missing 2174 / extra 1926 / wrong-state 9。内訳を測ると**系統的なオフセットは無い**:
port の sculk 4001 のうち **2666 (67%) が参照と完全一致**、vein は 822 中 233 が一致、
残りは 1-2 セル散らばるか近傍に無い。→ 規則の系統誤りではなく、**列（sequence）が
途中で分岐して増幅**している形。

内部 chunk (-17,111) の書き込み順で最初に参照集合から外れるのは
`-272,-38,1776` の vein（参照は `-272,-39,1776` に vein = **1 セル上**）で、やはり
`spread-post-convert` 由来だった。

**次の一手**: 分岐点は「変換 → 変換後 spread」の中の 1 述語に絞られている。
`SPC_TRACE_CELL` と `SPCPRED` があるので、分岐セル 1 つについて vanilla 側の同じ瞬間の
状態（= 参照の最終状態から復元できる範囲）と突き合わせ、`passes=true` になった
述語のどれが vanilla と食い違うかを 1 つずつ反転して metric の向きを見る。

## 述語の1つずつ反転（ablation）— 3つとも load-bearing、どれも「緩すぎ」ではない（2026-10-28）

4x4 捕捉（参照 5082 = sculk 3881 + vein 1188 + catalyst 9 + shrieker 4、baseline port 4834 =
sculk 4001 + vein 822 + catalyst 11、missing 2174 / extra 1926）に対して、
`spc_vein_can_spread` / `spc_multiface_can_place` の3つの候補を1つずつ無効化して測った:

| 無効化した条件 | port のセル | sculk / vein | 解釈 |
|---|---|---|---|
| （なし = baseline） | 4834 | 4001 / 822 | — |
| `manhattan == 2` の `isFaceSturdy` 拒否 | **61152** | 2516 / **58585** | vein が 71 倍に爆発。この拒否が spread を局所に留めている = 必要不可欠 |
| `state.canBeReplaced()`（`is_replaceable`）の受理 | 4526 | 3800 / 711 | sculk −201 / vein −111。参照からは**遠ざかる**（5082 から 4526 へ）= この受理は寄与していて過剰ではない |
| `MultifaceBlock.canAttachTo`（`supports_multiface`） | **497** | 395 / 94 | ほぼ全滅。`supports_multiface` が spread 試行の約 95% を弾いている = 必要不可欠 |

**結論**: 3つとも load-bearing で、無効化すると爆発するか崩壊するか、参照から遠ざかる。
**どれも「緩すぎる」述語ではない**ので、分岐はこの3つ以外（カーソルの `facings`/`charge`/
`decay` の勘定、または spread を呼ぶ時点・回数）にある。

ablation のトグルは測定後に撤去し、4x4 dump が撤去前と**バイト同一**であることを確認した
（= 測定は読み取り専用で、ツリーに痕跡を残していない）。

## カーソル側の計測（2026-10-28 後半、いずれも 2x2/4x4 の実測）

ablation で 3つの spread 述語が load-bearing と分かった後、質量の出所と呼び出し回数を数えた。

**呼び出し回数（2x2 の 3x3 近傍、`SPC_CALLS`）**: `spread-post-convert` 2750、
`spread-cursor-1type` 1972、`spread-cursor-3type` **0**。書き込みタグ別では
`decay` 5076 / `spread-post-convert` 4595 / `convert` 2750 / `attempt` 574 /
`spread-cursor-1type` 22。→ vein の生成はほぼ**変換後の spread** が担い、カーソル自身の
spread（`m()` = SAME_POSITION のみ）は 22 セルしか寄与しない。

**カーソルの状態分布（`SPCATT`）**: `sculk`/`sculk_vein` 上の滞在が大半
（sculk 564、vein 各種 900+）、`air` 上は `facings=None` 72 / `facings=Some` 150 程度。
「カーソルが空気セルに居るときだけ自セルに vein を置ける」経路は確かに呼ばれている。

**変換の入力（`SPC_TRACE_CONVERT`）**: `deepslate` 1612 / `tuff` 12 / `gravel` 10 —
**air も vein もタグ外も 1 つも無い** = 変換の入力集合は正しい。

**vein の面構成**: 最終 vein セルの state id 分布を両側で突き合わせると形は同じ
（6面 ≈74% / 5面 25%）。違うのは**本数だけ**（参照 1188 に対し port 822、−31%）。

**sculk の内訳**: port 4001 のうち 2666（67%）が参照と完全一致。

→ まとめると: 規則は個別には正しく（変換入力・面構成・述語の load-bearing 性）、
**量が 25% 前後ずれる**（変換が多く、vein が少ない）。これは「パッチの起動回数」か
「1 カーソルの寿命（更新回数）」のどちらかが vanilla と違う形。次はそこを数える
（`canSpreadFrom` を通過した候補数と、1 パッチあたりの `spreadOneCharge` 呼び出し回数を
`SPC_CALLS` 方式で両側から測る。vanilla 側は参照の sculk セル数から逆算する）。

## 「最初のずれ」の正しい測り方と、その結果（2026-10-28 深夜）

**方法の修正**: 書き込み順で「最初のずれ」を探すとき、書き込みの state を参照の
**最終** state と比べると誤検出する（同じセルが最後に air に decay される場合、
途中の vein 書き込みが「余分」に見える）。正しくは

1. port の**最終**セルと参照の**最終**セルを比べて、**食い違うセル集合 D** を作り、
2. 書き込み順を歩いて、**D のセルに最初に触れた書き込み**を探す。

書き込みに `pop=cx,cz`（どの chunk の population が書いたか）を足した。
1 回の population は自身の chunk に書くので、`pop` で「1 パス分」に切って比較できる。

**結果（6x6 捕捉 chunks (-20,108)+6x6、参照 5987 セル / port 5739、D = 5037 セル）**:
`pop=(-19,110)` のパスで、D に属するセルに最初に触れた書き込みは

```
FWRITE -291 -40 1777 state=<vein> feature=1,7 pop=-19,110
```

で、呼び出し元は `spread-post-convert`（変換後の vein spread）。そのセルの port 側の履歴は
`spread-post-convert` 3 回（面を足す）→ `decay` 2 回（面を落とすが生存）= 最終 **vein**、
参照の最終は **air**。つまり **vanilla は面が尽きて air に decay したが、port は面が残った**。

decay は「隣が sculk の面を落とす」ので、面が残るということは **その周囲の sculk が
vanilla と違う**（変換の位置がずれている）ことを意味する。実際 4x4 で測ったとおり
sculk は 67% しか一致しない。

**結論**: ずれは「1 回の spread 呼び出しの中」で決まっており、その判断は**その瞬間の
セル状態**に依存する。参照の最終セルから中間状態を復元する方法はここで尽きた。

## native 実行の再挑戦と、2 つ目の障害（2026-10-28、実測）

option 2（harness に `ChunkAccess` を足す）を実際に試した。`NativeHarness` は
`getChunk(int,int,ChunkStatus,boolean)` に対して**実物の `ProtoChunk`** を返せるように
なった（`UpgradeData.EMPTY` + `LevelHeightAccessor.create` + 生成器と同じデータから構築。
サブクラス化は不可: game のクラスは default package にあり、そこに足したクラスは
signer 検査で落ちる）。これで最初の `UnsupportedOperationException`（`getChunk`）は解消。

しかし次の障害が出た:

```
java.lang.IllegalStateException: Missing key in ResourceKey[minecraft:root / minecraft:block]:
  ResourceKey[minecraft:worldgen/biome / minecraft:plains]
```

空の chunk が biome 問い合わせに**固定の `minecraft:plains`** で答え、それを chunk 構築時に
渡した `Registry` から引こうとする。1.21 の biome は**動的 registry**で、
`BuiltInRegistries` のどのフィールドにも無い（`lt` の 79 registry を列挙して確認）。
つまり biome に答えられる chunk は**実 world の `RegistryAccess`** を必要とする。

→ **sculk を native で駆動するには running server context が要る**（harness の拡張では
届かない）。実装した `ProtoChunk` 返却は harness に残すが（`getChunk` を呼ぶ他の feature の
ため）、sculk の合格判定は当面 whole-chunk 比較（`tools/sculk_gate.py`）に一本化する。

現状の到達点: 規則は個別に正しく（述語 3 つは load-bearing、変換入力は正しい、vein の
面構成も同じ）、量が ~25% ずれる。決定は 1 回の spread 呼び出しの中で、その瞬間の
セル状態に依存する — これは最終セルからは復元できない。

## パッチ数を絞った比較（count=1、マージン付き 4x4）— 2026-10-28

洞穴帯に絞った `count=1`（1 chunk 1 patch）で 4x4（マージン1）を取ると:

| | sculk | sculk_vein | catalyst | 合計 |
|---|---|---|---|---|
| 参照 | 386 | 209 | 0 | **595** |
| port | 416 | 203 | 2 | **621** |

missing 275 / extra 301。**総量は +4%、vein は −3%** で、count=256 のときの −31% より
はるかに近い。つまり**パッチ数が増えるほど deficit が積み上がる**（1 パッチあたりの
差が、候補数に比例して増幅する）形。

**最初に D（最終状態が食い違うセル）に触れた書き込みを実行順で見ると**、両方向が出る:

- `feature=0,7`（`sculk_vein` feature）が置いた vein を port は後で air に decay し、
  vanilla は保持（`-282,-29,1770`）→ **port の方が decay が進む**場合
- `feature=1,6`（ore の step）が書いた gravel が port の最終状態で残り、vanilla は
  sculk（`-267,-36,1769` ほか）→ **port のパッチが vanilla の覆ったセルを覆えていない**

どちらも「パッチのカーソルがどこを回ったか」の差で、規則の差ではない（述語 3 つは
load-bearing、変換入力は正しい、vein の面構成も同じ、をすべて実測済み）。

## 次の道: running-server context で native の中間状態を観測する

harness は 2 段目の障害（動的 biome registry）で閉じている。中間状態（各 update の
cursor pos / charge / facings）を native から取る唯一の現実的な道は、
**参照サーバーに開発用の probe mod を載せて `SculkSpreader$ChargeCursor.update` を
ログする**こと。このリポジトリには既に `--features fabric-probe` と
`specs/mod-compatibility/` の足場があり、参照は固定 jar なので mod 側の版も固定できる。
port 側は `SPC_CALLS` / `SPC_TRACE_CELL` で同じ形（pos/charge/facings/判断）を既に出せる
ので、**両側のトレースを index で突き合わせる**のが次の比較単位になる。

これは「推論ではなく証拠で閉じる」唯一の残った道で、作業量は probe mod 1 本。

## native の**中間状態**を観測した（2026-10-28、`tools/carve-agent` の sculk hook）

harness は 2 段目で閉じていたが、**リポジトリに既にあった javaagent**（`tools/carve-agent/`、
ASM、bootstrap の `CarveLog`）を使えば参照サーバーの内側を直接見られる。追加したのは:

- `SculkLog`（bootstrap、primitive + JDK 型のみ）: `SCURSOR r=<round> i=<index> x y z
  charge decayDelay updateDelay facingsMask`。`r` は `SculkSpreader.updateCursors` の呼び出し
  回数、`i` はそのラウンド内の cursor 添字。**charge 0 の cursor はリストから消える**ので、
  この 2 つが無いと死んだ cursor 以降を整列できない。mask は Direction の宣言順
  （down/up/north/south/west/east）で、port 側の面マスクと同じ添字。`-1` は null
  （SAME_POSITION のみの spreader）、`0` は空集合（全 type の spreader）。
- `Agent.java` の `SculkTransformer`: `dmu$a.a(dcx,jd,ayw,dmu,boolean)` の入口で
  `this.d`（jd.u()/v()/w()）・`e`（charge）・`g`・`f`・`h` をそのまま渡す。**乱数は引かない**
  ので spread の判断を変えない。
- `tools/chunk_world_fixture.sh` に `JAVA_AGENT=<carveagent.jar>`（features モードのまま）。

実行（seed 0、chunk (-16,112)、参照キャプチャ約3分）:

```sh
JAVA_AGENT=/var/tmp/sa/carveagent.jar tools/chunk_world_fixture.sh sa2 -16 112 1 features
grep -o 'SCURSOR .*' /tmp/chunkfix-sa2/run.log | head
# 28175 行。先頭は r=0 i=0..9 = 最初のパッチの 10 cursor の 1 回目の更新（すべて一致）
```

port 側も同じ行を `SPC_CURSOR_TRACE=1` で出すようにし、**(round, index) で整列**して比較する
と、最初の食い違いは **r=1 i=0**:

```
native (-261,-43,1784, charge 31, decay 1, update 1, facings 20)
port   (-260,-42,1785, charge 31, decay 1, update 1, facings  4)
```

つまり round 0（最初の更新）を終えた時点で、両者の cursor は**別のセルへ動き**、面構成も違う。
charge は両方 31（変換が起きた ✓）。`r=1 i=2` は完全一致、共有キー 4684 中 4654 が食い違い。

### この比較で見つけた bytecode 由来のバグ 2 件（修正済み）

1. **`decayDelay`（`j_`）の向きが逆だった**: `SculkBlock` と `SculkVeinBlock` は interface 既定
   （`1`）を継承し、`max(x-1,0)` を返すのは **`dmp` の匿名既定実装だけ**。port は vein 側に
   その実装を置いていた（＝vein に乗った cursor が毎回 decay する）→ 修正。トレースで
   `r=1 i=2` が一致するようになった（全体セル数は不変: 4x4 4834 / 6x6 5739 のまま）。
2. **`SculkVeinSpreaderConfig.canSpread` の隣接拒否リスト**: `dga.bQ` は `sculk_sensor` ではなく
   **`moving_piston`**（mappings で確認）。port は `sculk_sensor` を拒否していた → 修正。

どちらも**全体セル数は変えない**（メトリクスが鈍い）が、**トレースでは観測できる** —
これが「最終セルでは復元できない中間状態を見る」ための新しい計器の価値。

### 次の一手（この計器で）

最初の食い違い（r=1 i=0）は「round 0 の後の cursor 位置と面構成」。両者の round 0 は
同じ 22 draw（convert の 5 + findNextSculk の 17）を消費しているはずで、charge も一致する
ので、**候補セル集合**（＝round 0 に書いた vein/sculk のセル）が違う。次は
`setBlock`（`dds.a(jd,dtc,int)`）を同じ agent で hook して `SWRITE x y z stateId` を出し、
**書き込み列そのもの**を整列比較する（carver agent の `ASTATE` が block state id を
registry 経由で取る作法をそのまま使える）。

## 書き込み列の整列比較（2026-10-28、agent の featurebox + port の SPCSET）

`tools/carve-agent` は既に `-Dfeaturebox=minX,minY,minZ,maxX,maxY,maxZ` で **feature 層の
書き込みを state id 付きで**出す（`FCELL x y z state=<id> wrote=<bool> feature=<gi>,<step>
chunk=<pop>`, id は `dfy.i(BlockState)` = jar の registry id ✓ port の asset と同じ番号）。
`chunk_world_fixture.sh` に `JAVA_OPTS=` を足して agent の property を渡せるようにし、
port 側は同じ窓（`SCURSOR r=0 i=0` 〜 `i=9`）の `SPCSET` を並べれば**書き込み列そのもの**を
整列比較できる。

**結果（seed 0、chunk (-16,112)、最初のパッチの round 0）**: native 19 書き込み / port 18。
**最初の 10 書き込みが完全一致**（state id 込み）:

```
origin vein 22911 / 22907, north neighbour sculk 22799, (down,north) vein 22911 / 22910,
west vein 22911, east+down vein 22923, east vein 22911, (down,north) vein 22910, origin vein 22923
```

つまり **cursor i=0 の 1 回目の更新は「自分のセルに vein → 変換 → 変換後の spread → decay」まで
native と完全一致** ✓。最初の食い違いは i=1 の 1 回目:

| | native | port |
|---|---|---|
| i=1 の最初の書き込み | vein @(-261,-41,1786) | sculk @(-261,-41,1785)（変換） |
| その位置 | `SCMARK one-charge-vein` より前 | `convert` の直後 |

native は `attemptSpreadVein`（`SCMARK attempt-vein` は出ない = facings==null 経路）で
**隣接セルに vein を 3 つ書いてから**変換している。

### 未解決の矛盾（次の1点）

`dmp$1.a`（facings==null）は `((dmv)Blocks.SCULK_VEIN).m().a(state, level, pos, wg)` を呼び、
`dmv.m()` は **1 型** spreader（bytecode 確認: `m()` は `f` を返し、`f` は
`new dmv.a(dkz$e.a)` = 1 要素。`dkz$e$1.a` = `new SpreadPos(pos, to)` = SAME_POSITION、
`$2` = SAME_PLANE、`$3` = WRAP_AROUND）。1 型なら書き込み先は **cursor 自身のセルだけ**なのに、
native は隣接セルに書いている。→ この経路で**3 型の spreader が使われている**としか読めない。

次の計測は 1 つ: `dkz.a(dtc,dcx,jd,boolean)`（spreadAll）入口で `this.b.a().length`
（config が持つ spread type の数）を出す。反射で config の配列長を読む実装は書いたが
`types=-2`（ReflectiveOperationException）で失敗したので、`dmv$a` の `a()` を直接
`getDeclaredMethod` で引く形に直す。これで「1 型か 3 型か」が 1 行で確定する。

## 2件の根本原因を修正（2026-10-28、いずれも native 計測が起点）

### 1. `attemptSpreadVein` の spread type は**振る舞いごとに違う**

native の `spreadAll` 入口で config の spread type 数を出したら
**`SSPREADALL types=3`**（1 ではなく 3）。bytecode で裏づけ:

- `dmp` の **interface 既定** `a(dcx,jd,dtc,Collection,boolean)` は
  `((dky)Blocks.SCULK_VEIN).c().a(state, level, pos, wg)` = **`c()` = 3 型**（`facings` を無視）
- `SculkBlock` (`dmq`) も `SculkVeinBlock` (`dmv`) も**この 5 引数を override していない**
  （`dmv` にあるのは *static* 4 引数のほう）→ **vein/sculk 上の cursor は常に 3 型**
- `dmp$1`（その他のブロック）だけが `facings == null` で `m()` = 1 型

port は常に `dmp$1` の規則を使っていた ✗ → **修正**。

### 2. `findNextSculk` の 18 近傍リストの**順序**が逆だった

`BlockPos.betweenClosed(-1,-1,-1, 1,1,1)` の反復順を固定 jar で実測すると
`(-1,-1,-1) (0,-1,-1) (1,-1,-1) (-1,0,-1) ...` = **x が最速**、次に y、最後に z。
port は x を最外にしていた ✗ → 同じ乱数 stream でも `util_shuffle` の順列が変わり、
`findNextSculk` の選択が変わる（＝**規則ではなく順序**が最初の分岐点だった）。

### 計測した効果（cursor trace、同じ native trace と整列）

| | 最初の食い違い | 共有キーの食い違い |
|---|---|---|
| 修正前 | r=1 i=0 | 4654 |
| 修正後 | **r=3 i=0** | **4527** |

つまり最初のパッチの **round 0〜2 が完全一致**（round 3 で分岐）。

### 注意: 全域セル数の before/after は**この区画では主張できない**

同じ 6x6 recipe（seed 0、(-20,108)+6x6、features）で参照を取り直すと
**参照自体が 5933→5733 と動いた**（前回 5987 / 今回 5733）。同一 recipe の参照が
254 セル動く以上、capture をまたいだ比較は無効。このページの before/after はすべて
**同一 capture 内**（同じ `/tmp/chunkfix-*` の参照）で測っている。原因（生成順の
非決定性か、pack の差か）は**未解明のまま記録**する。トレース比較は同じ capture の
native trace に対して行っており、そちらは再現する範囲で有効。

## 未検証として残ること

- port は入ったが **未検証**（missing 209 / extra 241）。この文書は調査記録と比較経路・
  合格条件の記録であり、parity の主張ではない。
- 再開時は上の「実装に必要な残りの意味論」から始め、`tools/sculk_gate.py --require-match`
  が exit 0 になるまでを合格条件にする。
