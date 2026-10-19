# 光値の vanilla 比較（探針は動く。地形が一致しない）

`tools/light_parity_probe.py` は protocol 0x27 を読み、チャンクごとの
sky/block の light-mask と、mask が指す各区間の 2048 バイト配列の digest を出す。
両サーバで同じ seed・同じ view で動かし、同じ (cx,cz) を読めば比較できる。

## 探針が vanilla を読めるようにした修正

`tests/protocol767.py` の `decode_chunk_light` は、チャンクの
**区間配列を厳密に parse** して block state まで検証していた。これは
fixture（mc-spine のパレット）用で、vanilla の実チャンクでは
`non_air=4115 > 4096` のような不整合で `AssertionError` になり、
**光を読む前に落ちていた**（`light_parity_probe` は `STRICT=False` でも
この parse を通っていた）。

`STRICT=False` のときは区間配列を **parse せず読み飛ばし**、
後続の light-mask / 配列だけを読むようにした。光は区間配列の
あとに独立して並んでおり、その長さは varint で与えられるので、
パレット id が fixture と違っても光は読める。

また `Client.raw_chunk` に 0x27 の body を保存し、探針は
`MOTION_BLOCKING` heightmap（地形。光エンジンとは独立）を展開・表示する。

## 比較結果（同一 seed=0、chunk (0,0)）

| | vanilla | mc-spine |
|---|---|---|
| sky_mask | `0x700`（区間 8,9,10 のみ） | `0x3ffffff`（全26区間） |
| block_mask | `0x0`（送らない） | `0x3ffffff`（全26区間） |
| sky 区間8 | `e3e348713da23212` | `d48d514e1f29f9cd` |
| sky 区間9 | `4affdbe77c3695d6` | `e70b2161df86b603` |
| sky 区間10 | `d0ff1b294b5288d1` | `d0ff1b294b5288d1` |

- `e5a00aa9991ac8a5` = **全 nibble 0**、`d0ff1b294b5288d1` = **全 nibble 15**（2048 バイトの sha256 で確認）。
- したがって mc-spine の区間 0..7 は全0、区間 10..25 は全15。vanilla は
  全0/全15 の区間を **mask に載せない**（区間 8,9,10 は表面付近で非自明）。
- 区間 10 は両者一致（全15）。区間 8,9（表面付近）は **digest が違う**。

## 地形は一致する。heightmap の面の取り違えだった（訂正）

最初の比較で heightmap が一致しなかったのは、**mc-spine の `MOTION_BLOCKING` が
density 面（`ChunkCols::surface`、水も葉も含まない）を使っていた**ためである。
vanilla の `MOTION_BLOCKING` は流体を含む面（`heightmaps_nbt` は `wtop` を使うべき）。
`map_chunk` の `tops` を `wtop+1` に直すと、chunk (0,0) の第1行が
**vanilla と 7 列とも完全一致**した:

```
vanilla vals row0: [127,127,127,127,127,127,127,254,254,254,254,254,254,256,4,9]
spine   vals row0: [127,127,127,127,127,127,127,254,254,254,254,254,254,256,4,9]
```

この修正は前の「地形 mismatch」の結論を**撤回する**。地形（density 面と流体）は一致し、
不一致に見えていたのは heightmap の面の選択だけだった。残差は 256 列中 16 列（z=14,15 付近の
末尾 long）で、葉（`MOTION_BLOCKING` は葉を含む）が候補。

## 光の比較（地形一致後）

床（`sky_floors`）を「上から最初の光を遮るブロック」に近づける（`height_at+17` から下向き）と、
区間8の系統的な1レベル差が消えた:

```
vanilla section8 x=0,z=0: 0000000089abcdef
mc-spine（旧 -1 床）      : 000000009abcdeff   ← 1 明るい
mc-spine（新 +17 床）      : 0000000089abcdef   ← 一致
```

| 区間 | 旧床（`height_at-1`） | 新床（`height_at+17`） |
|---|---|---|
| 10（全15） | 0/2048 | 0/2048 |
| 9 | 39/2048 | 127/2048 |
| 8 | 382/2048 | **25/2048** |

## 残差は光エンジンではなく、worldgen の feature 差

区間9が「悪化」したのは、差が **x=6..9, z=9 の小さなクラスタ**に集中し、そこに
mc-spine が `oak_leaves`（y=66-67）を持つためである（chunk (0,0) を厳密復号して確認）:

```
x=6 z=9: 66:0 67:252(oak_leaves) 68:0
x=7 z=9: 66:248(oak_leaves) 67:248(oak_leaves) 68:0
```

**しかし vanilla にはこの木が無い。** `MOTION_BLOCKING`（葉を含む）の packed long は
index 150-153（z=9,x=6..9）で両者**完全一致**しており、vanilla が葉を持てばこの列の
heightmap は水面(127)より高くなるはずが、一致したままである。

つまり残差の原因は光エンジンではなく、**mc-spine の feature/populate 層が
vanilla に無い木を置いている**ことにある（worldgen の feature パリティ差）。

## 証拠の境界（主張しないこと）

- これは 1 サーバ・1 seed・数チャンクの比較で、feature 差の原因（配置位置・確率・乱数消費）
  は特定していない。
- 探針は client の復号（protocol 767）で読んだ wire 値であり、クライアントの画面表示ではない。
- heightmap の展開は末尾 long 付近で値域外（>384）を出すため、列の packed long の一致でのみ使った。

## feature 順序の vanilla アルゴリズム（逆コンパイル確定）

残差の正体は feature の step 内順序。vanilla の `FeatureSorter.buildFeaturesPerStep`
（obf `deg.a`、`java.util.List,Function,boolean`）を `javap` で確定した:

1. `featureIndex` = `Object2IntOpenHashMap` + `MutableInt.getAndIncrement`。
   バイオームを **step 順に flatten した初出順**で feature にグローバル index を割り当てる。
2. 各バイオームについて全 step を flatten した `nodeList` を作り、**隣接要素間にエッジ**
   （step 境界をまたぐ）。
3. `Comparator.comparingInt(deg$a::step).thenComparingInt(deg$a::featureIndex)` の
   `TreeSet` を用いた **DFS の post-order を `Collections.reverse`** した順が topological order。
   （`deg.a(java.util.Map,java.util.Set,java.util.Set,java.util.function.Consumer,Object)`
   = `ayd.a` の DFS。）
4. step ごとにグルーピングし、`deg$b(features, Util.createIndexIdentityLookup(features))`。
   `indexMapping` は **その step リスト内の位置**（`Util.h` = `list::indexOf` 相当）。

つまり `gi` は step リスト内位置で mc-spine と同じだが、**順序を作るアルゴリズムが違う**：
vanilla は DFS post-order reversed + cross-step エッジ + `step→featureIndex` 比較子、
mc-spine は Kahn FIFO。これが「vanilla に無い木」の原因。

比較子・エッジ・DFS は obf から確定済み（`deg.javap.txt`, `ad.javap.txt`）。

## vanilla のブロックを列単位で読めるようにした（突破口）

`decode_chunk_light` の strict 失敗は区間長の desync ではなく、
`non_air != sum(st not in AIR_STATES)` の**fixture の air state id 不一致**だけだった。
区間の palette を registry 上限なしで読むと vanilla のブロックが読める。

chunk (0,0) の x=2..4, z=12 を比較:

```
mc-spine:  y=62 dirt,  y=65-66 oak_leaves
vanilla :  y=62 grass_block,  上は空気
```

残差の「vanilla に無い木」の下は、mc-spine が `dirt`、vanilla が `grass_block`。
つまり light でも tree RNG でもなく、**地表材質ルール（surface rule）が
grass_block を置くべき列で dirt を置いている**のが根。tree の RNG 照合は
高さ・trunk・selector・foliage 高さ/半径・skip 判定・座標ループすべて一致済み
（`be25cbd` を含む）。

次の一手は `src/worldgen.rs` の surface rule（`mat`/`eval_rule`）で、
`grass_block` を置く条件（上に空気、biome、`stone_depth_above` 等）を
vanilla と突き合わせること。

## worldgen パリティの現状（2026-10 追記）

`tools/chunk_block_diff.py` と `MC_SPINE_NOFEAT=1` で測った残差:

```
chunk (0,0), seed 0: feature あり 11856/98304 (12.1%)
                      feature なし  7727/98304 ( 7.9%)
```

- **ノイズスタックは vanilla と一致**（bytecode で確認）: `XoroshiroRandomSource` seed、
  `fromHashOf(x,y,z)` = `RandomSource(getSeed ^ lo, hi)`、`fromHashOf(name)` = md5
  `Longs.fromBytes` ^ lo/hi、`ImprovedNoise` コンストラクタ（offset/perm/`nextInt(256-i)`
  シャッフル）とノイズ関数（8勾配・`lerp3`・`smooth`）、`NormalNoise` シード
  （旧 `splitter()` が最良で、`forkPositional`/`state()` や `valueFactor` 単純置換は悪化）、
  `vertical_gradient` = `Mth.map(y,b,e,1,0)` + `fromHashOf`。
- **密度 DF 演算も一致**: `squeeze`/`min`/`mul`/`gradient`/`lerp`/`range_choice`。
- **残差の主因（2026-10 追記、逆コンパイルで確定）**: vanilla の `overworld/final_density` は
  ```
  add( min( squeeze( interpolated( mul(blend_density(...), sloped_cheese) ) ),
            overworld/caves/noodle ),
       beardifier )
  ```
  で、`interpolated` は **`mul(...)` の部分だけ**を包む。外側の `squeeze`/`min`/`add` は
  非線形（`squeeze` は三次）なので、`squeeze(lerp(X))` と `lerp(squeeze(X))` は一致しない。
  mc-spine は `cols()` で `final_density` **全体**をセル隅評価してから `density()` で三線形
  補間するため、`lerp(squeeze(X))` になっている。**これが地形 7.9% 残差の有力原因**。
  正しい順序は「`interpolated` の入力だけを隅評価→三線形→その外側をブロック毎に評価」。
- **`interpolated` を `Cache`（恒等）に落としているのは誤り**（上記）。以前の
  「`DF::Interp` を足しても diff 不変」という測定は、`cols()` が依然として全体を隅評価して
  いたため二重補間になっただけで、順序の誤りを直していなかった。
- **`beardifier` は `DF::Zero` 固定だが、地形残差の原因ではない**: 新しい vanilla ワールドを
  `generate-structures=false` で生成しても chunk (0,0) の diff は 7727 のまま不変（実験で否定）。
  構造物の有無で vanilla のチャンク digest は変わるが、地形セルの比較には影響しない。

### 探針の既知制約

vanilla は移動しきい値を超えるテレポートを無視する（`moved too quickly`）ため、任意の
チャンクを wire で取得できない。`(0,0)`（ログイン位置付近）のみ安定して取れる。
`tools/chunk_block_diff.py` / `tools/vanilla_chunk_blocks.py` は 8 ブロック刻みで歩くが、
vanilla の任意座標取得にはログイン位置自体を対象チャンクにする必要がある。
