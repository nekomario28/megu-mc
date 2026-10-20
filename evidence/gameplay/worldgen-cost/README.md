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

## 試したこと3: feature 側のチャンク列キャッシュ（採用）

読み 1 回の内訳を疑い、`FC::get` がセルごとに `Gen::cols()`（HashMap 参照 +
`Arc` clone）を呼んでいたのを、チャンクが変わったときだけ取得するようにした。
`Gen::terrain_cell(&ChunkCols, …)` は `terrain_state` と同じ計算を、取得済みの
列に対して行う。

|| 前 | 後 |
|---|---|---|
| 地形読み | 1797 ns/call | **295 ns/call** |
| 地形読み合計（25 chunk） | 492 ms | **81 ms** |
| FEATPROF total | 1088 ms | **736 ms** |
| `populate9` | 667 ms | **411 ms** |

計算内容は同じ（同じ列・同じ密度・同じ material rule）で、feature capture
digest も変わらない。`Populate9` は 60 → 37 ms/chunk。

## 移動視点の再測定（4 回、20 秒窓）

| run | vanilla CPU | mc-spine CPU | 比 |
|---|---|---|---|
| 1 | 17.19 | 14.30 | 0.83 |
| 2 | 17.24 | 18.31 | 1.06 |
| 3 | 15.36 | 18.27 | 1.19 |
| 4 | 16.12 | 17.61 | 1.09 |

中央値は vanilla 16.68 対 mc-spine 17.94 で **x1.08**（RSS は 145 対 1114-1183 MiB）。
1 回だけ x0.83 が出たが、同じ binary の他 3 回は x1.06-1.19 なので **M3 はまだ
主張しない**。ログイン時の spawn 生成（約 121 チャンク）が 20 秒窓の大半を占め、
窓ごとのばらつきも大きい。feature ロジック側の改善（`populate9` -38%）は
限定的にしか end-to-end に出ていない。

## 次の一手（R3）

feature ロジック本体（読みを除いて約 40 ms/chunk）が残る。`ore_dirt` は
読みを安くしても 11.7 ms/chunk で、コストは vein の楕円体ループ
（候補セル数 × 距離計算）にある。`monster_room(_deep)` も同様。
次はこの 2 つを 1 つずつ、候補セル数と書き込み数、ループ内の計算量に分けて測る。
並列化（vanilla は 62-64 スレッド、mc-spine は 3）は所有者
（region/chunk pipeline）を決めてから。ログイン burst を窓から除外する測定も要る。

## 主張しないこと

- 1 ホスト・1 回の 20 秒窓。JIT/GC の影響を除いていない。
- 上の 22.56s は 1 回の測定。19.79s との差は 2.8s で、窓のばらつきより大きいが、
  同一 binary の反復は取っていない。
- 機能集合は等しくない（mc-spine の方が少ない）。同じ仕事の比較ではない。
- `FEAT_PROFILE` は `Instant` を feature ごとに取る診断で、その分の overhead を
  含む。順位付けに使い、絶対値の主張には使わない。
- end-to-end の 4 回は同一 binary だが、ホストの他プロセスと窓の内容で
  14.3-18.3s とばらつく。中央値だけを比較に使い、単発の最良値は主張しない。
