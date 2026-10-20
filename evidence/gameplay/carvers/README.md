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
