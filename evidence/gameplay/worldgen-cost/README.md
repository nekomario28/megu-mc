# 生成器の費用（地形一致後、M3 未達の内訳）

2026-10-20。地形（feature/carver なし）が vanilla と 0 セルで一致した後の費用。
`tools/load_compare.sh 20 0`（同一ホスト、20 秒窓、同じ移動列、view 4 / sim 5、
圧縮なし）と、`WGPERF=1` / `FEAT_PROFILE=1` の診断で取った。

## 代表負荷

| | CPU(s) | RSS(MiB) | threads | packets |
|---|---|---|---|---|
| vanilla | 17.59 | 1204.7 | 62 | 2133 |
| mc-spine | 19.79 | 145.2 | 3 | 571 |

CPU は **x1.125 で M3 未達**（メモリは約 8 倍軽い）。以前の x0.63 は地形が
違う状態の値で、現在の代表値にしない。

## 内訳（release、seed 0）

- `chunk_cols`（地形）: **11.5 ms/chunk**。
- feature 層の populate: **約 60 ms/chunk**（`feat_populate_perf` の 25 chunk で
  819.7ms）。生成時間の約 84% は feature。
- `deep_state`: 0.3 µs/call。

`FEAT_PROFILE` の上位（25 chunk 分の合計）:

| ms | feature |
|---|---|
| 323.6 | `ore_dirt` |
| 147.3 | `monster_room_deep` |
| 98.0 | `ore_gravel` |
| 95.2 | `monster_room` |
| 24.5 | `trees_birch_and_oak_leaf_litter` |
| 20.6 | `ore_tuff` |
| 18.8 | `ore_diorite_lower` |
| 18.7 | `ore_granite_lower` |
| 17.9 | `ore_andesite_lower` |
| 13.3 | `underwater_magma` |

## 原因

feature の候補セルごとに `FC::get` → `Gen::terrain_state`（density の
ブロック毎評価 + material rule + deep_state の memo）を呼び、1 セルあたり
µs 台かかる。vanilla はチャンクのブロック配列を読むだけなので 1 桁 ns。
生成が単一スレッドなのも重なる（vanilla は 62 スレッド）。

ore 1 本は楕円体の候補セルすべてでこの読み出しを行うため、`ore_dirt` だけで
13 ms/chunk になる。地形の再計算そのものではなく、**feature が読む地形の
作り方**が律速。

## 試したこと: セクション単位の地形配列

`chunk_data` は以前、1 ブロックごとに worldgen のロックを最大 5 回取り、
overlay があるときは生成状態を 2 回計算していた。`Gen::block_state_at` と
`terrain_section`（16³ を 1 ロックで埋める）に変え、`wg2_block` も 1 ロックにした。

- 地形パリティは 0 セルのまま（(0,0)、(0,1)、(-1,-1)）。
- 代表負荷は 19.87 → 19.79 s（ノイズ内）。fill のロックは支配的ではなかった。
- 費用の支配は feature 側なので、この変更は「計測上の改善」ではなく
  「1 ロック・1 回計算」への整理として残す。

## 次の一手（R3）

feature が読む地形を、セルごとの再計算ではなく**チャンク単位の配列として
一度だけ作って共有する**。populate と fill が同じ配列を読み、LRU で
9〜16 チャンク（1 チャンク 384 KiB、3.5〜6 MiB）に収める。地形は feature の
書き込みに依存しない（feature は `ctx.wr` を上に重ねる）ので、順序に依存しない
共有キャッシュにできる。並列化は所有者（region/chunk pipeline）を決めてから。

## 主張しないこと

- 1 ホスト・1 回の 20 秒窓。JIT/GC の影響を除いていない。
- 機能集合は等しくない（mc-spine の方が少ない）。同じ仕事の比較ではない。
- `FEAT_PROFILE` は `Instant` を feature ごとに取る診断で、その分の overhead を
  含む。順位付けに使い、絶対値の主張には使わない。
