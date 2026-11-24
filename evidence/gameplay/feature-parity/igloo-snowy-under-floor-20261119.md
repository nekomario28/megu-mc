# `igloo` `(37,−512)` seed 4242: `grass_block[snowy]` 8 セル(構造物passの `updateShape`)

Status: **HOLD(受入しない)。原因は記述子単位で特定済み: port の
`src/template_structures.rs::update_shape` に `SnowyDirtBlock.updateShape` の腕が無い。
`src/` は無変更(このレーンは調査と記録のみ)。**

## 0. Provenance

| | |
|---|---|
| base commit | `0fa6807`(`perf/worldgen-parallel`) |
| port binary | `target/debug/mc-spine` sha256 `7dc623bf3372e5e8d71ad6cab6b65196f2a3ba7f125862234b4371524c4ef7ca` |
| capture | `<local>/captures/matrix-v3/nat-4242-igloo`(frozen, overworld/noise, −64/384, seed 4242) |
| port dump | `dumps/nat-4242-igloo/dump.bin` sha256 `e23b53520eb5529e…` |
| heightmap 対照 | `dumps/nat-4242-igloo-hm/`(`--port-hm --hm-name MOTION_BLOCKING`) |
| instrumented recapture | `captures/scratch-p21t3/igloo-featurebox`(`-Dfeaturebox=588,78,-8192,600,90,-8178`)、`…/igloo-stack`(同 + `-Dfeaturestack=1`) |
| start | `minecraft:igloo` `iglu#0` BB `[592,81,−8189,598,85,−8182]` |

## 1. 残差(8 セル、状態は1種類)

```
diff summary: all_air_kinds=8 / 98,304      native non_air = port non_air = 31,721
(594,80,−8189) (594,80,−8188) (594,80,−8187) (595,80,−8186)
(595,80,−8185) (595,80,−8184) (595,80,−8183) (596,80,−8182)
  native: minecraft:grass_block[snowy=true]
  port:   minecraft:grass_block[snowy=false]
```

他の全セルは一致。8 セルはすべて `iglu#0` BB の **1つ下** (y=80) で、直上 y=81 は両側とも
`minecraft:snow_block`(igloo の template 最下層)。8 セルは BB 内で `y=80` が
`grass_block` の列と正確に一致する(`y=80` が `dirt` の列は差にならない)。

## 2. heightmap(差ではない)

```
MOTION_BLOCKING: port vs native saved = 0 columns differ   (両側 78..90)
```

8 列の保存 `MOTION_BLOCKING` は 84..86(`snow[layers=1]` は motion-blocking に数えられない)、
`WORLD_SURFACE` は 85..87。**port の heightmap は native と列単位で一致している**ので、
この残差は heightmap の不一致でも、`MotionBlocking` の読み方の差でもない。

## 3. `run.log` の writer triage(2 capture)

`tools/carve-agent`(`-Dfeaturebox` で `WorldGenRegion.setBlock` を計装)による実測。

1. **attribution:構造物passであって freeze ではない。** chunk `37,-512` の `state=8` 書き込みは
   80 行あり、そのうち **72 行が `feature=0,10`**(step 10 = `top_layer_modification`、
   index 0 = `minecraft:freeze_top_layer`)、**8 行が `feature=3,4`**(igloo の template セル
   152 行と同じ pass)だった。差になったのはその 8 行だけ。
2. **算術でも freeze は排除できる。** `SnowAndFreezeFeature`(`edn`)の bytecode は
   `dyy$a.e` = `Heightmap.Types.MOTION_BLOCKING` を読み(`getHeight`)、
   `pos4 = (x, height, z)`(最初の air)、`pos5 = pos4.relative(DOWN,1)`(最上の
   motion-blocking)として `shouldFreeze(pos5, false)` → ice、`shouldSnow(pos4)` →
   `setBlock(pos4, SNOW)` と `pos5` の `snowy=true` を行う。この8列では
   `height` が 84..86 なので `pos4` は `snow[layers=1]`、`pos5` は `snow_block` で、
   y=80 はどちらの位置にもならない。
3. **`-Dfeaturestack=1` が caller を名指しした**(このために `CarveLog.featureCell` へ
   追加した opt-in。既定 off なので既存 capture の行 byte は不変):

   ```
   # template セル (594,81,-8189) state=5781 snow_block
   arb.a(304) | dde.a(15) | ent.a(246) | ekb.a(83) | elq$a.a(114) | ejz.a(100) | ...
   # 残差セル (594,80,-8189) state=8 grass_block[snowy=true]
   arb.a(304) | dde.a(15) | ent.a(372) | exl.a(225) | exl.a(203) | ent.a(360) |
   ent.a(323) | ekb.a(83) | elq$a.a(114) | ejz.a(100)
   ```

   mapping(`03f89854…`)での名前: `arb` = `WorldGenRegion`、`dde` = `LevelWriter`
   (`setBlock(pos,state)` default、行15)、`ent` = `StructureTemplate`、
   `ekb` = `TemplateStructurePiece`、`elq$a` = `IglooPieces$IglooPiece`、
   `ejz` = `StructureStart`。すなわち
   `IglooPiece.postProcess` → `TemplateStructurePiece.postProcess` → `StructureTemplate`
   の**2つ目の経路**(template セルは `ent.a(246)`、残差は `ent.a(360/372)` 経由)。

## 4. 記述子:何が `snowy=true` を書くのか

`ent` の line 361–374 の method(`javap -p -c -l`)は:

```java
// StructureTemplate:361..374(3引数の setBlock ではなく flags &~1)
BlockState next = state.updateShape(dir, placed, level, pos, neighborPos);   // line 371
if (next != state) level.setBlock(pos, next, flags & -2);                    // line 372
```

つまり template の `snow_block` を置いた後、**隣接ブロックの状態を
`BlockState.updateShape` で問い直して書き戻す**経路。その隣接ブロックが
`grass_block` で、方向が UP のとき:

```java
// SnowyDirtBlock (dnj).a(state, direction, neighborState, level, pos, neighborPos)
if (direction == Direction.UP)
    return state.setValue(SNOWY, isSnowySetting(neighborState));   // 行 2..24
return super.updateShape(...);
// isSnowySetting(state) = state.is(BlockTags.SNOW)  (awe.bA)
```

`#minecraft:snow` は `snow` と `snow_block` を含むので、**igloo の最下層 `snow_block` が
`grass_block` の直上に置かれた列だけ** `grass_block[snowy=true]` になる。
BB 内で y=80 が grass の列がちょうど 8 列 = 残差 8 セル。

## 5. port 側の owner(次レーンで直す場所)

`src/template_structures.rs`:

- `update_shape()`(行 1250〜)は `_wall_banner` / `_stairs` / `_wall` / `_fence` などの腕を持つが、
  **`SnowyDirtBlock`(`snowy` プロパティ)の腕が無い**。
- 呼び出し側は既にある(行 1005/1011 の両側 update、行 2016 の再適用)。したがって
  「隣接の `updateShape` を走らせていない」のではなく、**家族が1つ足りない**。
- `postprocess_shape_marks` / `postprocess_bush_shape`(行 1852/1937)は promotion 時の
  `BushBlock`/`DoublePlantBlock` 専用で、この差の owner ではない。

したがって次のスライスの最小変更は「`update_shape` に `SnowyDirtBlock` の腕を足す」
(方向 UP → `snowy = in_tag(neighbor, "minecraft:snow")`)で、対象は
`igloo` の 8 セル。ただし**このレーンでは `src/` を触っていない**(slice の discipline)。

## 6. 未測定・注意

- `-Dfeaturestack` は `tools/carve-agent/CarveLog.java` への opt-in 追加で、agent jar の
  sha256 が変わる(`61051cc698576f482aec59a51a9b04525a1ca1545a0440c07a9b10211a718f7a`)。
  本ページの capture は**診断用**で、受入行の `inputs.sha256` には入れていない。
- 上の stack の中間フレーム `exl`(行 203/225)は pinned mapping では
  `DiscreteVoxelShape` と引けるが、`StructureTemplate` → shape → `StructureTemplate`
  という入れ子は構造物配置の呼出し形として不自然なので、この 2 フレームの名前は
  **未検証**として扱う。検証は `javap -p -c -l` の line table と実引数で行う
  (`ent.a(360)` / `ent.a(372)` の本体は上で読んでおり、`updateShape` → 書き戻しという
  意味は確定している)。
- 主張しないこと: 「igloo が 8 セルで閉じる」ことは示していない。8 セル以外が一致して
  いるのはこの 1 chunk の話で、`updateShape` の腕を足した後にもう一度測る必要がある。
- 受入セット §3 には **HOLD** として記録した。行は昇格していない。
