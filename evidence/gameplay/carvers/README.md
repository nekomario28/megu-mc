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
