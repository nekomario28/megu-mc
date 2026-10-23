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

### 32.1 続き: 掛け算は「加算」で合成する（XOR ではない）

`setDecorationSeed` の decompiled は

```java
long $$5 = $$1 * $$3 + $$2 * $$4 ^ $$0;    // (x*l) + (z*m) してから seed と XOR
```

で、`+` と `^` の順序が肝。chunk (-1,0) は z=0 なので `(x*l + 0) ^ 0 = x*l` と
XOR 形式でも同じ値になり、**両者を区別できない**。chunk (1,1)（x=z=16）で初めて差が出る:

| 式 | 値 |
|---|---|
| `(16*l) ^ (16*m)` | 11973949894776616928 |
| `(16*l) + (16*m)` | **12083320522243344384** = vanilla |

修正後: 全 4 chunk で decoration seed が一致、ore feature は 22/22（chunk (1,1)）、
feature あり 2×2 の diff は 15820 → **3472**（1 chunk あたり ~870 = 0.9%）。

残差 3472 の内訳: `magma_block`（1236、vanilla は 0 = `underwater_magma` を
置きすぎ）、木（leaves/log 747、置き漏れ）、`dirt != sand/gravel`（579、disk 系）、
`diorite != water` など。

## 33. `underwater_magma` は「水面下」でしか置かない

残差 3472 のうち最大の 1236 セルが `magma_block`（vanilla は 0）。
`UnderwaterMagmaFeature`（`eds`）を逆コンパイルすると:

```java
OptionalInt floor = Column.scan(level, origin, cfg.floorSearchRange,
                                s -> s.is(WATER), s -> !s.is(WATER)).map(Column::getFloor);
```

つまり **origin のセルが水であること**が前提で、水を下に辿って最初の非水セルが床。
こちらの実装はこの前提を持たず、陸地でも「下が水/空気でなくなるまで降りて」置いていた。

さらに配置は

```java
BlockPos.betweenClosed(base.offset(-r,-r,-r), base.offset(r,r,r))
    .filter(p -> random.nextFloat() < cfg.placementProbability)
    .filter(p -> isWaterSatisfied(level, p))     // 自身/上/6近傍が水でも空気でもない
    .forEach(p -> setBlock(p, MAGMA));
```

で、**箱の全セルで nextFloat を引いてから** 包囲判定。修正後は陸地で 0 セルになり、
feature あり 2×2 の diff は 3472 → **2188**（1 chunk あたり ~550 = 0.56%）。

## 34. 木が 1 本も生えない理由: placed_feature の版ずれ

feature ありの残差から木（leaves/log 約 750 セル）を調べると、chunk (0,0) の
step 9 で `trees_birch_and_oak` が **+0 writes** だった。biome の feature リストは
1.21.1 の jar から取り直してある（`forest` の step 9 に `trees_birch_and_oak`）が、

- 同梱の `placed_feature/` は 1.21.2+ の名前で、`trees_birch_and_oak.json` が無く
  `trees_birch_and_oak_leaf_litter.json` だけがあった
- したがって `fr.placed.get("minecraft:trees_birch_and_oak")` が None になり、
  **黙ってスキップ**されていた

`placed_feature/` を 1.21.1 の jar から上書き（234 ファイル）すると木が走り出す。
feature あり 2×2 の diff は 2188 → **2062**。

**ただし** `feature/`（configured feature）は 1.21.2+ の形のままで、1.21.1 の
`{"type":"minecraft:tree","config":{...}}` とは構造が違う（こちらは `config` 無しで
`foliage_provider` が `{"id":...}`）。1.21.1 の configured_feature をそのまま
上書きすると逆に 24063 まで悪化する（ore の `targets.state` が `{"Name":...}` 形式に
なるため）。木・disk を仕上げるには **configured_feature の 1.21.1 化 + パーサの
両対応**が必要で、これは次の作業単位。

## 35. feature の順序表が 53 biome だった（木が全部ずれていた真因）

木の trunk 位置を実サーバーから取り（`edq` = TreeFeature を実行時にフック、
`-Dtreechunk=cx,cz` は ±1 chunk を許容）、こちらの `TREE_TRACE` と比べると
38 本中 1 本しか一致しなかった。y は概ね合うのに (x,z) が違う。

決め手は **feature ごとの deco seed**。vanilla の `setFeatureSeed(seed, i, step)` は
`seed + i + 10000*step` なので、chunk (0,0)（deco seed 0）では *i そのもの*が出る。
両者を並べると:

| step | vanilla の i | こちら |
|---|---|---|
| 1, 3, 6, 8 | 一致 | 一致 |
| 2 | [2] | [3] |
| 9 | [0,17,20,48,49,51,60,63,64,71,72,73] | [2,4,28,31,36,41,49,68,69,80,81,93] |
| 10 | [0] | [1] |

step 6（ore）だけ一致するのは、**どの biome でも step 6 の並びが同一**なので
トポロジカルソートの結果が tie-break 順に依存しないため。step 2/9/10 は
biome ごとに並びが違うので、`FeatureSorter` の first-seen index（biome を
**どの順で**走査するか）に依存する。

こちらの `OW_BIOMES` は 64 件で nether / end / the_void を含んでいた。vanilla の
`FeatureSorter.buildFeaturesPerStep` が受け取るのは **biome source の
possibleBiomes（53 件、overworld のみ）**。`deg.a`（= buildFeaturesPerStep）の
第 1 引数をエージェントで出力して実測すると:

```
[Reference{.../minecraft:mushroom_fields}, .../deep_frozen_ocean, ...,
 .../jungle, .../badlands, .../desert]
```

この 53 件順で `OW_BIOMES` を置き換えると **全 step の i 集合が vanilla と一致**
（`trees_birch_and_oak` は 31 → **20**）。tree の origin 一致は 1/38 → 14/38 に増えた。
step 6 の不変性のおかげで ore は今まで正しく見えていた、という関係。

**教訓**: feature の RNG オフセットは「step 内の添字」で決まる。添字は
全 biome の並びから作るトポロジカルソートなので、**入力の biome 集合と順序**が
正しくないと、並びが同一の step（ore など）だけが正しく見える。

## 36. 木の残差は「disk 系 feature が先に置くブロック」の差だった

`OW_BIOMES` を実サーバーの 53 biome 順に直した後も、木の位置は 14/38 しか一致しない。
決め手は 2 つ:

1. **`InSquarePlacement` の生ログ**（`eis.a_` をフック、locals は 0=this/1=eiw/2=ayw/
   3=jd/4=x/5=z）。chunk (‑1,0) の feature (20,9) で vanilla の候補列は
   `(12,6),(10,4),(11,2),(12,6),(6,1),(10,2),(1,3),(6,8),(4,13),(13,11),(5,5)`
   （chunk 相対）、こちらは `(12,6),(5,8),(7,11),…` と 2 個目からずれる。
2. **`RandomSource` の default メソッド**（`dyn.a(I)I` / `dyn.i()F`）をフックした生ドロー列。
   vanilla は `nextInt(10)=9, nextInt(16)=12, nextInt(16)=6, nextFloat, nextFloat,
   nextInt(16)=10, nextInt(16)=4, …`。**2 個目の候補の前に float が 2 つだけで、
   int を 1 つも引いていない** = 1 個目の候補では木を生成していない。

`trees_birch_and_oak` は `random_selector`（chance 0.2 の birch / 0.1 の fancy oak、
default は `oak_bees_0002`）で、default の placed feature は
`block_predicate_filter(would_survive oak_sapling)` だけを持つ。つまり vanilla は
候補 (‑4,63,6) で **sapling が育たない**と判定して木を 1 つも生成していない。

理由は下のブロック。vanilla の最終世界では (‑4,62,6) は **gravel**、こちらは
**grass_block**。`is_dirtish(gravel)` は false なので vanilla は正しく拒否し、
こちらは「育てる」と判断して木を生成 → **消費ドロー数が変わり、以降の候補が
すべてずれる**（木 1 本の差が 11 候補全体を動かす）。

(‑4,62,6) の gravel は disk 系（`disk_gravel`）が置いたもので、同じ 2×2 の残差に
`dirt != sand` 411 / `dirt != gravel` 168 があることと一致する。**次の単位は
disk 系 feature（sand/clay/gravel）の実装**で、それが直れば木の位置も揃う見込み。

### 計測器（今回追加）

- `eis`（InSquarePlacement）の生 (x,z)、`eii`（BiomeFilter）の位置と可否、
  `dyn` の default ドロー（`nextInt(bound)`/`nextInt()`/`nextFloat`）、
  `deg.a` の biome リスト、`duz.a` の dimension（`LevelHeightAccessor.getMinBuildHeight`、
  ‑64 のみ overworld）。
- port 側: `TREE_TRACE` / `BIOMEF_TRACE` / `INSQ` 相当は無いが `FSEED_ARM` で
  `NDRAW`（next(bits)）/ `FDRAW`（nextFloat）/ `XDRAW`（nextInt(bound)）を出力。
- `PF_UNKNOWN=1`: 解析できず**黙って落ちる** placement modifier を表示
  （`fixed_placement` が見つかったので実装した）。

**注意**: `LevelHeightAccessor.getMinBuildHeight` の obf は `I_()`（`dcy`）。
nether/end は同じ chunk 座標・同じ level seed で装飾するので、
dimension で切らないと FSEED/BIOMEF のログが混ざる（今回それで 1 往復無駄にした）。

## 37. disk 系が「黙って 0 書き込み」だった 2 つの理由

`disk_sand` / `disk_gravel` / `disk_grass` が 1 セルも置いていなかった。原因は
**どちらも「解析に失敗したものが黙って落ちる」**型で、ログには `+0 writes` としか
出ない（＝「置くものが無かった」と見分けがつかない）。

1. **1.21.1 の rule-based state provider には `type` が無い**。形は
   `{"fallback": {...}, "rules": [...]}` で、こちらは `jstr(j,"type")?` で
   即 `None` → **feature 全体が登録されない**（`CF_MISSING=1` を足すまで見えなかった）。
   1.21.2+ は `"type": "minecraft:rule_based_state_provider"` を書くので気付きにくい。
2. **`"fluids": "minecraft:water"` は単一値**。こちらのパーサは
   `jarr(j,"fluids")` か `jstr(j,"fluid")` しか見ておらず、`fluids` が文字列だと
   `Pr::Fluids(vec![])` になり **全候補が棄却**（`disk_sand` の placement は
   `count 3 → in_square → heightmap(OCEAN_FLOOR_WG) → block_predicate_filter(matching_fluids water)`）。

修正後: feature あり 2×2 の diff は 2510 → **1856**、chunk (0,0) 単体は 540 → **140**。

残差の内訳は `oak_leaves != air` 541 / `air != oak_leaves` 303 /
**`oak_leaves != oak_leaves` 190**（＝同じ葉だが `distance` プロパティが違う）=
TreeFeature の `updateLeaves`（葉の distance 伝播）が未実装なのが次。

### 計測器（追加）

- `PF_UNKNOWN=1`: 解析できず落ちた placement modifier を表示（`fixed_placement` を発見）
- `CF_MISSING=1`: 名前で参照された configured feature が見つからないとき表示
  （今回の disk はこれで見つかった）
- `DISK_TRACE=1`: disk の判定を列単位で表示（追加したが、今回は不要だった）

## 38. 葉の `distance` は worldgen の値ではない（tick が上書きする）

残差の内訳に `oak_leaves != oak_leaves` 190（同じ葉で `distance` だけ違う）があった。
調べると **こちらの値は常に vanilla より 1 小さい**。原因はアルゴリズムではなく
**段階**だった。

`LeavesBlock`（`dki`）の逆コンパイル:

```java
protected BlockState updateShape(state, dir, neighbour, level, pos, neighbourPos) {
   if (state.getValue(PERSISTENT)) level.scheduleTick(pos, WATER, ...);
   int i = getDistanceAt(neighbour) + 1;
   if (i != 1 || state.getValue(DISTANCE) != i) level.scheduleTick(pos, this, 1);
   return state;                    // ← state は変えない
}
protected void tick(state, level, pos, random) {
   level.setBlock(pos, updateDistance(state, level, pos), 3);   // ← ここで初めて distance を書く
}
```

`Block.updateFromNeighbourShapes`（`TreeFeature.place` の最後）は上の `updateShape` を
境界面ごとに呼ぶだけで、**返り値を代入するのは「変わったとき」だが `updateShape` は
state を変えない**（＝実質 `scheduleTick` だけ）。したがって保存済み world の
`distance` は **スケジュールされたブロック tick が連鎖して書いた値**であり、
worldgen の出力ではない。TreeFeature 内の BFS（`updateLeaves`）が書くのは
初期値にすぎず、tick が上書きする。

実測でも裏が取れる: 保存済み world の oak_leaves 590 セルのうち **166 (28%) が
「distance == min(近傍)+1」を満たさない**（＝まだ収束していない）。

さらに `randomTick` は `!persistent && distance == 7` の葉を**消す**ので、
forceload された chunk では樹冠が欠ける（水の流れと同じ種類の tick 由来の差）。

したがって:
- `oak_leaves != oak_leaves`（distance のみの差）は **worldgen の残差ではない**ので
  除外して数える（水の `air vs water[level>0]` と同じ扱い）。
- ただし「こちらに葉があり vanilla が air」の 578 セルの距離分布は 1〜3 が大半
  （d=1:135 / d=2:237 / d=3:168 / d=4:36 / d=7:2）で、距離 7 の落葉では説明できない。
  これは**樹冠の形（幾何）の差**なので残差として追う価値がある。

**教訓**: 保存済み world は worldgen の出力ではない。tick を持つブロック
（水・葉・作物・流体）は「worldgen 段階の値」と「tick 後の値」が別物で、
固定具の world は後者を保持している。差を数える前に「これはどちらの段階か」を
確かめる（葉は `updateShape` が state を返さないことが根拠になる）。

## 39. 入れ子の「placed feature 参照」が modifier を落としていた（木の真因）

`random_selector` / `simple_random_selector` / `weighted_random_selector` /
`sequence` が保持するのは **`Holder<PlacedFeature>`** であって configured feature
ではない。したがって参照先の *placement modifier* も、同じ RNG ストリームで
実行される。こちらの `pf_any(J::Str)` は

```rust
J::Str(s) => Some(Pf { feat: FRef::Name(ns_name(s)), mods: vec![], .. })
```

と、**modifier を空にしていた**。その結果:

- `trees_birch_and_oak`（random_selector）の子 `birch_bees_0002` /
  `fancy_oak_bees_0002` / default `oak_bees_0002` の
  `block_predicate_filter(would_survive oak_sapling)` が実行されず、
- 候補ごとに必ず木を生成 → **消費ドロー数が増え、以降の候補位置がすべてずれる**
  （chunk (‑1,0) で 11 候補中 vanilla 6 本 / こちら 9 本）。

`FRef::Placed(String)` を足し、生成時に `fr.placed` から引いて `gen_pf` を呼ぶ
（＝子の modifier 連鎖を同じ RNG で実行する）ようにした結果:

| | 前 | 後 |
|---|---|---|
| 2×2 feature あり diff | 1856 | **1035** |
| tick 由来（葉の distance）を除く | 1613 | **655** |
| 木の本数（3×3、chunk (0,0) 周辺） | 53 | **45**（vanilla 38） |
| chunk (0,0) 単体 | 140 | **92** |

残りは「45 vs 38 の木」と carver 層。木は `would_survive` の残差
（`can_place` の dirt 判定 vs `BlockTags.DIRT` + farmland）を追う。

## 40. 未解決: vanilla の `BiomeFilter` が「biome は forest」な位置で false を返す

feature (20,9) = `trees_birch_and_oak` の木の本数は 45 vs 38 まで詰まった
（chunk (‑1,0) は 6 vs 6、chunk (‑1,‑1)/(0,‑1)/(1,‑1)/(1,1) も一致）。残る差を
追うと **矛盾する観測**に行き着いた:

- chunk (1,0)、候補 (20,66,3):
  - vanilla の `BiomeFilter`（`eii.a` をフック）: **false**
  - こちらの `PM::Biome`: **true**（biome=forest）
  - 保存済み world の quart (20,64,0) = **minecraft:forest**
    （`featworld` / `treeworld` の両方で同じ、palette は river 36 / forest 28）
- vanilla の `InSquarePlacement` 候補（chunk (1,0)、feature (20,9)）は 10 個、
  そのうち `BiomeFilter` が true を返したものは **0 個**。

`BiomeFilter` の逆コンパイルは

```java
eiv feature = ctx.e().orElseThrow(...);         // topFeature
jm<ddw> biome = ctx.d().t(pos);                 // level.getBiome(pos)
return ctx.f().a(biome).a(feature);             // biome の feature 集合が feature を含むか
```

で、`forest` の step 9 には `trees_birch_and_oak` が入っている（step 9 の添字集合は
一致済み）。したがって「保存済み biome = forest なのに false」は説明がついていない。

### ここまでに確定したこと（追加計測）

`eii.a` の中で `ctx.e()`（topFeature）を文字列化したところ:

```
BIOMEF3 20 66 3 false biome=err top=Optional[Placed Reference{ResourceKey[
  minecraft:worldgen/configured_feature / minecraft:trees_birch_and_oak]=Configured: ...}]
```

- **topFeature は `trees_birch_and_oak`**（child の `oak_bees_0002` ではない）。
- 同じ chunk (1,0) の biome フィルタ呼び出しは **31098 true / 1646 false**。
  つまり biome 参照は一般的には働いており、この 1 点だけが false。
- `BiomeFilter` の bytecode は
  `eiw.e()`（feature）→ `((dds) eiw.d()).t(pos)` → `duz.a(biome)` → `.a(feature)`。
  `arb`（WorldGenRegion）は `getBiome` を**実装していない**（`a(III)Ljm;` =
  `getNoiseBiome` だけ実装）ので、`t(pos)` は `LevelReader` の default で
  `getNoiseBiome(QuartPos.fromBlock(x), fromBlock(y), fromBlock(z))`
  = この位置では `(5, 16, 0)` に落ちる。

### 解決: vanilla の `getBiome(BlockPos)` は chunk の container ではない

`arb.a(III)Ljm;`（= `WorldGenRegion.getNoiseBiome`）をフックしても **1 行も出なかった**
（`-Dqbiome=all` で 0 件）。つまり decoration 中に `getNoiseBiome` は呼ばれていない。
`dcz`（LevelReader）の default を読むと:

```java
public default Holder<Biome> t(BlockPos pos) {     // getBiome(BlockPos)
   return this.F_().a(pos);                        // getBiomeManager().getBiome(pos)
}
```

`F_()` は `ddy`（`BiomeManager`）。つまり vanilla には **biome が 2 種類**ある:

1. **chunk の container**: quart セルごとに `fillBiomesFromNoise` が
   `biomeSource.getNoiseBiome(qx,qy,qz,sampler)` を直接書いたもの。保存された world に残り、
   こちらの `biome_quart` / `MC_SPINE_BIOME_DUMP` はこれを再現している（64/64 セル一致）。
2. **`BiomeManager.getBiome(BlockPos)`**: `pos − 2` の quart セルと小数部から
   **8 近傍の noise サンプルを `getFiddledDistance`（zoom seed を使う重み付き距離）で
   選ぶ**、いわゆる biome blend。`LevelReader.getBiome(BlockPos)` はこちら。

`BiomeFilter` は `ctx.getLevel().getBiome(pos)` = **2** を使う。こちらの `PM::Biome` は
**1** を使っている。境界（river/forest）では両者が食い違うので、chunk (1,0) の
(20,66,3) が「保存データは forest、フィルタは false」になったのは当然の結果だった。
木の不足（45 vs 38）はこの 1 点に集約される。

### 次の一単位（これで木が揃う見込み）

`BiomeManager.getBiome(BlockPos)` 相当を port に実装する:

- `ddy.a(Ljd;)Ljm;` と `getFiddledDistance` の bytecode（zoom seed・8 近傍・重み付き距離）を読む。
- `Gen` に `biome_at_blended(x,y,z)` を足し、`PM::Biome`（と `getBiome(BlockPos)` を使う
  他の消費者）をそちらへ向ける。chunk container を使う経路（保存・送信）はそのまま。
- 出口: chunk (1,0) の候補 (20,66,3) で判定が false になり、木が 45 → 38 に揃う。

候補の (x,z) が一致している以上、ここの解決で木は 45→38 に揃う見込み。

## 41. 残差の正体: 石系 ore は vanilla の**上位集合**で、差は後続の spring が置く水

2×2 の石系 ore（`ore_granite/diorite/andesite/tuff`）のセル集合を vanilla と比べると:

| ブロック | ours | vanilla | 共通 | ours-only | vanilla-only |
|---|---|---|---|---|---|
| diorite | 4251 | 4032 | 4032 | **219** | **0** |
| andesite | 5232 | 5112 | 5112 | **120** | **0** |
| granite | 4007 | 3960 | 3960 | **47** | **0** |
| tuff | 5568 | 5535 | 5535 | **33** | **0** |

**vanilla-only が全部 0、ours-only が計 419**。つまり鉱脈の位置・形は（origin も
セルも）vanilla を包含しており、差は「vanilla では後から別の feature がそのセルを
置き換えた」側だけ。最大のペア `diorite != water` 113 が典型で、vanilla は
step 8 の `spring_water` が diorite を水に置き換え、こちらは置き換えていない。

したがって次の単位は **spring 系（`spring_water` / `spring_lava`）**:
`SpringFeature` は面の条件（`rock_count` / `hole_count` / `valid_blocks`）で
「岩盤に開いた穴」を探して流体を置く。`diorite != water` 113 + `diorite != gravel` 87 +
`andesite != dirt` 80 などの大半がこの 1 機構の説明で片づく見込み。

（なお `juxtapose` の注意: この比較は feature ありの world なので、carver/aquifer 層は
別途 0 セルであることを確認済み — `tools/carver_fixture.sh tcur - - - 2` が 0。）

## 42. 残差の内訳（最終）と spring の向き

ブロック別の集合比較（2×2、feature あり）:

| ブロック | ours | vanilla | ours-only | vanilla-only |
|---|---|---|---|---|
| diorite | 4251 | 4032 | 219 | **0** |
| andesite | 5232 | 5112 | 120 | **0** |
| granite | 4007 | 3960 | 47 | **0** |
| tuff | 5568 | 5535 | 33 | **0** |
| gravel | 1341 | 1462 | **0** | 121 |
| dirt | 3547 | 3642 | 1 | 96 |
| water | 1867 | 1971 | 58 | 162 |
| sand | 439 | 433 | 6 | 0 |

つまり「vanilla にあってこちらに無い gravel/dirt/water」のセルが、こちらでは
**後続の ore が stone-family で埋めている**（ore 側が上位集合なのはそのため）。
石系 ore の位置・形そのものは一致していて、欠けているのは step 6 の先頭 2 つ
（`ore_dirt` / `ore_gravel`）と step 8 の spring が置くセル。

**spring の向きのバグを修正**（`SpringFeature` = `edp`）:

```java
if (!level.getBlockState(pos.below()).is(cfg.validBlocks)) return false;   // below を見る
if (cfg.requiresBlockBelow && !level.getBlockState(pos.below(2)).is(...)) return false;
// 走査は west, east, north, south, **above**
```
こちらは *above* を見て、走査に *down* を使っていた（＝天井から生える spring）。
さらに流体は `createLegacyBlock()` で置くので falling な水は **level 8**。

修正後も 2×2 の diff は 1035 のまま（spring が置く水は tick で広がるため、
稼働後の world と比べると別物）。`SPRING_TRACE=1` で候補 229 件中 4 件が
rock 4 / hole 1 の条件を満たすことを確認。

### 1035 セルの分解（現時点の理解）

| 種類 | セル | 性質 |
|---|---|---|
| 葉の `distance`（`oak_leaves != oak_leaves`） | 380 | **tick 由来**（`LeavesBlock.updateShape` は tick を予約するだけ）。既定で除外 |
| vanilla の gravel/dirt/water vs こちらの ore | ~379 | 欠けている ore_dirt/ore_gravel と spring（水は tick で広がる） |
| 樹冠の幾何（葉 vs 空気） | ~470 | 木の位置/形（45 vs 38 本） |
| その他 | 残り | — |

carver / aquifer 層は現行コードでも 2×2 で **0 セル**
（`tools/carver_fixture.sh tcur - - - 2`）。

## 43. `BiomeManager.getBiome` を実装して木が 45 → 42、diff 1035 → 940

§42 で「vanilla には biome が 2 種類ある」と判明したので、**2 番目**
（`BiomeManager.getBiome(BlockPos)` = biome blend）を port に実装した。

- `BiomeManager.a(JIIIDDD)D`（`getFiddledDistance`）: quart 座標 3 つと zoom seed を
  `Mth.hash` に 6 回かけて `s` を作り、`(floorMod(s>>24, 1024)/1024 - 0.5) * 0.9` を
  x/y/z それぞれに 1 つずつ得て、`(fz+f)² + (fy+e)² + (fx+d)²` を返す。
- `BiomeManager.a(Ljd;)Ljm;`: `pos − 2` を取り、その quart セルと小数部から
  **8 近傍**（各軸 `i&4`/`i&2`/`i&1` で ±1、小数部は `f` か `f−1`）を上の距離で順位付けし、
  最小の角の biome を返す。
- zoom seed は `Hashing.sha256().hashLong(seed).asLong()`（Guava の `putLong` は
  little-endian、`asLong` も先頭 8 バイト little-endian）。この値は
  **`sha256` を自前実装**して一致を検証した（seed 0 → 8794265229978523055、
  既知ベクタ `sha256("abc")`）。
- port 側: `generation_math::biome_zoom_seed` / `mth_hash` / `fiddled_distance`、
  `worldgen::Gen::biome_blended`、`FC::biome` をそちらへ（`PM::Biome` のほか、
  `ctx.biome` を使う vegetal 系も同じ経路に乗る）。chunk container を使う経路
  （保存・送信・`MC_SPINE_BIOME_DUMP`）はそのまま。

結果:

| | 前 | 後 |
|---|---|---|
| 2×2 feature あり diff | 1035 | **940** |
| うち tick 由来（葉の distance）を除く | 655 | **546** |
| 木（chunk (0,0) 周辺 3×3） | 45 | **42**（vanilla 38） |
| chunk (1,0) の木 | 2 | **0**（vanilla 0）✓ |
| chunk (1,1) の diff | 208 | **176** |

`BIOMEF_TRACE` の (20,66,3) は **false / biome=river** になり、vanilla の
`BIOMEF 20 66 3 false` と一致した（この 1 点が 4 つの木と 100 セルを動かしていた）。

残り: chunk 別の木は (0,-1) 6 vs 7、(0,1) 8 vs 5、(-1,-1) 3 vs 2、(-1,1) 8 vs 7。
次の一単位は同じ手法（`InSquarePlacement` の生候補 + `BiomeFilter` の可否 +
`would_survive`）を chunk (0,1) に当てる。

## 44. chunk (0,1) の候補列: 最初の 2 つは一致、3 つ目からずれる

biome を直した後の残りは chunk 別の木の数（(0,1) 8 vs 5 が最大）。
`InSquarePlacement` の生候補を両側で取り出した（port に `INSQ_TRACE=cx,cz`、
agent は `-Dinsqchunk`）:

| # | こちら (x,z) | vanilla (x,z) |
|---|---|---|
| 1 | 3,20 | 3,20 ✓ |
| 2 | 0,17 | 0,17 ✓ |
| 3 | 2,17 | **10,17** |
| 4 | 6,16 | 6,16 |
| 5 | 12,18 | 12,18 |
| 6-10 | 1,16 / 3,27 / 14,23 / 15,27 / 3,31 | 2,21 / 0,31 / 3,30 / 11,19 / 8,18 |

**deco seed は一致**（両側 353052641971821200）。したがって最初の 2 候補の一致は
本物で、3 つ目の x が違う = **候補 1〜2 の間で消費ドロー数が違う**（候補 2 の木の
生成、または候補の棄却）。4〜5 が一致するのは偶然の重なり（x は 16 値、z も 16 値）。

もう一つ重要な観測: **vanilla の候補 2 は (0,70,17)**（y=70）。地表は 64 なので、
この時点でその列の OCEAN_FLOOR ハイトマップは **既に別の木の樹冠で持ち上げられている**。
保存済み chunk のハイトマップでも (3,30)=71,(6,16)=70,(15,27)=69,(3,31)=71 と、
樹冠を数えている（＝葉は OCEAN_FLOOR の対象）。こちらの `oc_t` は「solid を書いた
セル」で持ち上げるので、**葉を solid と数えるか**と**候補間のタイミング**が同一で
ある必要がある。y が違えば biome 判定も変わる。

### 次の計測

`FSEED_ARM`/`-Dfseedarm` を (feature 20, step 9, chunk (0,1)) に合わせ、生ドロー列
（port: `NDRAW`/`FDRAW`/`XDRAW`、agent: `RDRAW`）を先頭から diff する。最初の
食い違いが「どのドローが余分/不足か」を直接示す（carver で効いた手法）。

## 45. 「候補の間で世界が変わる」を移植する（残差の次の機構）

chunk (0,1) の比較で見えたのは、候補ごとに **世界の状態が変わる** こと:

- vanilla の候補 2 は **(0,70,17)**。地表は 64 なので、その時点でその列の
  OCEAN_FLOOR ハイトマップは**既に別の木の樹冠で持ち上がっている**。
- 保存済み chunk のハイトマップも樹冠を数えている
  （(3,30)=71、(6,16)=70、(15,27)=69、(3,31)=71 に対し、地形だけなら 63〜64）。
  ＝ **葉は OCEAN_FLOOR の対象**で、`TreeFeature` の書き込みがハイトマップを上げる。
- したがって `heightmap(OCEAN_FLOOR)` を使う feature は、**同じ chunk の先行候補が
  置いた木の高さ**を見る。こちらの `oc_t`（solid を書いたセルで持ち上げる）は
  この意味で正しい形だが、**葉を solid と数えるか**と**候補間の適用タイミング**が
  vanilla と同一でないと y がずれ、y がずれた先で biome 判定も変わる。

これは「地形を一致させれば十分」ではなく、**配置が自分の入力を書き換える**という
一般形。同じ形は spring（水を置くと後続の fluid 判定が変わる）、disk（地表を
砂に変えると後続の would_survive が変わる）にもある。

## 46. ハイトマップの所有者が違った: 940 → 704、木 42 → 39

§44/§45 で特定した「候補の間で世界が変わる」の実装バグ。生ドロー列を (feature 20,
step 9, chunk (0,1)) で突き合わせると、**最初の食い違いは draw 28**:

```
27 まで両側一致
ours 28: nextInt(3)=2      ← 木の幹の高さ（= 候補 2 で木を生成し始める）
van  28: nextInt(16)=10    ← 候補 3 の in_square（= 候補 2 を棄却した）
```

vanilla の候補 2 は BIOMEF で **(0,70,17)**。地表は 63/64 なので、y=70 は
**隣の chunk の木の樹冠**（その列の葉は 65〜69）で持ち上がったハイトマップ値。
sapling は葉の上に置けないので `would_survive` が false になり、木を作らない。
こちらは同じ列のハイトマップを 64 と読んでいたため木を作り始め、**そこから
ドローがずれて以降の候補が全部違う**（候補 4〜5 の一致は偶然）。

原因は場所: こちらの feature 層ハイトマップは `FC`（= populate 1 回分）が持つ
`ws_t`/`oc_t` で、**同じ chunk の書き込みしか見えない**。vanilla は
「書き込まれた位置を所有する chunk のハイトマップ」を `setBlock` ごとに更新するので、
隣の chunk の木がこちらの列を持ち上げる。

修正: ハイトマップの所有者を `Gen` に移し（`feat_hm`: 列 → (最高非空気+1, 最高固体+1)）、
`FC::set` のたびに `note_feature_write` で更新、`hm_ws`/`hm_oc` はそこを読む
（`FC` 側の `ws_t`/`oc_t` は削除 = 所有者を 1 つに）。

| | 前 | 後 |
|---|---|---|
| 生ドローの一致 | 27 draw | **139 draw** |
| 2×2 feature あり diff | 940 | **704** |
| tick 由来を除く | 546 | **285** |
| 木（3×3） | 42 | **39**（vanilla 38） |
| chunk (0,1) の木 | 8 | **5** = vanilla ✓ |

次の食い違いは draw 139: こちら `nextInt(16)=7`（in_square）に対し vanilla は
`nextInt(8)=3` / `nextInt(4)=0` — **別の feature の連鎖**に入っている。残差の木は
(-1,-1) 3 vs 2、(-1,1) 8 vs 7、(0,-1) 6 vs 7（1 本ずつ）で、同じ手法（生ドロー +
その feature の候補列）を当てる。

## 47. 残りの木は「同じ位置で背が高い」— chunk (-1,1) の tree height

chunk (0,1) は候補列も biome 判定も木の本数も vanilla と一致した（§46）。
唯一残る差は候補 2 の **y が 72（vanilla 70）** で、これは**隣の chunk の木の樹冠**が
2 ブロック高いため。そこで feature 層ハイトマップを列ごとに vanilla の保存済み
`OCEAN_FLOOR` と比べると（**populate 後に読むこと** — 集計前だと地形値が返り、
偽の差が出る。`MC_SPINE_HM_DUMP` は 3×3 を populate してから読むよう修正した）:

| 列 | こちら | vanilla |
|---|---|---|
| (-1,16) | 72 | 70 |
| (-3,16) | 71 | 69 |
| (-16,17) | 69 | 64 |
| (-15,17) | 68 | 65 |

81/256 列が違い、方向は一貫して**こちらが高い**:

- x = -1〜-3, z = 16〜17: **同じ origin の木が 2 ブロック高い**（vanilla も
  (-1,64,17) に木を持ち、本数も一致している）
- x = -16/-15: vanilla に**無い木**（tree count 差 (-1,1) 8 vs 7 の 2 本）
- これらは chunk (-1,1) の配置なので、生ドローの窓（chunk (0,1) の fseed から 200 draw）
  の外にある

次の計測: 窓を **chunk (-1,1)** の (feature 20, step 9) に移し、`(-1,64,17)` の木の
**幹の高さ**（`nextInt(3)` → `nextInt(1)`）と selector/foliage のドローを vanilla と
突き合わせる。木の高さが 2 違う = その 2 draw の値が違うはずで、1 箇所で決まる。

### 47.1 chunk (-1,1) の窓で特定: draw 120 の候補 (-16,26)

窓を chunk (-1,1) の (feature 20, step 9) に移すと、**120 draw まで一致**して
そこで分岐する:

```
119 まで両側一致（候補 (-16,26) の in_square と selector の float 2 つ）
ours 120: nextInt(3)=1       ← 幹の高さ（木を生成し始める）
van  120: nextInt(16)=7      ← 次の候補の in_square（＝ (-16,26) を棄却）
```

セルを見ると理由が明確:

| (-16, 63..71, 26) | こちら | vanilla |
|---|---|---|
| 65〜69 | oak_log（幹） | **oak_leaves**（隣の木の樹冠） |
| 70 | oak_leaves | air |

vanilla は (-16,26) の列に**別の木の樹冠**（葉が 66〜69）を持ち、その列の
`OCEAN_FLOOR` は 70。候補 y=70 の下は葉なので sapling は育たず棄却。こちらは
その樹冠が無く y=65（草の上）を読んで木を作る。その樹冠は chunk **(‑2,1)** 側の
木（窓の外）由来で、こちらの tree 一覧にその木が無い（または樹冠が届いていない）。

次の計測: 窓（`-Dtreechunk`）を (-2,1)/(-2,2) まで広げ、両側の tree origin 一覧を
比べて「無い木」を特定する。`-Dfseedarm` の窓も (-2,1) に移せば、その木の
chain（selector/trunk/foliage）まで diff できる。

## 48. 2 つの発見: feature 集合は 3×3 の section palette、残差は**装飾順**

### (a) feature の実行条件を取り違えていた（修正）

vanilla の `applyBiomeDecoration` は
`ChunkPos.rangeClosed(chunkPos, 1)` の各 chunk について **section の biome palette を
union** し（`$$11.retainAll(possibleBiomes)` したうえで）その集合で feature を選ぶ。
こちらの前フィルタは **3×3 の中心 9 マスを y=0 でサンプル**していた。y=0 は地下なので
**cave biome を読み、地表の biome（river など）を落とす**。

実測: chunk (-2,1) の step 9 で `trees_water`（index **48**）ほか 48/60/73 の 3 つが
こちらの一覧から欠けていた（vanilla 12 件 / こちら 9 件）。修正後は 12 件すべて一致。

修正: `Gen::chunk_biome_set(cx,cz)`（その chunk の全 section・全 quart セル 1536 点の
distinct biome、chunk 単位で LRU キャッシュ）を足し、前フィルタは 3×3 のそれを union する。

### (b) それでも (-2,1) の木は一致しない → 残差は「装飾順」

修正後も chunk (-2,1) の木は vanilla と 1 本も重ならない（こちら 6 / vanilla 4）。
deco seed も feature 添字集合も一致しているので、違いは **feature 実行時に世界が
どこまで装飾済みか** = 装飾の順序。

- vanilla は「その時点までに装飾した chunk」の書き込みを全部見る（木の樹冠は
  隣 chunk から 2 ブロックまで届く）。
- こちらは `feat_state` が 3×3 をオンデマンドに populate するので、**こちらが
  決めた順序**でしか蓄積しない。

2×2 の固定具（(0,0)〜(1,1)）が 704 セルで動かないのは、影響する chunk (-2,1)/(-1,1) が
固定具の外だから。順序依存の差を消すには「隣が全部装飾済みの chunk を装飾する」順序で
回す（＝3×3 を先に埋める）か、サーバーが装飾した順序を再現する必要がある。

## 49. 残差 704 の内訳と、次の一単位（ore_dirt / ore_gravel の blob が部分集合）

2×2 の残差（tick 由来の葉 419 を除いた 704）をブロック別に取ると:

| 組 | セル |
|---|---|
| diorite / andesite != water | 142 |
| diorite != gravel | 87 |
| andesite != dirt | 80 |
| oak_leaves != air / air != short_grass / seagrass 系 | ~137 |
| deepslate != deepslate_lapis_ore | 35 |
| deepslate != deepslate_diamond_ore | 27 |
| その他 | ~150 |

**vanilla にあってこちらに無い** ore/gravel/dirt 系のセルは 319 で、そのうち
**air に隣接するのはわずか 2 セル** → `discard_chance_on_air_exposure` の経路ではない。
タグの疑い（`base_stone_overworld` が gravel/dirt を含む）も
**単体テストで否定**（`base_stone_overworld_excludes_gravel_and_dirt`、gravel/dirt/grass/
sand/clay/slab は一致しない）。

セルを見ると:

```
(15,-16,11) こちら tuff     vanilla gravel（下は deepslate）
(24,10,12)  こちら andesite vanilla dirt
(6,-32,12)  こちら deepslate vanilla deepslate_lapis_ore
```

つまり **先に走る `ore_dirt`(index 0) / `ore_gravel`(1) の blob がセルを落としており**、
そのあと stone 系 ore（andesite/tuff/diorite…）がそこを埋めるので
「石系 ore は上位集合・gravel/dirt は部分集合」という非対称が出る。
lapis/diamond も同様に部分集合。exposure 経路と tag は否定済みなので、残る候補は
**blob のセル走査そのもの**（origin は一致している）。

次の一単位（隔離実験）: `placed_feature/ore_*.json` の `count` を 0 にして
**1 つの ore だけ動かした vanilla world** を作り、こちらの同じ構成の dump と
セル単位で比べる（skill の「count: 0 で 1 feature を隔離する」パターン）。
`tools/carver_fixture.sh` の `CARVER_FIXTURE_EXTRA` が ctl datapack を差し込めるので、
その仕組みを feature 用に流用する。

## 50. 隔離実験の道具・pack 優先順位の欠陥・blob の形

`ore_gravel` だけを動かす隔離固定具（`tools/ore_fixture.sh` + `tools/ore_diff.py`）を
足した。他の `ore_*`/`disk_*` の placement を `count: 0` にした ctl pack を
**vanilla world と repo の両方**に置き、その 1 feature のセルだけを比べる。

その過程で 2 つの**道具側の欠陥**が出た:

1. **pack の優先順位が未定義だった**。`dp_files` は `read_dir` の順に
   `entry().or_insert()` していたので「先に見つけた pack が勝つ」= 上書きが効かない
   ことがあった（ctl pack を無視して全 ore が走り、241 起源 vs server 126 起源）。
   `Gen::packs()`（`vanilla` を最下位に固定し、残りは名前順）＋ `insert`（後勝ち）に
   修正。**datapack の上書きはこれで決定的**になった。
2. **agent の `ore` フックが chunk ゲートを失っていた**（以前の編集で
   `-Dorechunk` の判定を dimension 判定に置き換えてしまっていた）。そのためログが
   3×3 の 9 chunk 全部を拾い、「126 vs 14」という**存在しない配置差**に見えていた。
   両方のゲートを戻した。

修正後の隔離比較（chunk (0,0)、`ore_gravel` のみ）:

| | |
|---|---|
| 起源（origin） | **こちら 14 / vanilla 14 で完全一致**（こちら 14 は vanilla の部分集合で同数） |
| セル | ours 1361 / vanilla 1334、共通 **1173**、ours-only **188**、vanilla-only **161** |

**起源が一致してセルが 8% 違う** = blob の**形**（半径・丸め・セル走査）の差。
次は 1 起源を選び、両側でその blob のセルを flood fill して並べる
（`ORE_CELL_TRACE` はこちらの書き込み数を出す。vanilla 側は world から起源周辺を
取り出せばよい）。

## 51. 残差の正体: `OreFeature.place` の探索箱 vs ハイトマップ判定（＝装飾順）

隔離固定具で `ore_gravel` を両側同じ条件にして比べると:

| chunk | 起源の一致 |
|---|---|
| (0,0) | 14/14 |
| (0,1) | 14/14 |
| (1,1) | **9/14** |

ドロー列を **nextDouble（vein の半径）と feature 帰属つき**で突き合わせると
（`f=1,6` が付くので帰属を推測しなくてよい）、**最初の食い違いは draw 108**:

```
107 まで両側完全一致（in_square, height_range, 角度, y オフセット2つ）
ours 108: nextDouble ...    ← 候補3で vein を生成し始める
van  108: nextInt(16)=5     ← 候補3を捨てて次の候補の in_square へ
```

`OreFeature.place` の構造が理由:

```java
float angle = rand.i() * PI;               // ← ここまでは両側とも引く
double l = pos.y + rand.a(3) - 2;          // ここも
int o = pos.y - 2 - j;
for (xp...) for (zp...)
   if (o <= level.getHeight(Heightmap.Types.OCEAN_FLOOR_WG, xp, zp))   // ← ここで分岐
      return this.a(level, rand, cfg, ...);   // vein の半径を引く
return false;                                  // 引かずに終わる
```

候補 3 は **(23, 74, 30)** で、`o ≈ 69` に対し地形の `OCEAN_FLOOR_WG` は 64 前後。
vanilla は `69 <= 64` が偽で vein を生成しない。こちらは**その列のハイトマップが
樹冠で 69 以上に上がっている**ため箱が届くと判定して vein を生成する。

つまり残差の ore 配置差（chunk (1,1) の 5 origin、ひいては 2×2 の ore/dirt/gravel
非対称）は、**どの樹冠がその時点で見えているか = 装飾順の差**に帰着する。
`OCEAN_FLOOR_WG` は葉を数えるので、隣 chunk の木が 2 ブロック届く範囲で判定が変わる。

### 道具の改善（今回）

- port / agent の DRAW ログに **feature 帰属**（`f=i,step`）を追加（`XTRACE_FEAT` /
  `FEAT`）。「どの feature のチェーンか」を列の形から推測しなくてよくなった。
- port の `nextDouble` を DRAW に追加、agent に `nextDouble`（`dyn.j()D`）フックを追加。
  これが無いと vein の半径 33 ドローが両側 invisible で、**次の候補の in_square と
  誤整列**して「108 で nextDouble vs nextInt(16)」の意味を取り違える。

次の一単位: 装飾順を一致させる（「隣が全部装飾済みの chunk を装飾する」または
server の順序を再現）か、`OCEAN_FLOOR_WG` の更新規則（葉を数える／prime の時点）を
vanilla と突き合わせる。

## 52. 装飾順の実験: 「自分の chunk だけ decorate」は**悪化する**（実測）

「chunk の feature は driver が要求したときにだけ走る（隣は走らせない）」に変えると、
2×2 の diff は **704 → 6709** になった（deep 5719 / carver 990）。

理由: こちらの**マテリアルルール（surface）は feature 層のハイトマップを読む**ので、
隣 chunk の樹冠が見えなくなるだけで *地形*の出力が変わる（`cc.surf`/material 窓が動く）。
つまり feature 層のハイトマップは配置段階だけでなく地形段階の読みにも効いており、
順序の変更は地形を動かす。

逆に、**現行の「3×3 を先に decorate」は固定具の world と整合している**（704 に戻る）。
固定具の world は spawn 準備（半径 ~11 chunk）で周辺が**先に**装飾されてから
forceload されるので、dump 対象 chunk にとっては「隣が先」が実際の順序だった、
という説明がつく。

結論: 順序は *chunk ごとに違う*（spawn 準備の内側か外側か、forceload の順序）。
一律の規則では合わせられないので、次の選択肢は
(1) 固定具を **単一 chunk の forceload**にして、隣が未装飾であることが保証される条件で比べる、
(2) サーバーの装飾順を実測して再現する、
(3) 順序依存のセル（隣 chunk の樹冠が届く範囲）を比較から除外する。

### 52.1 順序に依存しない固定具への道（今回の試行と結果）

「隣が未装飾であることが保証される chunk」を作るための試行:

| 試行 | 結果 |
|---|---|
| spawn から 112 block 離れた chunk を `forceload add` | コマンドが失敗（`An unexpected error occurred`、スタックトレース無し）。spawn 近傍では成功する |
| `setworldspawn 112 64 -144` + 再起動 + 100 秒待ってから save/stop | spawn 準備は `Preparing spawn area: 0%` のまま進まず、遠方 chunk は `structure_starts` のまま（`prepareLevels` は**ワールド作成時**の spawn 周りだけを準備するため） |
| 上記の chunk を無理に比較 | 参照が `structure_starts` なので**比較自体が空振り**（両側「skip」で 0 セルと表示される）——固定具は「参照側が full か」を必ず確認する |

残る道: (a) 我々の protocol client（`tools/load_client.py`）で bot を遠方へ歩かせて
サーバーに自然な順序で生成させる（画面は使わない方針に合致）、
(b) `level.dat` を直接書いて spawn を先に移してから新規ワールドを作る、
(c) spawn 準備の spiral 順序を再現する、(d) 順序依存セルを比較から除外する。

## 54. 6709 の正体: 隣を見ないと**ore のゲートが閉じる**（＝参照は並列装飾を見ている）

chunk (0,0) 単体で両モードを比べた（参照は feature あり world）:

| モード | 差 |
|---|---|
| `FEAT_NBR=1`（3×3 を先に decorate、現行既定） | **82** |
| `FEAT_NBR=0`（自分の chunk だけ） | **1924** |

`FEAT_NBR=0` の内訳は `stone != andesite` x1061、`deepslate != tuff` x284、
`stone != diorite` x213 …＝**vanilla にある ore が無い**。理由は `OreFeature.place` の
探索箱判定（`boxMinY <= getHeight(OCEAN_FLOOR_WG, x, z)`）で、隣 chunk の樹冠が
ハイトマップから消えると**その ore が丸ごと生成されない**（§51 と同じ機構）。

したがって:

- こちらの ore 配置は**隣 chunk の feature 書き込みをハイトマップ経由で見ている**。
- 参照 world はその状態で作られている ＝ **並列装飾中に隣の書き込みを見ている**
  （`FEAT_NBR=0` が 1924 で悪化する事実がその証拠）。
- よって「ある chunk がどの隣の書き込みを見たか」は**実行時の並列進行度に依存**し、
  固定具の world を厳密に再現するにはその進行度までモデル化する必要がある。
  現行の「3×3 を先に decorate」は**その状況の最も近い近似**（82 セル）であり、
  順序についてこれ以上単純な規則では合わせられない。

残差 704 のうち:
- **順序/並列に依存する部分**（ore 系のゲート、`diorite != water/gravel` など）
- **依存しない部分**（spring の水、disk、樹冠の形など）＝ここは本物の実装差として追う価値がある

## 55. 順序に依存しない残差: 樹冠の形（隔離世界での実測）

隔離世界（`ore_gravel` 以外の ore/disk を 0 にした world、木は両側で動く）で
ブロック単位に比べると、**木の幹は完全一致、葉だけが数セル〜1 塊ずれる**:

| ブロック | ours | vanilla | 共通 | ours-only | vanilla-only |
|---|---|---|---|---|---|
| `oak_log` | 50 | 50 | **50** | 0 | 0 |
| `birch_log` | 12 | 12 | **12** | 0 | 0 |
| `oak_leaves` | 614 | 590 | 574 | 40 | 16 |
| `birch_leaves` | 105 | 126 | 105 | **0** | **21** |

- `oak_leaves` の 40/16 は **tick の落葉**（参照 world は稼働中なので `distance=7` の葉が消える）
  が混ざる。幹が一致しているので木の位置・高さは正しい。
- `birch_leaves` は **21 セルが丸ごと欠け**、位置は **x=30..31, y=68..71, z=0..4**
  （固定具領域の x 端 2 列）。幹は 12/12 一致しているので、**ある 1 本の樹冠の
  x≥30 部分だけが書かれていない**。

次の一単位: その幹（x≈30, z≈2 付近）を特定し、`FoliagePlacer` のドロー
（`BlobFoliagePlacer` の高さ・半径、`RandomSpread`/`FoliagePlacer` のセル走査）を
(該当 chunk, 該当 feature) の窓で両側 diff する。境界列であることが
「chunk 境界のクリップ」なのか「樹冠の形そのもの」なのかをまず切り分ける
（固定具領域を 4×4 に広げて同じ木を見れば、クリップ説は消える）。

## 56. 4×4 の隔離固定具: 樹冠の「欠け」は**スケジュール依存**だった（自分の訂正2回目）

2×2 の隔離世界で `birch_leaves` が 21 セル欠けていた件（§55）を、**4×4 の隔離世界**で
測り直すと:

| ブロック | ours | vanilla | 共通 | ours-only | vanilla-only | クラスタ |
|---|---|---|---|---|---|---|
| `birch_leaves` | 471 | 471 | **471** | 0 | 0 | 両側で完全一致 |
| `birch_log` | 59 | 60 | 59 | 0 | 1 | 幹 1 本だけ差 |
| `oak_leaves` | 2311 | 2369 | 2116 | 195 | 253 | クラスタサイズはほぼ同じ |

**樹冠は完全一致**した ＝ §55 の「ある 1 本の樹冠の片側が書かれていない」は
**樹冠の形のバグではなく、その 2×2 world の装飾スケジュール（並列進行度）の差**だった。
（同じ pack でも forceload 範囲が違えばスケジュールが変わり、こちらのモデルの
一致/不一致も変わる。）自分の読み違いを2回記録したことになるので、
「原因の説明は検証済み／仮説を分けて書く」規則（DESIGN 第6項）の実例として残す。

### 4×4 隔離固定具の残差（8/16 chunk が参照側 full、tick 由来の葉 distance を除く）

```
chunks 4x4  differing cells: 938（carver 656 / deep 282）
per chunk: 9 67 61 60 293 103 276 69
内訳（tick 由来を除く生の差 2407 のうち）:
  oak_leaves != oak_leaves 1166 / birch_leaves != birch_leaves 303 → 全て葉の distance（tick）
  air != oak_leaves 252 / oak_leaves != air 186 → 葉の有無（落葉/tick と実差）
  gravel != stone 116 / gravel != deepslate 70 / deepslate != gravel 39 → ore_gravel の blob
  air != short_grass 101 / water != tall_seagrass 30 / grass_block != dirt 42
```

2×2 より代表的な固定具になった（16 chunk・種々の地形）ので、以降はこれを使う。
`tools/ore_diff.py` も参照側の status を**先に**見て、不完全な chunk は両側とも
集計から外すよう修正（§55 の 4×4 比較が「ours 6709 / vanilla 2372」と出たのは
この欠陥だった）。

## 57. 参照の再現性（ノイズ床）と、こちらの再現性を測った

`tools/world_diff.py`（保存済み world 同士を chunk 単位で比較）を追加して、
**同じ pack・seed・forceload で参照を2回生成**し、互いに diff した:

```
two reference runs, 8 chunks compared: noise floor 22 cells
per chunk: 0 22 0 0 0 0 0 0
                  air != None      21
                  None != seagrass  1
```

- **ノイズ床 = 22 セル / 8 chunk**（1 chunk に集中、seagrass のパッチ）。vanilla の worldgen は
  **ほぼ決定的**（並列装飾でも保存結果は揺れない）＝「参照はスケジュール依存」という
  §54 の説明は**この規模では成り立たない**（床は 22 セルで、938 セルの 2%）。
- 同じ 22 セルは **scope を変えた参照同士**（2×2 の世界 vs 4×4 の世界）でも出た
  ＝ その 1 パッチは scope にも敏感。
- こちらの**ダンプは文脈非依存**であることを確認: chunk (1,0) を n=1 / n=2 / 2×2 固定具 /
  4×4 固定具で出して **0 セル差・md5 一致**。

したがって 4×4 隔離固定具の **938 セルはほぼ全部が実装差**で、参照の揺らぎではない。
§55/§56 の「birch の 21 セル欠け」は 4×4 では再現せず（471 = 471 で完全一致）、
この 21 セルは未説明のまま座標付きで残す（x=30..31, y=68..71, z=0..4）。

残差の内訳（4×4 隔離、床を引いた後に追うべき順）:
葉の在否 438 / gravle blob 225 / 植生（short_grass・seagrass・dirt↔grass）173 /
その他 ≈100。葉の `distance` 1469 は tick 由来で除外。

## 58. 未実装の configured-feature **型**が 34 件あった（`random_patch` を実装）

`short_grass` が ours 0 / vanilla 102、`seagrass` が 0 / 28 ＝ **丸ごと置けていない**。
逃げ道（`CF_MISSING` / `PF_UNKNOWN` / `FEAT_TRACE`）では何も出ず、feature は走っているが
`+0 writes` だった。原因は **configured feature の型の未実装**:

- `p_cf` が扱う型は 26 件、パックの `feature/*.json` の型は **60 件**（未実装 34 件）。
  内訳の上位: `random_patch` 18、`nether_forest_vegetation` 6、`flower` 6、
  `huge_fungus` 4、`seagrass` 4、`end_podium` 2 …（overworld の植生の大半が
  `random_patch` ✓）。
- `patch_grass_forest` の configured feature は `minecraft:patch_grass`
  （**type: `minecraft:random_patch`**）で、私たちは型を持っていなかった。

`RandomPatchFeature`（`edc`）を bytecode から実装:

```java
int xz = cfg.xzSpread() + 1, y = cfg.ySpread() + 1;
for (int i = 0; i < cfg.tries(); i++) {
   pos.setWithOffset(origin,
       rand.nextInt(xz) - rand.nextInt(xz),
       rand.nextInt(y)  - rand.nextInt(y),
       rand.nextInt(xz) - rand.nextInt(xz));
   if (cfg.feature().value().place(level, generator, rand, pos)) placed++;   // 入れ子は *placed* feature
}
```

＝ 1 try あたり 6 ドロー、順序は `xz, xz, y, y, xz, xz`（引数の左→右評価）。
入れ子は `FRef::Placed`（既存）で自分の modifier を同じ RNG で走らせる。

### さらに: `"blocks": "minecraft:air"` は**スカラー**

`patch_grass` の入れ子 placed feature は
`block_predicate_filter(matching_blocks {"blocks": "minecraft:air"})` を持つが、
こちらの `p_pred` は `blocks` を**リスト**としてしか読んでおらず、スカラーだと
`Pr::Blocks([])` ＝ 永久に不一致 → **入れ子が一切走らない**（`"fluids"` のスカラーと
同じ型のバグ）。修正した。

### 結果: 残差は 938 → **42939**（＝むしろ正しい方向）

| ブロック | 修正前 | 修正後 | vanilla |
|---|---|---|---|
| `short_grass` | 0 | 362 | 102 |
| `seagrass` | 0 | 0 | 28 |
| 4×4 の diff | 938 | **42939** | — |

これまで「動いていなかった」多数の feature が動き出すので、**diff は増える**
（＝ 0 セルという出口条件にはまだ遠いが、状態としてはより正しい）。
次にやること:

1. **過剰配置**（`short_grass` 362 vs 102 ＝ 3.5 倍）の原因究明
   （`random_patch` の tries/ドローか、入れ子 `simple_block` の条件か）
2. 残る型の実装（`seagrass`・`flower`・`kelp`・`sea_pickle`・`large_dripstone`・
   `geode` など overworld に効くもの）
3. その後で残差を**再基準化**して、順序非依存の塊（葉の在否・gravel blob・植生）を追う
