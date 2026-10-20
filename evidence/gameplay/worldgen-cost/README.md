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

## feature の地形読みの内訳

`FEAT_PROFILE` に地形読みの集計を足して測った（25 chunk の populate）:

- 地形読み（`FC::get` → `terrain_state`）: **492 ms / 1088 ms（45%）**、
  **273,941 回、1.8 µs/call**。
- 残り 55% は feature 自身のロジック（modifier、楕円体、書き込み）。

読み 1 回 1.8 µs の内訳は density のブロック毎評価 + material rule +
`deep_state` の memo（`deep_c` は 500k 上限で、burst では溢れて再計算になる）。

## 試したこと2: チャンク単位の地形配列（却下）

feature が読む地形をチャンク単位の配列（16×16×height、no features）として
一度だけ作り、populate と fill で共有する実装を試した。

- 配列 1 チャンクの構築: **20.7 ms/chunk**（release、`wtop` より上は air として
  スキップ）。これは fill がセルごとに払っていた費用とほぼ同じで、feature の
  地形読み（約 20 ms/chunk）を消せる計算だった。
- ログイン burst（spawn の ~121 チャンク）では容量が足りず、cap 32/64 は
  再構築で 15 秒のログイン期限を超えた。cap 160 で burst は中立
  （ログイン 12.5s 対 キャッシュなし 12.8s）だが RSS は 147 → 242 MiB。
- 移動視点の代表負荷は **22.56s 対 19.79s（x1.27、キャッシュなしは x1.13）**で
  **悪化**した（driver は両方 ok）。配列の構築費用とキャッシュ圧が読みの節約を
  上回った。
- 判断: **撤回**。地形の読みを作り直す方向では M3 に届かない。次の候補は
  feature ロジック本体（40 ms/chunk）の削減か、生成の並列化（所有者を決めてから）。

## 次の一手（R3）

feature ロジック本体の 40 ms/chunk を対象にする。最初に `FEAT_PROFILE` の
上位（`ore_dirt` 13 ms、`monster_room(_deep)` 9.7 ms）を 1 つずつ測り、
候補セル数・書き込み数・地形読み以外のコストを分ける。並列化は所有者
（region/chunk pipeline）を決めてから。

## 主張しないこと

- 1 ホスト・1 回の 20 秒窓。JIT/GC の影響を除いていない。
- 上の 22.56s は 1 回の測定。19.79s との差は 2.8s で、窓のばらつきより大きいが、
  同一 binary の反復は取っていない。
- 機能集合は等しくない（mc-spine の方が少ない）。同じ仕事の比較ではない。
- `FEAT_PROFILE` は `Instant` を feature ごとに取る診断で、その分の overhead を
  含む。順位付けに使い、絶対値の主張には使わない。
