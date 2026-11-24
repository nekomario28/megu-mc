# `village_plains` `(−338,−384)` seed 4242: 159 / 98,304 の残差(jigsaw piece 差)

Status: **HOLD(受入しない)。`src/` は無変更。測ったのは
「どの piece BB の中で、どちら側が何を書いたか」までで、最初の分岐は未測定。**

## 0. Provenance

| | |
|---|---|
| base commit | `0fa6807`(`perf/worldgen-parallel`) |
| port binary | `target/debug/mc-spine` sha256 `7dc623bf3372e5e8d71ad6cab6b65196f2a3ba7f125862234b4371524c4ef7ca` |
| capture | `<local>/captures/matrix-v3/nat-4242-village-plains`(frozen, overworld, window −64/384, seed 4242, `Chunks` 1) |
| port dump | `dumps/nat-4242-village-plains/dump.bin` sha256 `c372f3700e20778b…` |
| triage | `dumps/nat-4242-village-plains/{triage.json,port_triage.log}` |
| heightmap 対照 | `dumps/nat-4242-village-plains-hm/`(`--port-hm --hm-name MOTION_BLOCKING`) |

capture は `tools/structure_capture.py --structure minecraft:village_plains --seed 4242
--scan-radius 384` が作った。start は `minecraft:jigsaw#0`
BB `[−5408,64,−6151,−5394,68,−6144]`、窓内に 26 piece(INTERSECTS 18 / halo≤1 8)。

## 1. 数字

```
diff summary: all_air_kinds=159 / 98,304  (lossy non_air_projection=159)
native non_air 30,966 / port non_air 30,979
MOTION_BLOCKING: port vs native saved = 46 columns differ (deltas -5..+7)
```

heightmap が 46 列で違うのは**別の原因ではない**。piece のブロックが違えば、その列の
motion-blocking top も違う。したがって heightmap 差は piece 差の従属量として読む
(逆に「heightmap が合っているのにブロックが違う」igloo の残差とは性質が違う)。

## 2. 残差の分布(native → port)

すべて `inside_structure_bb`(帰属は native の `structures.starts.Children[].BB`)。

| piece BB | セル数 | native → port |
|---|---|---|
| `jigsaw#32` `[−5408..−5400, 66..72, −6133..−6129]` | 84 | `air` → `oak_planks` 24 / `air` → `oak_stairs[facing=south,half=bottom…]` 15 / `air` → `oak_log` 9+4+2 / `air` → `cobblestone` 7 / `short_grass` → `cobblestone` 2 / `oak_trapdoor[open=true]` → `air` 2 |
| `jigsaw#21` `[−5398..−5394, 64..69, −6143..−6139]` | 60 | `oak_leaves[distance=1..4]` 44 → `air`、`oak_log` 4 → `air`、`dirt` → `grass_block[snowy=false]` 1、`short_grass` → `tall_grass[lower]` 1、`oak_log` → `dandelion` 1 |
| `jigsaw#35` `[−5395..−5394, 64..65, −6137..−6135]` | 4 | `dirt_path` → `grass_block[snowy=false]` 2、`air` → `tall_grass[lower]` 1、`air` → `short_grass` 1 |
| `jigsaw#54` `[−5396..−5396, 63..64, −6133..−6132]` | 3 | `air` → `tall_grass[lower]` 1、`air` → `tall_grass[upper]` 1、`short_grass` → `air` 1 |
| その他(halo `jigsaw#19`) | 1 | `grass_block[snowy=false]` → `dirt_path` |

読み方:

- `jigsaw#32` は **port だけが家の部材(planks / stairs / log / cobblestone)を書いている**。
  native の同じ BB には air がある。
- `jigsaw#21` は **native だけが樫の木(leaves + log)を持つ**。port は air。
- `jigsaw#35`/`#54` は `dirt_path`↔`grass_block`、`tall_grass` の有無という小さな差。

この2つの大きい塊は「同じ位置に別のものを置いた」ではなく、**piece の内容が入れ替わって
いる**形(家が立つ場所と木が立つ場所)に見える。ただし piece の位置・template id・rotation を
両側で並べていないので、これは**観測の読み**であって原因の同定ではない。

## 3. 対照(比較が全セルを見ていること)

`village-taiga`(既存受入行、3×3 `(4,−32)` seed 4242、`matrix-v1/nat-4242-village-taiga`)
は同じ binary で **0 / 884,736**。したがって
「jigsaw 一般が壊れている」わけではなく、この seed/chunk の `village_plains` の
piece 選択・配置に差がある。この対照は
[p2-1 受入lane S08](../../../specs/worldgen-parity/finite-acceptance-set.md) の実測と同じ binary で取れる。

## 4. 次の計測(このレーンでは実行していない)

1. port 側の jigsaw piece 列(template id・BB・rotation・mirror)を、native chunk の
   `structures.starts["minecraft:village_plains"].Children[]` と **piece 単位で対応づける**
   (port には start を列挙する CLI が無いので、まず `--worldgen-structure-candidates` 相当の
   読み出し口が要る。`src/` の変更なので別レーン)。
2. 対応づけた後、`jigsaw#32` と `jigsaw#21` のどちらが先に分岐するかを
   `JIGSAW` 系 trace で決める(port の jigsaw 実装は `src/jigsaw_structures.rs`)。
3. 木(`oak_leaves`)の差は `trees_plains` の配置と piece の前後関係に依存するので、
   piece が一致した後に再測する。piece が違うまま木の差を追うと、原因を2つに割ったまま
   どちらも閉じない。

## 5. 何を主張しないか

- 159 セルを「village_plains の jigsaw 実装が間違っている」と分類していない。
  測ったのは分布・帰属・heightmap の従属性まで。
- 受入セット(`finite-acceptance-set.md` §3)には **HOLD として記録**し、行は昇格していない。
  理由は本ページ。
