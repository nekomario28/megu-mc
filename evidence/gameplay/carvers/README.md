# 洞窟（carver）の基準値

2026-10-20。world 生成の残差のうち、洞窟は**未実装**で、これまで一度も
比較していなかった。ここはその規模を測った記録。

## 固定具

`tools/vanilla_nofeat_pack.py <jar> <world> [--keep-carvers]`:

- 既定: biome の `features` を空にし、`carvers` も空にする
  （地形だけを見る比較用。従来の terrain-parity はこれ）。
- `--keep-carvers`: `features` だけ空にし、`carvers` は jar のまま残す。
  **洞窟だけの世界**になり、carver の寄与を直接測れる。

どちらも biome ファイルは jar から書き、`pack.mcmeta` を置く。新しい world で
起動すること（生成済みチャンクは再生成されない）。

## 測定（seed 0、chunk (0..5, 0..5) の 36 chunk、y<60）

| | y<60 の空気セル |
|---|---|
| carvers 有効 | **3361**（93/chunk） |
| carvers 無効 | 143 |
| carver 由来 | **3218（96%）** |

- 洞窟の 96% は carver（`cave` / `cave_extra_underground` / `canyon`）由来で、
  density function の洞窟（`overworld/caves/*`）はごくわずか（143 セル）。
- 個別 chunk では 0〜1166 セルとばらつく（(0,0) は 44 セルでほぼ固体）。
  **1 chunk だけ見て「洞窟は無い」と結論しない**。

## いまの mc-spine

`src/` に carver の実装が無く、`datapacks/vanilla` に `configured_carver` も無い。
biome の `carvers` リスト（1.21.1 の jar と同内容）はデータとして存在するが読まれない。
したがって mc-spine の地下空気は 0 で、vanilla との差は約 93 セル/chunk。

## 次の一手

1. `configured_carver/cave.json` 等を jar から同梱データへ（`state`・`replaceable`・
   `y`・`yScale`・`lava_level`・`debug_settings`・`probability`）。
2. `CarvingContext`（surface height / aquifer）と `CaveWorldCarver`
   （tunnel・branch・room、`carve_ellipsoid`）を bytecode から移植。
3. feature より**前**に走らせる（vanilla の順序）。
4. `--keep-carvers` の世界と block diff を取り、0 セルを出口条件にする。

## 1. configured_carver を同梱（2026-10-20 実施）

jar の `data/minecraft/worldgen/configured_carver/*.json` を
`datapacks/vanilla/.../configured_carver/` へそのまま置いた（4 個）:

| file | type | probability | y | yScale | lava |
|---|---|---|---|---|---|
| cave | cave | 0.15 | uniform(above_bottom 8 .. absolute 180) | uniform(0.1 .. 0.9 exclusive) | above_bottom 8 |
| cave_extra_underground | cave | 0.07 | uniform(above_bottom 8 .. absolute 47) | uniform(0.1 .. 0.9) | above_bottom 8 |
| canyon | canyon | 0.01 | uniform(absolute 10 .. 67) | 3.0 | above_bottom 8 |
| nether_cave | nether_cave | 0.2 | uniform(absolute 0 .. below_top 1) | 0.5 | above_bottom 10 |

`replaceable` はいずれも `#minecraft:overworld_carver_replaceables` / `#nether_carver_replaceables`。
読み込みが正しいことは `bundled_carver_configs_are_the_1_21_1_shapes` で固定（type と
`probability`/`y`/`yScale`/`lava_level`/`replaceable` の存在）。

## 2. 移植先の obf 名（`server_mappings.txt` から）

| named | obf |
|---|---|
| CaveWorldCarver | `ebb` |
| CanyonWorldCarver | `eaw` |
| WorldCarver (base: carveBlock / canReplaceBlock / isStartChunk) | `ebe` |
| CarvingContext | `eaz` |
| CarverConfiguration | `eax` |
| CaveCarverConfiguration | `eba` |

`CarvingMask` は名前一致で見つからないので、`CarvingMask` の内部型（`$Mask`）を
`javap` で探すところから。移植の順序: `WorldCarver.carveBlock`（replaceable 判定・
`cave_air`/溶岩/水の充填）→ `CaveWorldCarver.carve`（tunnel/branch/room、
`carveEllipsoid`、`CarvingMask`）→ `CarvingContext`（surface height と aquifer）→
chunk 生成の feature 前に呼ぶ。

## 3. 移植の足がかり（obf シグネチャ、2026-10-20 時点）

`javap -p` で確認した入口:

```
ebb (CaveWorldCarver) extends ebe<eba>
  public boolean a(eba config, ayw random)                      // isStartChunk: random.nextFloat() <= probability
  public boolean a(eaz ctx, eba cfg, duy chunk, Function<jd,jm<ddw>> biomes,
                   ayw random, dyk chunkPos, dcd region, dux mask)   // carve(...)  ← dux = CarvingMask
  protected int   a()                                            // getRange()
  protected float a(ayw r)                                       // config.yScale のサンプル？
  protected double b()                                           // 定数（tunnel の太さ係数）
  protected void  a(ctx, cfg, chunk, biomes, dyk pos, double x, double y, double z,
                    float a, double b, dux mask, ebe$a skip)      // carveTunnel
  protected void  a(ctx, cfg, chunk, biomes, long seed, dyk pos, double x, double y, double z,
                    double a, double b, float c, float d, float e, int f, int g, double h,
                    dux mask, ebe$a skip)                         // carveTunnelBranch
  private static boolean a(double,double,double,double)           // 交差判定
ebe (WorldCarver) extends ... <C extends eax>
  // ebe に carveBlock / canReplaceBlock / carveEllipsoid がある。eax = CarverConfiguration
```

次に bytecode から読むべきは `ebb.a(eaz, eba, duy, Function, ayw, dyk, dcd, dux)`（carve 本体）で、
**draw の順序**（`config.y.sample` → `nextFloat()*PI` → `yScale.sample` → tunnel/branch の分岐 →
`random.nextInt` の入れ子）を確定させる。`isStartChunk` が `nextFloat() <= probability` なので、
チャンクごとに 1 回この判定が走る（`cave` 0.15 / `cave_extra_underground` 0.07 / `canyon` 0.01）。

移植は次の順で、各段で `--keep-carvers` の世界（36 chunk、3361 セル）に対して
block diff を取る:

1. `CarvingMask`（chunk 内 16×384×16 のビット列。`dux`）
2. `WorldCarver.carveBlock`: `#overworld_carver_replaceables` 判定 → `lava_level` 以下は溶岩、
   それ以外は `cave_air`、aquifer があれば水
3. `CaveWorldCarver.carve` + `carveEllipsoid`（r=1 の楕円体、`skip` 判定）
4. `CarvingContext`（surface height は `chunk_cols` の `surface()`、aquifer は既存の `aq_state`）
5. `applyCarvers`: biome の `carvers` を step 順に、**feature より前**に実行

## 5. 移植（2026-10-21 実施）

読み取った bytecode のとおりに実装した。`src/feature.rs` の `// ---------- carvers` 節:

| 移植したもの | obf | 内容 |
|---|---|---|
| `apply_carvers` | `dzb.a(arb,long,...)` | origin を -8..8 で走査し、origin chunk の**最小角 (x*16,0,z*16)** の biome の carver リストを k=0.. で回す。seed は `setLargeFeatureSeed(worldSeed + k, ox, oz)` |
| `carve_block` | `ebe.a(...)` | `#overworld_carver_replaceables` 判定、lava_level 以下は溶岩、他は aquifer。grass/podzol を掘ったら**下**が dirt なら `top_material` で表面を張り直す（`reachedSurface`） |
| `cave` / `room` / `tunnel` / `ellipsoid` | `ebb` | tunnel 数 `nextInt(nextInt(nextInt(15)+1)+1)`、room は `nextInt(4)==0`、branch 数 `112 - nextInt(28)`、1/4 捨て、`canReach`、fork（`yScale>1`） |
| `canyon` / `canyon_tunnel` / `canyon_widths` | `eaw` | `getWidths`（高さごとの幅）、`getY`（三角窓 × yScale × 0.75..1.0）、`skip` は `(x²+z²)*w + y²/6 >= 1` |
| `CarvingMask` | `dux` | `(x&15) \| ((z&15)<<4) \| ((y-minY)<<8)` のビット列。チャンク共有で二度掘らない |
| `top_material_at` / `carve_state` / `prelim_min` | `eaz`, `dyk` | `worldgen.rs` 側。surface rule を stone_depth 1/1・fluid=y+1 で 1 点評価する |

**1.21.1 の bytecode で確定した、推測だと間違える点**

- 楕円体の正規化は x/z が `+0.5`、**y だけ `-0.5`**（`carveEllipsoid` の
  `i2d; ldc 0.5; dsub`）。`+0.5` にすると各楕円体の上端が 1 セル高く出る。
- `skip` は「`ny <= floor_level` なら捨て」であって `>` ではない（`dcmpg; ifgt`）。
- cave の tunnel 方向は `x += cos(yaw)*cos(pitch)`, `z += sin(yaw)*cos(pitch)`。
- `canyon.json` の `yScale` は**素の数値**（codec が `either(float, FloatProvider)`）。
- `replaceable` は `#` 付きで書かれているが tag 引きは `#` 無し。
- tunnel の RNG は `RandomSource.create(seed)` = **LegacyRandomSource**（Xoroshiro ではない）。

### 5.1 見つけた既存の欠陥: `Lcg::next_i64` が符号拡張していなかった

`RandomSource` の既定 `nextLong` は `(long)next(32) << 32 + next(32)` で、両半分は
**signed int** として符号拡張される。`generation_math.rs` は両方を unsigned として
連結していたので、下位ワードの bit31 が立つ約半分の draw で結果が 2^32 ずれていた。
`setPopulationSeed` / `setLargeFeatureSeed` / tunnel seed はすべてこの上に乗る。
`java.util.Random(0).nextLong()` = `-4962768465676381896` を
`legacy_next_long_sign_extends_like_javas` で固定した。

測った影響（seed 0、2x2 chunk、feature 有効で vanilla と比較）: 44187 → 45226 セル。
この層には**別の相殺する欠陥が残っている**（R1a）ので数字は悪くなったが、
二重の誤りは校正値ではない。carver の seed は正しい値でなければならないので修正は残す。

### 5.2 検証の手順（再実行できる形）

1. `tools/carver_diff.py <dump.bin> <world_dir> --n N --x0 X --z0 Z` —
   mc-spine のオフラインダンプ（`MC_SPINE_WG_DUMP`）と vanilla の region を
   ブロック state id で比較する。`Status` が `full` でない chunk は飛ばす。
2. `tools/vanilla-probe/CarverProbe.java` — vanilla の carver クラスを**そのまま**
   呼び、all-stone の fake chunk に書かれたセルを出す。`-Dcanyon=1 -Dk=2` で canyon、
   `-Dextra=1 -Dk=1` で cave_extra。mc-spine 側は
   `CARVE_PROBE="<tx>,<tz>" CARVE_ONLY=<id>` で同じ世界（all-stone）を作る。
   これで**同じ chunk のセル集合が完全一致**することを確認した:

   | carver | chunk | cells (java = rust) |
   |---|---|---|
   | cave | (0,-3) | 1400 = 1400 |
   | cave_extra_underground | (-5,-3) | 3042 = 3042 |
   | canyon | (-5,-3) | 342 = 342 |

3. `tools/carver_fixture.sh <tag> <cave> <extra> <canyon> [N]` — 設定を差し替えた
   vanilla 世界を作り、両側が同じ JSON を読む状態で比較する。制御実験
   （`y` を absolute 40 の定数、probability 1.0、3 carver とも同じ config）では
   **4 chunk 393216 セル中 1 セル**しか違わない。つまり world への配線
   （surface rule 済み地形の読み、mask、feature より前、`top_material`）は正しい。

### 5.3 残差（未解決）

新しい固定具（seed 0、1.21.1、`--keep-carvers`、feature 無し、49 chunk =
chunk (-5..1, -3..3)）で測り直した:

| | セル |
|---|---|
| vanilla の carver 由来（no-carver 世界との差、solid→air） | **53601**（うち y<60 が 51419） |
| mc-spine の carver 由来 | 47212 |
| 一致 | 5289 |
| vanilla のみ（掘り漏れ） | 48312 |
| mc-spine のみ（掘りすぎ） | 41923 |

carver を切った地形そのものは同じ固定具で **49 セル**（48 = copper vein の
`granite ↔ stone` / `copper_ore`、1 = density 由来の洞窟 1 セル）で、これは
R1a（ore vein）の既存残差。

**次の一手**: 残差は「同じ楕円体の上で、セルごとの採用判定が違う」形ではなく
**tunnel の位置そのものが違う**形で出ている（同じ chunk で y 帯が 27..69 と
-40..10 に分かれる）。個々の carver は vanilla のクラスと完全一致するので、
残る差は `carveBlock` の入力だけ:
`carve_state` が `aq_state(x,y,z,0.0)` を呼ぶ経路は**地形比較では一度も
踏まれない**（地形は density<=0 のセルでしか aquifer を引かない）。したがって
最初に疑うべきは `dyk$c.a(dyr$b, double)`（`NoiseBasedAquifer.computeSubstance`）の
density=0.0 経路 — barrier 判定 `density + sim12*calc > 0` と 3 近傍の重み。
`tools/vanilla-probe/` に aquifer 用の probe を足して、同じセル集合で
`aq_state` を直接突き合わせるのが最短。

## 6. 残差の切り分け（2026-10-21 後半）

§5.3 の残差を「tunnel の位置」ではなく「carveBlock の入力」に絞った記録。

### 決定的だった 1 chunk の比較

固定具 `/tmp/carverfix-caveonly`（`tools/carver_fixture.sh caveonly cave.json zero zero 2`、
cave だけ probability 0.15、extra/canyon は probability 0.0 を両側に入れた world）の
chunk (1,1):

| | セル数 |
|---|---|
| vanilla のクラスを直接呼ぶ probe（all-stone、cave） | **401** |
| mc-spine の同じ probe | 303 |
| vanilla の**実 world** が掘ったセル | **0** |

注意: この probe は `dyk`（aquifer）を **proxy で渡している**（`CarverProbe.java`
167 行、全メソッド null 返し = 「aquifer なし」）。したがって probe の 401 は
**地形形状だけ**の値で、実世界の 0 は「vanilla の本物の aquifer が全て拒否した」
ことしか言っていない。ただし同じ 2x2 の他のチャンクは 20/49/8 セル掘られており、
(1,1) だけ 0 は設定（cave probability 0.15）依存の局所的なもの。

start 集合も一致していた（36 origins、Java probe と完全一致）。つまり
**同じ tunnel が、vanilla の実 world では 1 セルも掘られていない**。
`carveBlock` は `canReplaceBlock` か `getCarveState` で全部 false を返した、という意味。

- `#overworld_carver_replaceables` の tag ファイルは jar と**バイト一致**（入れ子の
  `base_stone_overworld` / `dirt` / `sand` / `terracotta` / `iron_ores` / `copper_ores` も）。
  我々の側の `cur` も全て stone（tag 内）。
- したがって残るのは `getCarveState` → `aq_state(x,y,z,0.0)` の**barrier 判定**だけ。

### bytecode と突き合わせて一致を確認した部分

`dyk$c`（NoiseBasedAquifer）を 1 メソッドずつ読んだ。以下は mc-spine と**一致**:

- `a(int,int)`（近接度）= `1.0 - |d1-d0|/25.0`
- `dyk$c` の static `C`（13 個の 2D オフセット表）= `AQ_OFF` と同内容・同順
- 12 セル（i∈{0,1}, j∈{-1,0,1}, k∈{0,1}）から近い 3 つを選ぶ挿入
- セル fluid の走査（center 特例、`minSurface`、`+8`、`computeFluid`）
- レベル決定 `a(IIILdyk$b;IZ)I`: `clampedMap(top,0,64,1,0)`、`map(f,1,0,-0.3,0.8)`、
  `map(f,1,0,-0.8,0.4)`、`floor(s/3)*3 + cellY*40 + 20`、`min(...)`
- kind 決定 `a(IIILdyk$b;I)Ldtc`: `level <= -10 && level != センチネル && state != LAVA`
  → lava ノイズ `|v| > 0.3` で溶岩
- センチネル `dwy.g`: `(65 - h - i) << 4`（h=i=1+2^25）= **-1073740816**。
  mc-spine の `AQ_MIN = -32512` と**挙動は同じ**（どの y よりも下なので「流体なし」）
- `calc_density`: 溶岩/水ペアで 2.0、`|Δlevel|==0` で 0、`o = hd - |sc|`、
  `sc>0 ? (o>0?o/1.5:o/2.5) : (p=3+o; p>0?p/3:p/10)`、`q∈[-2,2]` の外は 0、
  `2*(r+q)`。**完全一致**
- barrier の 3 テストの組み合わせと重み（`sim12*calc(p14,p16)`,
  `sim12*sim13*calc(p14,p18)`, `sim12*sim23*calc(p16,p18)`）、`MutableDouble` の共有

### 残る唯一の候補

`a(IIILdyk$b;IZ)I` の**最初の分岐**だけ形が違う:

```
0: new dyr$e(x, y, z)
12: getfield u:Ldyr;  16: getfield v:Ldyr;  22: invokestatic dem.a:(Ldyr;Ldyr;Ldyr$b;)Z
25: ifeq 41                      // false なら通常計算
28: d0 = -1.0; d1 = -1.0         // true なら「深い」= 両方 -1
```

mc-spine は `excl`（noise settings の `aquifers.exclusion`）1 本を `> 0.0` で見ている。
vanilla は **2 本の DF（`u`, `v`）と `dem.a(DF,DF,ctx)`** という別の述語。
ここが `deep` を決め、`deep` が true だとレベルが両方 -1 になって
`calc_density` の溶岩/水分岐と barrier の符号が変わる。

※ `dem.a(DF,DF,ctx)` は `dze`（noise router）の `h()`/`i()` = **erosion / depth** を
`< -0.225 && > 0.9` で見る述語だった。datapack の
`noise_settings/overworld.json` の `aquifers.exclusion` は
`min(-0.225 - erosion, depth - 0.9)` として**同じ述語を DF で表現**しており、
mc-spine の `excl > 0.0` は等価（`min(a,b) > 0 ⟺ a>0 ∧ b>0`）。よってここも一致。

**次の一手**: 残った候補は (a) aquifer の各 DF id の解決（barrier/flood/spread/lava/excl
が意図した DF を指しているか。terrain は通るので大半は正しいはず）、(b)
`aq_cell_fluid` に渡す座標が vanilla のセル原点（16/40/16 の倍数）と一致しているか、
(c) `split`（セルごとの乱数位置）の導出。切り分けには draw 列の比較が使える:
`CARVE_RNG="<ox>,<oz>"` で `Lcg` の全 draw を `RNG <method> <value>` として出し、
`java -Dleg=1 -Donly=<ox>,<oz> -cp ... CarverProbe 0 <tx> <tz>` の `DRAW` 行と並べる
（probe 側は `n[0] < 60` 件、こちらは 400 件で止まる）。`tools/vanilla-probe/` に aquifer 用の probe（NoiseChunk を
Unsafe で確保し `computeSubstance` を直接叩く）を足せれば、セル単位で
`aq_state` を突き合わせて即座に白黒つく。

## 7. 残差は aquifer の「セル流体」だけ（2026-10-21 続き）

### 形状は完全一致（もう疑わなくてよい）

chunk (1,1) で `CARVE_PROBE`（all-stone）と Java probe を突き合わせた:

```
mc-spine (CARVE_ONLY=minecraft:cave): 401 cells
CarverProbe 0 1 1                 : cells=401 writes=401
```

セル集合も一致。**トンネルの形・開始点・マスクは一致している。**
したがって差分は `carveBlock` の中で `getCarveState` が返す値だけ。

### 差分の内訳（49-chunk 固定具の chunk (1,1)、1450 セル）

| 我々 | vanilla | セル数 | y 範囲 |
|---|---|---|---|
| air | deepslate | 788 | -54..-10 |
| lava | deepslate | 243 | -62..-55 |
| water | stone | 152 | 39..49 |
| air | stone | 151 | 30..41 |
| stone | water | 116 | 39..44 |

vanilla 側が「何も書かない」= `getCarveState` が **null**（barrier）。
我々は 1334 セルで書き、116 セルで書かない。両方向・狭い y 帯 = 値のずれ。

### 我々の判断（`AQ_TRACE="1,1"` の実測）

- air-vs-deepslate の 788 セル: `sim12 = -1.24 .. -1.72`（負）で
  **早期 return 経路**。最近 2 セルの level はどちらも `-32512`（センチネル）
  → `lvl_block` が 0 → air。
- water-vs-stone の 152 セル: `sim12 = 0.80 .. 0.96`（正）で barrier 経路。
  最近 2 セルの level がどちらも `63` → `|Δ| = 0` → `calc = 0` → barrier 不成立 → water。

vanilla が null を返すには `sim12 > 0` かつ `calc > 0` が必要。`sim12` は
**同じ 12 セル・同じ乱数位置**から出るので一致するはず。よって残るのは
**セル流体（level, kind）の値そのもの**。それは
`b(px,py,pz)`（13 列スキャン）だけで決まる。

### `b(int,int,int)`（13 列スキャン）の bytecode（転記）

```
def = t.computeFluid(x, y, z);   minEst = MAX;  j = y+12;  k = y-12;  foundWater = false;
for (off : C) {                                  // C = 13 個の (dx,dz)
    xx = x + (off[0] << 4);  zz = z + (off[1] << 4);
    n  = l.a(xx, zz);                            // 予備地表レベル
    o  = n + 8;
    center = off[0]==0 && off[1]==0;
    if (center && k > o) return def;
    b18 = j > o;
    if (b18 || center) {
        fs = t.computeFluid(xx, o, zz);
        if (!fs.at(o).isAir()) { if (center) foundWater = true; if (b18) return fs; }
    }
    minEst = min(minEst, n);
}
level = a(x,y,z,def,minEst,foundWater);          // §6 で一致確認済み
kind  = a(x,y,z,def,level);                      // §6 で一致確認済み
```
mc-spine の `aq_fluid_level_at` はこの形と一致（`aq_pick(o)` = `t.computeFluid` の
y のみ版、`prelim_est` = `l.a`）。

### 最後に残った検証点: `prelim_est`

`prelim_est` は **aquifer 専用**（地形比較では踏まれない）。実測すると
chunk (1,1) の 256 列で

- 値は `{40, 48, 56}` の 3 種類だけ（8 ブロック量子化）
- 実際の地表（carvers 無効 world の最上位固体）との差は
  **平均 -11.97 / 中央 -14 / 範囲 -22..-7**（系統的に下）

datapack の定義は

```
chunk_surface_level     = interpolated(cell_size_xz=16, cell_size_y=1, preliminary_surface_level)
preliminary_surface_level = find_top_surface(cell_height=8, density=add(...))
```

量子化 8 は `find_top_surface.cell_height` と整合。系統的に下に出るのは
`find_top_surface` の探索範囲/終了条件か、`interpolated` の扱いのどちらか。
`minEst` は `f = clamp(minEst + 8 - y, 0, 64)` に入るので、14 の下振れは
`aq_fluid_y` の 2 つの比較（`-0.3/0.8`, `-0.8/0.4`）を確実に跨ぐ。

補足（同日、bytecode で確認）:

- aquifer が読むのは noise router の `chunk_surface_level` ではなく
  **`aquifers.surface_level`**（= `preliminary_surface_level`、生の
  `find_top_surface(cell_height=8)`）。前者は `interpolated(cell_size_xz=16)`
  で包んだ別の値。mc-spine は router 側を読んでいたので
  `aquifers.surface_level` 優先に直した（`eval` は `interpolated` を点評価する
  ので今回の datapack では数値は不変 = md5 一致。意味の修正）。
- 既定流体 picker（`dzb.a(dzd)` + `dzb.a(dyk$b,int,dyk$b,dyk$b,int,int,int)`）は
  `y < min(-54, seaLevel) ? (-54, lava) : (seaLevel, water)`。
  mc-spine の `aq_pick` と**一致**。`-54` はハードコードで正しい。

**次の一手**: `prelim_est` を vanilla の DF codec で評価して 256 列を数値比較する
（`JsonOps` + resource lookup が要るので小さな probe を 1 本書く）。ここが合えば
aquifer 連鎖の入力は全て vanilla と同一になり、残りは vanilla 側の
セルキャッシュ `r[]`/`s[]`（`a(III)` のスロット衝突）だけになる。

## 8. `prelim_est` は正しい（§7 の宿題を回収）

1.21.1 の aquifer が読む地表は **コード**で計算される（データではない）。
`dzc.a(long)`（= `getPreliminarySurfaceLevel`）の bytecode:

```
x,z = 4 ブロックに丸めた列;   y = minY + height;
while (y >= minY) {
    if (router.k().compute(ctx(x,y,z)) > 0.390625) return y;   // k() = initial_density_without_jaggedness
    y -= 8;
}
return Integer.MAX_VALUE;
```

リポジトリの datapack（後述のとおり 1.21.1 のものではない）は同じものを
DF で表現している:

```
preliminary_surface_level = find_top_surface(cell_height=8,
    density = <1.21.1 の initial_density_without_jaggedness> + (-0.390625))
```

両者を式レベルで突き合わせた: 1.21.1 の
`mul(depth, cache_2d(factor))` は、datapack 側の
`mul(add(gradient(1.5@-64,-1.5@320), offset), factor)` と同値
（`depth` の定義が `gradient + offset`、`cache_2d` は値を通すだけ）。
したがって `prelim_est` の数値は vanilla と一致する。§7 の「14 低い」は
`find_top_surface` が平滑化されることによるもので、ずれではない。

## 9. リポジトリの vanilla datapack は 1.21.1 のものではない

`datapacks/vanilla/.../noise_settings/overworld.json` は `aquifers: {...}` と
`material_rule` を持ち、`noise_router` は 8 キー。一方 1.21.1 の jar の同じ
ファイルは `aquifers_enabled`/`surface_rule` を持ち、router は 15 キー
（`initial_density_without_jaggedness`, `barrier`, `lava`, ... を含む）。
つまり**後方のバージョン（1.21.2+）のスキーマ**のデータが入っている。

- 依存している値は同値であることを確認済み（§8 の地表、§6 の exclusion、
  carver 3 種の JSON も 1.21.1 の既定と一致）。
- ただし「1.21.1 の bytecode に合わせる」対象としては由来が違うので、
  どこかの時点で 1.21.1 の jar から再生成するのが望ましい（TODO 化）。

## 10. 残差の見え方が変わった: vanilla はこの chunk でほぼ何も掘っていない

固定具（49 chunk）の実測:

| chunk | cave probe（vanilla のクラスを直接呼ぶ） | vanilla の実 world が掘ったセル |
|---|---|---|
| (1,1) | 401 | **0**（cave 由来。chunk 全体でも 116） |
| (-3,0) | — | 2870 |

(1,1) の 401 セルは**全て solid**（air も fluid も 0）。つまり vanilla の
本物の aquifer はこの chunk の cave セルを**全拒否**している。こちらの port は
sim12<0 の早期 return で空気を返すため、ほぼ全部掘ってしまう。

同じ 49 chunk の中で (-3,0) は 2870 セル掘れており、拒否率は chunk 依存。
この「拒否率の差」が残差の実体で、次に必要なのは
**実物の `NoiseChunk`（`dzc`）を持つ aquifer probe**。bytecode 上の鎖は
§6〜§8 で全部一致を確認済みなので、残る違いは
`dzc` のセル格子座標（aquifer が terrain 経路で 4x8x4 格子の座標で呼ばれる）
と、carve 経路（ブロック座標）での `r[]`/`s[]` キャッシュの共有状態。
probe では `dyk$c` を直接 new できない（`dzc` が要る）ため、
`NoiseChunk` を組み立てる小さな harness が必要。

## 11. 独立実装との突き合わせ（hyperchunk, MIT）

`Soju06/hyperchunk`（MIT, C11, 26.2 の bit-exact 主張、`openspec/` に bytecode
再構成ノート）の `core/src/aquifer.c` は、我々の `aq_state` と**分岐単位で一致**
した（similarity・13 offsets・12 セル走査・サンプル位置・早期 return・
water-over-lava・圧力 3 テスト・calc の短絡・level/kind・PSL メモ・
skipSamplingAboveY・carve 時の lava floor）。二つの独立実装が一致したので、
aquifer の論理そのものを疑うのは打ち切り。詳細は
`evidence/foundation/oss-survey/README.md`。

## 12. 今回直した 2 件（数値は不変、bytecode 一致のため）

1. **PSL の探索開始 y**: 1.21.1 は *コード*で `minY + height`（=320）から 8 刻みで
   下ろす。リポジトリの datapack（1.21.2+ 形式）の `find_top_surface` には
   `upper_bound` DF があり、この世界では **47** に評価される。そこから探すと
   予備地表が最大 270 ブロック低く出る。`prelim_est` を 1.21.1 の形
   （`minY + height` から開始、失敗時 `i32::MAX`）に変更。
   ただし実測では真の PSL が 47 より下（{40,48,56}）なので数値は変わらず。
2. **nearest-4 の同点規則**: bytecode は `dx 0..1 / dy -1..1 / dz 0..1` の順で
   比較はすべて `>=`（同点なら後着が前に挿入）。我々は `>` で順序も別だった。
   スロット 2/3 が圧力テストのペアを決めるので同点規則は観測可能。bytecode
   どおりに書き換え（この chunk では同点がなく数値不変）。

## 13. 残差の内訳（chunk (1,1), cave のみ 401 セル）

| | セル数 |
|---|---|
| 我々も vanilla も拒否（一致） | 250 |
| 我々が air にし、vanilla は stone のまま | 151 |
| 我々は stone のまま、vanilla が water にしたセル（**我々の 401 とは別のトンネル**） | 116 |

- 401 セルの**集合は vanilla のクラスを直接呼んだ結果と完全一致**（`DUMPCELLS`
  を足した probe で diff 0）。
- 116 セルは我々の cave/extra/canyon のどのセットにも含まれない（y 39..44 の
  水トンネル）。つまり固定具の world には、seed 0・jar の config・±8 chunk の
  走査から出るはずのないトンネルが 1 本ある。ここは固定具の生成条件そのものを
  疑う必要がある（`level.dat` は seed 0 / DataVersion 3955 / DataPacks
  vanilla+file/nofeat を確認済み）。
- 残差は「1 chunk あたり ±150 セルの局所的な不一致」で、系統的な全面不一致ではない。

**次の一手**: 実物の `NoiseChunk`(`dzc`) を組み立てて `dyk$c` を作る probe。
または固定具を作り直して（world を作る工程を 1 から記録しながら）116 セルの
トンネルの出所を特定する。

## 14. 高速固定具と、地形は bit-exact という新しい事実

`tools/carver_fixture.sh` を改良した（1 サイクル **9 秒**）:

- `CARVER_FIXTURE_X0/Z0` で chunk 原点を指定（spawn chunk を避けられる）。
- `CARVER_FIXTURE_FRESH=1` で world の region/entities/poi を消してから生成
  （`vanilla_nofeat_pack.py` が world 作成時に 1 回サーバを起動するため、
  spawn 周辺が ctl pack より前に生成されるのを防ぐ）。
- `trap ... EXIT` で repo の carver JSON を必ず復元（`| head` で殺されると
  作業ツリーが汚れたままになっていた）。

**新しい事実: 地形は bit-exact。** 3 carver すべてを `zero` にした固定具で
`differing cells: 0`（4 chunk = 393216 セル）。つまり terrain + surface rule +
aquifer の地形側の寄与は完全一致。残差は carve 段だけ。

carver 別（既定 probability, 2x2 chunk）:

| carver | 差分セル |
|---|---|
| canyon | **0** |
| cave | 617 |
| cave_extra_underground | 341 |

aquifer のサンプル位置（12 セル）は Java 側で
`RandomSource.fromSeed(0).nextPositionalRandomFactory().fromHashOf("aquifer")
.nextPositionalRandomFactory()` から直接計算した値と**完全一致**
（`tools/vanilla-probe/AquiferPos.java` と `AQ_POS="x,y,z"`）。
`legacy_random_source: false` なので Xoroshiro。

注意: 既存の `CarverProbe` は fake chunk を**全 stone**で埋めている。
`#overworld_carver_replaceables` に air/water は入らないので、実際の chunk では
空気/水のセルは `canReplaceBlock` が false になり掘れない。したがって
「probe の 401 セル」は上限であって、world の実測とは一致しない。

**次の一手**: `CarverProbe` の fake chunk を**実地形**（no-carve dump）で埋めて
から carve させる。そうすると `canReplaceBlock` の挙動まで world と同じになり、
probe がそのまま「本物の aquifer 抜きの oracle」として使える。
その oracle と我々の出力を比べれば、残差が幾何（トンネル）か aquifer かを
1 回で切り分けられる。dense 設定（cave probability 1.0）では既に 32023 セル差が
出ており、信号としては十分。

## 15. aquifer を切った固定具は level.dat を作り直さないと効かない

`aquifers_enabled: false` の `noise_settings/overworld.json` を ctl datapack に
入れても、**既存 world には反映されない**。`WorldGenSettings`（generator
settings 込み）は `level.dat` に**インラインで保存**されるため、datapack の
上書きは world 作成時にしか効かない。実測でも no-aquifer を期待した実行が
通常時とほぼ同じ差分（4025 セル）になった。

→ no-aquifer 固定具を作るには `level.dat` を消して `level-seed=0` で作り直す
必要がある（`server.properties` の `level-seed` を使う）。今回は未実施。

実装側は `aquifers_enabled` を読むようにした（`MC_SPINE_NOAQ` でも強制可）。
1.21.1 の `dyk$1`（aquifer 無効時の実装）と同じ
`density > 0 ? null : picker.at(y)` 経路に入る。

## 16. 未解決の核心: world の carve がモデルから再現できない

cave だけ有効にした固定具（4 chunk）で、world が掘ったセルは
(0,0)=20, (0,1)=8, (1,0)=49, (1,1)=**0**。一方

- 我々の port: 4 chunk で 540 セル
- vanilla のクラスを直接呼ぶ probe: (1,1) で 401 セル、(0,0) では **0** セル

つまり **world は「probe が 0 と言う chunk で 20 セル掘り、401 と言う chunk で
0 セルしか掘っていない」**。seed 0（level.dat で確認）・jar の carver config
（フィールド単位で一致確認）・±8 chunk 走査・`k = biome の carver list index`
というモデルからは再現できない。

候補は 3 つに絞られた:
1. **aquifer** が carve 経路でだけ我々と違う（ただし位置・similarity・level は
   検証済みで、`computeSubstance` が null を返す条件と矛盾するケースがある）。
2. **world の生成条件**がモデルと違う（spawn 生成・dimension の再生成・
   `level.dat` の generator settings など）。§15 の level.dat インライン化は
   その傍証。
3. **probe が忠実でない**（fake chunk が全 stone、biome 関数が null、ctx を
   Unsafe で確保）。ただし probe と port が 401 セル集合一致なので、少なくとも
   幾何に関しては両者が同じ実装を見ている。

次の一手は候補 2 の排除: 固定具を「level.dat を作り直す」手順に変え、
`level-seed=0` を明示したうえで同じ 4 chunk を測る。それで数字が変わらなければ
候補 1/3 に戻る。

## 17. probe に実地形を食わせたら完全一致 → 残るのは aquifer だけ

`CarverProbe` に `-Dterrain=<file> -Dterrainchunk=<tx>,<tz>` を足した。
`MC_SPINE_NOCARVE=1` のダンプ（98304 u32/chunk、section 順）を読み、
`FakeChunk.getBlockState` がその state を返し、`setBlockState` が配列も更新する
（`dfy.q.a(id)` で id→BlockState）。これで `canReplaceBlock` まで実 world と同じ。

結果（chunk (1,1), cave, 実地形）:

```
probe(vanilla のクラス, 実地形): cells=401
mc-spine CARVE_PROBE (実地形) : 401
集合差                       : 0
```

**幾何・置換判定・マスク・seed/k は完全一致**。probe が忠実でないという疑い
（§16 の候補3）はこれで消えた。

同じ chunk の world との差は 2 家族だけ:

| 我々 | world | ベース | セル |
|---|---|---|---|
| water | stone | stone | 152 |
| air | stone | stone | 151 |

**vanilla だけが掘るセルは 0**（この chunk では）。つまり我々が余分に掘っており、
その余分は carve 経路の aquifer 判定（`computeSubstance` が null を返すはずの
ところで air/water を返している）に由来する。

`carve_block` の `x & 15` 折り畳みで隣 chunk のトンネルを混入させている疑いも
検証した: ターゲット chunk 外を弾くガードを入れても差分は 617 のまま（ガードは
一度も発火しない）。よって折り畳みは無実。ガードは保険として残した。

残る焦点は `compute_fluid`（サンプル位置の level/kind）と `calc` の値で、
sim12・サンプル位置・PSL・DF 評価は検証済み。次は AQD ダンプに
`sim12<=0` 早期 return と `skip_above` 早期 return も含めて全分岐を記録し、
world の結果と 1 セルずつ突き合わせる。

## 18. §17 の結論を訂正: world の carve は p=1.0 でも再現できない

cave probability 1.0（他は 0）の固定具と、同じ config で vanilla のクラスを
直接呼ぶ probe を chunk (1,1) で比較した:

```
world が掘ったセル : 2353
probe が掘ったセル : 5486
共通               :  212  (world の 9%)
```

p=1.0 では `isStartChunk` の確率判定が常に真なので、start の有無・乱数消費の
差は原理的に消える。それでも 9% しか重ならないということは、
**carve 段の呼び出し（seed / 走査範囲 / 順序）そのものがモデルと違う**。

seed を 0..3 で振ってみた（`setLargeFeatureSeed(seed+k, cx, cz)`）:

| seed | probe セル | world との共通 |
|---|---|---|
| 0 | 5486 | 212 |
| 1 | 6366 | 647 |
| 2 | 6769 | 313 |
| 3 | 3331 | 110 |

どの seed でも一致しない（seed 1 が最も近いが 27%）。つまり
「world seed 0 + `seed+k`」というモデルでは world の carve を再現できない。

したがって §17 の「残差は aquifer だけ」は**言い過ぎ**だった。正しくは:

- 幾何（probe と port）は一致する ✓
- 置換判定・マスクも一致する ✓
- しかし **world の carve 自体がモデルから再現できない**（p=1.0 で 9%）
- よって per-cell の aquifer 比較（§17 の 216/98/87）は、まず carve 段の
  呼び出しモデルを直さないと意味を持たない

次の一手は carve 段の呼び出しの実測: サーバー側に Mixin / javaagent を入れて
`NoiseBasedChunkGenerator.applyCarvers` の `seed`, `sourcePos`, `carverIndex`,
`carver`, `isStartChunk` の結果を記録する。それが取れれば上の表のどこが
違うのかが即座に分かる。

## 19. サーバー内計測（javaagent）: 世界は chunk (1,1) を 1 セルも掘っていない

`tools/carve-agent/` に javaagent を追加した。`applyCarvers` の
(source chunk, carver index, isStartChunk) と、`WorldCarver.getCarveState` の
戻り値（null か state id か）をサーバー内で直接記録する。fixture の出力は
エージェント有無で不変（617 セルのまま）なので、計測は生成を乱さない。

実測（cave のみ、2x2 chunk）:

- `applyCarvers` 呼び出し 104907 行、`isStartChunk=true` 4800。
- **target (1,1) の start は 36、すべて k=0**（cave）。我々のモデルと一致。
- ところが **chunk (1,1) 内の `getCarveState` 呼び出しは 0 件**。
  `ASTATE` は 76 chunk をカバーするが (1,1) は含まれない。

つまり world では cave の 36 start が **chunk (1,1) に 1 セルも届いていない**。
こちらの port（と probe）は同じ 36 start から 401 セルを (1,1) に掘る。

start は一致しているので、**ずれは `carve` の中（乱数消費かパラメータ）**。
aquifer ではない。§17/§18 の「aquifer だけ」も、この意味で修正が必要。

次の一手: `ebb.carve` の入口で source chunk と最初の数ドロー
（y / yaw / pitch / thickness）を記録し、こちらの `CARVE_RNG` トレースと
並べる。最初に食い違うドローがそのままバグの位置になる。

## 20. javaagent でターゲット付き計測 → ずれの正体は「ターゲット外セルの地形読み」

エージェントに target chunk を記録させ（`applyCarvers` 入口で thread-local に
設定）、`ASTATE t=<target> s=<source> x y z <id>` の形で出るようにした。

cave のみ・target (0,0) の実測:

```
target (0,0) で getCarveState を呼んだ source     : 1 個だけ (1,-3)
その source が (0,0) に書いた非 null セル         : 20（先頭 (0,34,0)）
```

こちらの port は target (0,0) に対して **0 セル**しか書かない（同じ 33 start、
同じ seed で）。`CV_TRACE` で我々の source (1,-3) のトンネル開始位置を見ると
(20,84,-46) / (20,122,-36) / (23,33,-36) で、3 本目が vanilla の書いた
y≈34 と整合する。つまり **start は同じで、そこから先の書き込みが起きていない**。

原因は `carve_block` の読み:

```rust
fn read(&self, g, cc, x, y, z) -> u32 {
    match self.out.get(&((x & 15), y, (z & 15))) {
        Some(&st) => st,
        None => g.terrain_cell(cc, x, y, z),   // cc = *target* の ChunkCols
    }
}
```

`terrain_cell` は `block_density(cc, x, y, z)` を **`x & 15` で折り畳んで**
評価するので、ターゲット chunk の外にあるセルは「隣 chunk の地形」ではなく
「ターゲット chunk の同じローカル座標の地形」で判定される。vanilla は
`chunk.getBlockState(pos)` で**隣 protochunk の実際の地形**を読み、書き込みだけを
`x & 15` で折り畳む。この差で、遠い source chunk からのトンネルは
`canReplaceBlock` の判定が食い違い、書くセル集合が変わる。

§16〜§18 で「aquifer だけ」「world の carve が再現できない」と見えていたものの
主因はこれで、aquifer は二次的。`MC_SPINE_NOFOLD=1` で折り畳みを切る実験口も
入れたが、この読みの問題があるため差は変わらない（617 のまま）。

**次の一手**: `read` を「隣 chunk の ChunkCols で terrain_cell を評価する」形に
する（`g.cols(x >> 4, z >> 4)` で列を取得）。性能は carve ループ内なので、
隣 chunk の列キャッシュが要る。

## 21. §20 の訂正: ずれは「y のサンプル」にある

§20 で「ターゲット外セルの地形読み」を原因と書いたが、次の 3 つを実測した結果
いずれも差分は **617 のまま**だった:

1. `MC_SPINE_NOFOLD=1`（書きの折り畳みを切る）
2. `read` を隣 chunk の ChunkCols で評価（vanilla は折り畳んで読むので、これは
   そもそも bytecodeと違う。撤回済み）
3. `ellipsoid` の距離 bail-out を削除

決定的だったのは y の分布。エージェントの `ASTATE t=... s=1,-3 ...` を集計すると、
**source (1,-3) が実際に彫ったセルは y=28..45 に集中し、y>70 は 0 個**。

一方こちらの `CV_TRACE` は source (1,-3) のトンネル開始を
(20,84,-46) / (20,122,-36) / (23,33,-36) と出している。3 本のうち 2 本が
y=84/122 で、vanilla の分布と合わない。

→ **`cfg.y.sample(random, ctx)` の結果が vanilla と違う**（同じ seed・同じ
config JSON なのに）。`above_bottom 8` の解決（-56 のはず）か、uniform height
provider のサンプル方法（nextInt の回数/範囲）が疑わしい。ここが直れば
トンネルの向き・到達 chunk が変わり、§16〜§20 で見えていた残差の大半が
説明できる可能性が高い。

次の一手: エージェントで `ebb.carve` の y ドロー直後の値を記録する（または
`ehx`/`ehv` の `sample` を計測し、`above_bottom`/`absolute` の解決値と
`nextInt` の引数を実測する）。こちらの `c.y.sample` の実装
（`p_anchor` → `c.y.resolve(min_y, height)`）と突き合わせる。

## 22. §21 の訂正: y サンプルは一致していた

`ehv`（UniformHeight）の bytecode を読み直した:

```java
int lo = min.resolveY(ctx), hi = max.resolveY(ctx);   // -56, 180
if (hi - lo - inner + 1 <= 0) return lo;              // inner default = 1
int k = random.nextInt(hi - lo - inner + 1);          // nextInt(236)
return random.nextInt(k + inner) + lo;                // nextInt(k+1) + lo
```

`inner` の既定値は codec の `optionalFieldOf("inner", 1)` で **1**。こちらの
`HProv::Uniform`（`next_bound(hi-lo)` → `next_bound(k+1) + lo`）と**一致**する。

§21 の「y が違う」に見えたのは錯覚で、**y=84/122 のトンネルは空を掘るので
`canReplaceBlock` が false になり ASTATE に出ない**だけだった。実サーバーの
y=28..45 は 3 本目（y≈33）のトンネルの分。

よって残る差は 3 本目（source (1,-3), start (23,33,-36)）が **target (0,0) に
届くかどうか**。vanilla は (0,34,0) を彫るが、こちらは同じ source から
1 セルも書かない。候補は yaw/pitch の以後のドロー、`count`（トンネル長）、
`can_reach` の打ち切り位置、ellipsoid のクリップ。

次の一手: `ebb.carve` の 3 本目のトンネルで、yaw/pitch/count/seed を
エージェントから記録し、こちらの `CARVE_RNG`（`RNG <method> <value>`）と
先頭から並べる。最初に食い違う値がそのままバグの位置になる。

## 23. トンネル単位の実測: seed と y/z が違う

エージェントに `ebb`（CaveWorldCarver）の `carveTunnel` フックを足した。
descriptor は `(Leaz;Leba;Lduy;Ljava/util/function/Function;JLdyk;DDDDDFFFIIDLdux;Lebe$a;)V`
（ジェネリクス `C` の消去が `ebe` では `eax`、`ebb` では `eba` になる点に注意）。
ローカルは seed=5-6, x/y/z=8..17, yScale/yaw/pitch=18/19/20, branchIndex/Count=21/22。

4 chunk の固定具で **10,475 回**の carveTunnel 呼び出しを記録。source (1,-3) の
最初のトンネルを比べると:

```
実サーバー: seed=1754441436904522573  x=20 y=34  z=-41 yaw=2.4176 pitch=0.0605 count=86
こちらの port: seed=138348582335151166  x=20 y=84  z=-46 yaw=4.6633 pitch=-0.0023 count=92
```

**x だけ一致し、y・z・seed・yaw・count がすべて違う。** トンネル開始前のドローは
「tunnels の nextInt 連鎖3回 + x の nextInt(16)」だけなので、`x` が同じで `y` が
違うのは、**y のサンプル（`nextInt` 2回）でストリームがずれている**ことを意味する。
`UniformHeight` の `inner` 既定は 1（codec の `optionalFieldOf("inner", 1)`）で
bound は `hi - lo - inner + 1 = 236`、こちらも同じ 236 を使っているので、bound の
差ではなく**直前のストリーム**が違う。

未解決の観測: 同じ `s=1,-3` でも target が違うと ATUN の y/z が変わる
（(20,34,-41) と (16,101,-40) が混在）。seed は `(worldSeed, k, sourceX, sourceZ)`
だけで決まるので target 非依存のはずで、`s=` の attribution（thread-local の
設定/解除）を疑う必要がある。ここを確定してから「seed が違う」と結論する。

次の一手: プローブの DRAW ログ（`-Donly=1,-3 -Dleg=1`）と本 port の
`CARVE_RNG=1,-3` トレースを先頭 10 ドローで並べる。プローブの DRAW ゲートは
`-Donly` 指定時にも出るよう修正済み（この commit）。ただしプローブ実行は
まだ bootstrap で落ちており（`starts=` まで到達しない）、そこを直す必要がある。

## 24. probe と実サーバーが同じ seed で違う → ドロー列をサーバー側で取るしかない

probe（vanilla のクラスを直接呼ぶ）に `-Donly=1,-3 -Dleg=1` を付けて、source
(1,-3) の最初のトンネルのドロー列を取った（`tools/vanilla-probe/CarverProbe.java`
の DRAW ゲートを `-Donly` 指定時にも出るよう修正）:

```
i()      -> 0.12260747      // isStartChunk: 0.1226 < 0.15 → start
a(15)    -> 7               // inner
a(8)     -> 7               // mid
a(8)     -> 3               // tunnels = 3
a(16)    -> 4               // x = 16 + 4 = 20      ← 実サーバーと一致
a(236)   -> 156             // y 1回目 (hi-lo-inner+1 = 236)
a(157)   -> 140             // y 2回目 → y = 140 - 56 = 84
a(16)    -> 2               // z = -48 + 2 = -46
```

つまり probe（＝こちらの port）は **(20, 84, -46)**。実サーバーの ATUN は
**(20, 34, -41)** で、`y=34` は `nextInt(91)=90` 相当、`z=-41` は
`nextInt(16)=9` 相当。初期 seed を 0/12345/999 に振っても probe の値は不変
（`setLargeFeatureSeed` が完全に再 seed するため）なので、**同じ
`setLargeFeatureSeed(0, 1, -3)` から出発しているのに 2 回目の y ドローで
食い違う**。

probe と実サーバーは同じ `dzx.c(JII)`（`WorldgenRandom` の継承メソッド）を
呼んでいるはずで、そこが食い違うなら**推測をやめてサーバー内で生ドローを取る**
しかない。次の一手は `ayw` をエージェントでプロキシし、`applyCarvers` が
作る `WorldgenRandom` の `nextInt/nextLong/nextFloat` を先頭 10 回ぶん記録して、
probe の DRAW 列と直接 diff すること。

（なお実サーバーの ATUN は source あたり 5 種類のトンネルが各 121 回出る。
5 = 1 start の tunnels(3) + room 分岐(2)、121 = その source が start になる
target の数、で説明がつくので attribution 自体は正しい。）

## 25. 原因: `minecraft:uniform` の height provider は 1 回しか引かない

サーバーの生ドローを取るため、エージェントに

- `dzx.c(JII)` (`setLargeFeatureSeed`) の reseed を記録
- `dyz.c(I)` = `dyn` の `next(bits)` — **全ドローの唯一の関所** — の戻り値を 16 回ぶん記録

を足し、port 側にも `CARVE_RAW=<cx>,<cz>`（reseed 後の `nx(bits)` を 16 回）を入れて
同じ生値を出した。source (1,-3), k=0, seed=0:

```
server: 0:24->2057012 1:31->297986332 2:31->1934649034 3:31->896980054
        4:31->545566709 5:31->436620924 6:31->1021188899 7:24->2285188 ...
port  : 4:24->2057012 5:31->297986332 ...   (先頭4つは seed 混合の nextLong)
```

7 ドロー目まで完全一致。その次に server は **float**、port は int (z) を引く。

`ebb.a(...)` の bytecode の順序は
`x=nextInt(16) → y=cfg.e.a(random,ctx) → z=nextInt(16) → hrm → vrm → floor → nextInt(4)`
で port の順序は正しい。ずれは **y のドロー数**だった:

- `ehv`（3 フィールド = trapezoid）は `nextInt(range-plateau+1)` と `nextInt(k+plateau)` の 2 回
- 1.21.1 の `minecraft:uniform` は **`eia`** で、`Mth.randomBetweenInclusive`
  （`ayo.b`）= `nextInt(max-min+1)` の **1 回**。`i > j` なら warn して min を返す

server の 5 番目 `436620924 % 237 = 90` → `y = 90 - 56 = 34` ✓ ATUN の y=34、
6 番目 `1021188899` → `nextInt(16) = 7` → `z = -41` ✓ ATUN の z=-41。**ATUN は正しかった。**

port は probe が `ehv`（trapezoid, plateau=1）で組んでいたのに引きずられ、2 回引いていた。
draw が 1 つずれると yaw/pitch/yScale/count/seed が全部ずれ、トンネルが別物になる。

### 修正と結果

`HProv::Uniform` を `lo + next_bound(hi - lo + 1)` の 1 ドローに修正:

| 固定具 | 修正前 | 修正後 |
|---|---|---|
| cave only 2×2 (0,0) | 617 | **0** |
| 全 carver 2×2 (0,0) | 617+341 | **0** |
| 8×8 spawn 周辺（比較 42 chunk） | — | 52（3 chunk、`deep` 層、state 1↔2） |

8×8 の残差は spawn 準備の partially generated chunk 22 個を除いた領域に出ており、
carver の出力（air を掘る）ではなく state 1↔2 の入れ替わりなので別件として追う。

probe (`CarverProbe`) は今も `ehv` で組んでいるので `eia`/`ayo.b` に直す必要がある（TODO）。

### 25.1 他の carver と probe の修正

probe を `eia.a(...)` に直したところ、サーバーの生ストリームと完全一致した:

```
a(15)  -> 7      a(8) -> 7      a(8) -> 3
a(16)  -> 4      // x = 20
a(237) -> 90     // y = 90 - 56 = 34   ← ATUN と一致
a(16)  -> 7      // z = -48 + 7 = -41  ← ATUN と一致
i()    -> 0.13620782   i() -> 0.07097554
```

固定具の結果（すべて 2×2 at (0,0)、band y=-64..319）:

| 固定具 | 結果 |
|---|---|
| cave only | **0** |
| cave_extra only | **0** |
| canyon（jar の probability 0.01） | 0（この 4 chunk では start が無く、空の比較） |
| canyon probability 1.0（強制 start） | **1** セル（`deep`、got air / want `stripped_mangrove_wood`） |
| 全 carver | **0** |

canyon の JSON は `debug_settings`（air_state = warped_button）を持っており、
port はそれを含めて一致している（4 chunk を canyon で埋めて 1 セルだけ違う）。

## 26. 広い領域の残差を分解した

2×2 の固定具は 0 になったが、8×8（spawn 周辺、比較 42 chunk）では 52 セル残る。
`tools/carver_diff.py --cells N` を足して座標とブロック名を出すと、2 種類に分かれた:

| セル | 中身 | 層 |
|---|---|---|
| 48 | granite / stone / copper_ore の入れ替わり（(‑36,27..32,61..63) 付近） | 地形（R1a の ore vein） |
| 4 | `air` vs `water`、y=55、(‑38..‑37, 13..16) | carver/aquifer |

48 セルの方は carver ではない。`MC_SPINE_NOFEAT=1` でも `MC_SPINE_NOCARVE=1` でも
chunk (‑3,3) の granite は 48 セルのまま（features on だと 1510）で、vanilla 側も
features を空にした world で 44 セル持っている。つまり両者とも地形段階で同じ
大きさの granite 塊を作っており、位置が数十セルずれる = TODO の R1a（ore vein）。

4 セルは chunk (‑3,0) を単独で固定具にしても **4 セルで再現**する（t16）。すべて
`got air / want water`、y=55 の水平なパッチで z=15/16 の chunk 境界をまたぐ形。
aquifer の fluid level 判定が境界で 1 違う類なので、次は `AQ_TRACE=-3,0` と
`CARVE_PROBE=-3,0`（実地形 dump を `-Dterrain=` で食わせる）で 4 セルを見る。

canyon を probability 1.0 にしたときの 1 セルも同じ `air` vs `water` だった
（§25.1 で `stripped_mangrove_wood` と書いたのは誤り — `assets/blocks_1211.json`
をリストの添字で引いていた。state id → 名前は `minStateId..maxStateId` の範囲で
引く必要があり、`carver_diff.py --cells` はそうしている。id 81 は water）。

## 27. 残り 4 セルは worldgen ではなく水の流れだった

§26 の 4 セル（`air` vs `water`、y=55）を掘った結果:

- こちらの dump は (-37,55,13) に **level 0 の水**、(-38,55,13..15) は air
- vanilla の chunk palette には water **level 0 が 2 個**、**level 1 が 1 個**
- chunk には structure の start が無い（`structures.starts` が空）ので構造物ではない

つまり vanilla 側だけ、湧き水の隣接セルへ **水が流れた**（level 1 = 流動）。
spawn 周辺は forceload されて tick するので、サーバーが動いている間に流体が広がる。
こちらの port は tick しないので air のまま。**worldgen の差ではない。**

canyon を probability 1.0 にしたときの 1 セルも同じ `air` vs water（level 付き）で、
同じく流動。したがって carver の world 比較は、地形層の ore vein 残差（R1a）を
除いて **0 セル**。

教訓: 保存済み world は純粋な worldgen の産物ではない。spawn/forceload された
chunk は tick するので、`air` vs `water[level>0]` や `water[level=0]` の隣接は
worldgen の差ではなく流体伝播として除外して読む（`--cells` で座標と名前を見る）。

## 28. R1a（granite/copper の 48 セル）の正体: 1.21.1 の ore vein は地形段階

chunk (‑3,3) の granite を集合で比べた:

| | セル数 |
|---|---|
| こちら（`MC_SPINE_NOFEAT=1`） | 48 |
| vanilla（features 空） | 44 |
| 共通 | **42** |
| こちらだけ | 6 |
| vanilla だけ | 2 |

bbox は完全一致（x‑39..‑33 / y26..32 / z61..63）。つまり**同じ鉱脈をほぼ同じ形で
作っていて、境界の数セルだけ違う**。

なぜ features を切っても両側に granite が残るのか:

- 1.21.1 の `noise_settings/overworld.json` には **`ore_veins_enabled: true`** と
  router の **`vein_toggle` / `vein_ridged` / `vein_gap`** があり、鉱脈は
  **地形（NoiseChunk）段階**で生成される。`material_rule` ディレクトリは存在しない
  （jar 内 0 件）
- リポジトリの `datapacks/vanilla` は 1.21.2+ で、鉱脈は
  `material_rule/overworld/copper_ore_vein.json`（`type: minecraft:ore_vein`,
  `density`/`richness`/`filler_gap` を名前付き density function で参照）にある
- こちらは後者を `Rule::OreVein` として実装しているので、**features を切っても
  地形段階で鉱脈が出る**（両側とも出る）✓

残る 6/2 セルの差は実装の境界条件。port の評価は

```rust
if d <= 0.0 { return None; }
let mut r = sp.pos(x, y, z);
if r.next_f32() > d as f32 { return None; }   // ← density を確率として使っている
```

と、density を**確率ゲート**に使っている。1.21.1 の sampler は toggle/ridged/gap の
しきい値で決める決定的な判定なので、ここが「ほぼ同じだが数セル違う」形の原因と
みられる（1.21.2 の `ore_vein` ルールの正しい意味も要確認）。

**したがって R1a の恒久修正は「データを 1.21.1 jar から再生成 + 1.21.1 の
OreVeinSampler を実装」**であり、`Rule::OreVein` の微修正では終わらない。

## 29. 1.21.1 の `OreVeinSampler` を特定して実装した（既定では未使用）

`NoiseChunk` (`dzc`) のコンストラクタが

```java
if (settings.oreVeinsEnabled())
    list.add(OreVeinSampler.create(router.m(), router.n(), router.o(), randomState.e()));
```

と `dzi.a(Ldyr;Ldyr;Ldyr;Ldzl;)` を呼ぶ。`dzi` が 1.21.1 の **OreVeinSampler** で、
bytecode から読んだ本体は:

```
t = vein_toggle(ctx);  type = t > 0 ? COPPER : IRON
y が type の帯の外                          -> なし
|t| + map(min(up,down), 0, 20, -0.2, 0) < 0.4 -> なし
r = RandomState.at(x,y,z); r.nextFloat() > 0.7 -> なし
vein_ridged(ctx) >= 0                        -> なし
richness = map(|t|, 0.4, 0.6, 0.1, 0.3)
r.nextFloat() < richness && vein_gap(ctx) > -0.3
    ? (r.nextFloat() < 0.02 ? raw_ore : ore) : filler
```

`dzi$a` の enum 定数は **COPPER: y 0..50, copper_ore / raw_copper_block / granite**、
**IRON: y -60..-8, deepslate_iron_ore / raw_iron_block / tuff**（`dga` の static init
から名前を解決）。ランダムは `RandomState.at` = seed source の `forkPositional()`
（aquifer のサンプル位置で検証済みの Xoroshiro）。`eie`（MaterialRule のリスト）は
**最初の非 null** を返すので、鉱脈は地形より前に評価される = 地形を上書きする。

実装は `Gen::vein_override`（`MC_SPINE_OREVEIN=sampler` のときだけ有効）。データは
1.21.1 の router 3 関数 + `ore_veins_enabled` を
`datapacks/vanilla/.../noise_settings/overworld.json` に追加した（port のパーサは
1.21.2 の綴り `input` / `left` / `right` を期待するので、`argument` /
`argument1` / `argument2` を正規化して入れた）。

### 実測（chunk (‑3,3)、features off）

| 実装 | granite | vanilla と共通 | こちらだけ | vanilla だけ | chunk の全差 |
|---|---|---|---|---|---|
| 1.21.2 material rule（既定） | 48 | 42 | 6 | 2 | **10** |
| sampler（Xoroshiro pos） | 29 | 22 | 7 | 22 | 30 |
| sampler（LCG pos = `dyz$a`） | 34 | 26 | 8 | 18 | 28 |

補間は効いている: (‑36,27,62) で `t=0.4001`（グリッド補間）vs `0.3997`（直接評価）。
鉱脈は **0.4 のしきい値**の上に乗っているので、補間が 1e-4 違うだけでセルが反転する。

sampler の制御フローは bytecode と一致しているのに配置が少ないので、次は
`t` / `ridged` / `gap` の**値そのもの**を vanilla 側で測る（javaagent で
`dyr.a(dyr$b)` を評価して座標指定で出力する）必要がある。既定は依然
material rule（差 10 セル）で、sampler は opt-in のまま。

**ハーネスの罠**: `tools/carver_fixture.sh` の `trap restore_repo EXIT` は
`git checkout -- datapacks/` を実行するので、**未コミットの datapack 変更は
固定具を 1 回流しただけで消える**（今回それで `ore_veins_enabled` が消え、
sampler が `vein=None` になって既定と同じ 48 を出した）。データ変更は
コミットしてから測る。

## 30. 解決: 公式 mappings + 逆コンパイルで最後の 1 点 = "ore" positional factory

Mojang は 1.21.1 の server mappings を公開している
（`piston-data.mojang.com/v1/objects/03f898.../server.txt`、7.4 MB）。
これを Vineflower 1.11.1 で当てて `dzi` を逆コンパイルすると `OreVeinifier` で、
**bytecode から読んだ擬似コードと行単位で一致**した（定数 0.4 / 20 / −0.2 / 0.7 /
0.1 / 0.3 / 0.6 / 0.02 / −0.3 も一致）。mappings で `dzi = OreVeinifier`、
`dzi$a = VeinType`、`dze = NoiseRouter`（`veinToggle = n` / `veinRidged = o` /
`veinGap = p`）も確定。

残りの不一致は **乱数**だった。`RandomState` (`dzm`) の逆コンパイル:

```java
this.a = settings.getRandomSource().newInstance(seed).forkPositional();
this.f = this.a.fromHashOf(ResourceLocation("aquifer")).forkPositional();  // d()
this.g = this.a.fromHashOf(ResourceLocation("ore")).forkPositional();      // e()
```

`NoiseChunk` は `OreVeinifier.create(toggle, ridged, gap, randomState.e())` と、
**"ore" から派生した positional factory** を渡す。こちらは base factory を使っていた。

決め手はエージェントで `OreVeinifier` の lambda をフックして 3 つの DF 値を実測した
こと（`-Dveindump=x,y,z`）。(-36,27,62) で

```
vanilla: toggle=0.4000649451503646 ridged=-0.02908584499032882 gap=-0.2872244962433405
こちら:  toggle=0.4000649451503646 ridged=-0.029085844990328805 gap=-0.2872244962433405
```

と**入力は完全一致**（ridge は 1 ULP 差）→ 疑うべきは乱数だけ、と絞れた。

`self.factory.of_str("minecraft:ore").splitter().pos(x, y, z)` に直した結果:

| 固定具 | 修正前 | 修正後 |
|---|---|---|
| chunk (-3,3) 単体 | 10 | **0** |
| (-3,3) 2×2 | 48 | **0** |
| 8×8 spawn 周辺（比較 42 chunk） | 52 | **4**（すべて §27 の tick する水） |
| cave only 2×2 (0,0) | 0 | **0** |

これで **world 比較の残差は「サーバーが tick した水」だけ**。1.21.2 の
`material_rule/overworld/copper_ore_vein.json` 側は、sampler が有効なとき
`Rule::OreVein` を解決しないことで無効化している。

### 追加した計測器

- `tools/carve-agent`: `dzi` の lambda をフックし `-Dveindump=x,y,z` で
  toggle/ridged/gap を出力（`VDUMP`）。クラッシュ時の `VerifyError` は
  `vdump(IIIDDD)` の引数順（double を先に積んでいた）が原因だった。
- port 側 `MC_SPINE_DFDUMP=x,y,z` → `DFDUMP ... final=.. vein=Some((t,r,g))`。
- `~/.cache/mc-spine-work/mc121/` に server.txt と vineflower を置き、
  `java -jar vineflower-1.11.1.jar cls out` で任意クラスを読めるようにした。

## 31. feature 配置の乱数は「legacy」だった（`WorldgenRandom` の 2 つの顔）

地形と carver が 0 になったので feature 層（40975 / 4 chunk ≒ 10.4%/chunk）に着手。
公式 mappings で `OreFeature` (`ecz`) と `OreConfiguration` (`eeq`) を逆コンパイルすると、
配置ロジックはこちらの実装と一致していた（`size/8`、`ceil((size/16*2+1)/2)`、
`nextDouble()*size/16`、`(sin+1)*d+1)/2`、さらに `BitSet` の cell 訪問）。

ずれは**乱数**だった。`WorldgenRandom` (`dzx`) は Xoroshiro を包むが、
`dyn`（RandomSource）の **legacy な draw** を実装している:

```java
public int c(int bits) { count++; return d instanceof dyz l ? l.c(bits) : (int)(d.g() >>> 64 - bits); }
// dyn の default: nextInt(bound) = (bound * next(31)) >> 31 / rejection, nextInt() = next(32),
//                nextDouble() = ((next(26)<<27)+next(27))*2^-53, nextBoolean() = next(1)
```

こちらは `Xor` に modern `RandomGenerator` の形（low word の Lemire）を実装していたため、
**ore の origin が 242/242 すべて違っていた**（個数と size 分布は完全一致）。

計測は 3 つで足りた:

- エージェント: `WorldgenRandom.setDecorationSeed`/`setFeatureSeed` をフックして
  `FSEED <decoSeed> <i> <step>`、`OreFeature.place` で `ORE x y z size`、
  `XoroshiroRandomSource` の生 draw は `XDRAW`（`c(int)` が無いので `a(I)I`/`f()I`）
- port: `FSEED_TRACE` / `ORE_TRACE` / `FSEED_ARM=cx,cz,i,step` で同じ値を出力
- `FSEED` の列は step 6（underground_ores）で**完全一致** → seed は正しい、と切り分けられた

`Xor` に `seed_worldgen()`（legacy フラグ）を足して feature 配置だけ legacy に切り替えると:

| | 前 | 後 |
|---|---|---|
| ore origin 一致 | 0 / 242 | 37 / 242（最初の 3 つは完全一致） |
| feature あり 2×2 diff | 40975 | **34679** |
| chunk (0,0) 単体 | 10177 | **4908** |

残りは feature ごとの差（`FSEED`→`ORE` の対応付けは「配置 0 個の feature」で
ずれるので、次は ORE ログに feature id を付けて比較する）。biome の feature リストも
step 2/9/10 で差が出ている（1.21.2+ データの残り）。

## 32. feature の decoration seed は Xoroshiro + XOR だった

ore の配置が chunk (0,0) で完全一致したのに、隣の chunk (-1,0) では 242/242 ずれた。
`applyBiomeDecoration` を逆コンパイルすると:

```java
dzx rand = new dzx(new dzz(dzn.a()));            // WorldgenRandom wrapping Xoroshiro
long decoSeed = rand.a(level.getSeed(), origin.getX(), origin.getZ());  // setDecorationSeed
```

`setDecorationSeed`（`dzx.a(JII)J`）の bytecode は

```
setSeed(seed); l = nextLong() | 1; m = nextLong() | 1;
n = (x * l) ^ (z * m) ^ seed;      // ← lxor（加算ではない）
```

で、`l`/`m` は **Xoroshiro** の nextLong（`WorldgenRandom` 経由なので legacy 形）。
こちらは `Lcg` と `+` で計算していた。chunk (0,0) だけは `0*l ^ 0*m ^ seed = seed`
なので偶然一致していた（＝ 4 chunk 中 3 chunk の feature が全部ずれていた）。

修正後: 隣 chunk (-1,0) の decoration seed が vanilla と一致
（`6716476193438028432`）、ore feature 21/22 が一致、feature あり 2×2 の diff は
**33853 → 15820**（chunk (0,0) 単体 3992 → 1218）。
