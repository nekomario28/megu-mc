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
