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

## 移動視点の再測定（8 回、20 秒窓、spawn burst 込み）

| run | vanilla CPU | mc-spine CPU | 比 |
|---|---|---|---|
| 1 | 17.19 | 14.30 | 0.83 |
| 2 | 17.24 | 18.31 | 1.06 |
| 3 | 15.36 | 18.27 | 1.19 |
| 4 | 16.12 | 17.61 | 1.09 |
| 5 | 15.55 | 18.70 | 1.20 |
| 6 | 16.53 | 17.65 | 1.07 |
| 7 | 15.86 | 17.64 | 1.11 |
| 8 | 16.83 | 18.67 | 1.11 |

中央値は vanilla 16.33 対 mc-spine 17.96 で **x1.10**（RSS 142 対 ~1160 MiB）。
1 回だけ x0.83 が出たが、他 7 回は x1.06-1.20 なので **M3 は主張しない**。

## spawn burst を除いた測定（WARM=1、3 回）

`tools/load_compare.sh` の `WARM=1` は、測定窓の前に同じ長さの走行を 1 回行い、
spawn 生成を窓から除く（移動列は決定的なので、warm が前半、測定が後半を走る）。

| run | vanilla CPU | mc-spine CPU | 比 |
|---|---|---|---|
| 1 | 4.86 | 8.68 | 1.79 |
| 2 | 3.07 | 8.75 | 2.85 |
| 3 | 5.25 | 8.64 | 1.65 |

**定常状態では mc-spine の方が 1.6-2.9 倍重い**（中央値 x1.79）。burst 込みでは
差が小さいのは、vanilla の spawn 生成が高くつく（16-17s のうち ~11-13s）一方、
mc-spine は burst ~9s + 定常 ~9s だから。つまり M3 の残差は **移動中のチャンク
生成（feature の単一スレッド実行）**にあり、spawn ではない。

## 次の一手（R3）

定常状態の差（x1.65-2.85）は feature の単一スレッド実行が主因で、vanilla は
62-64 スレッドで同じ仕事を並列化している。個別の feature を削るより、
**チャンク生成の並列化**（読み取り専用の base terrain と、チャンク単位で閉じた
feature 書き込みを worker に渡し、所有者で決定的にマージする）が本筋。
owner: region/chunk pipeline の決定が先。並列化までの間は、`ore_dirt`
（11.7 ms/chunk）と `monster_room(_deep)`（計 8.9 ms）のループを測って削る。
測定は `WARM=1` を標準にし、spawn burst と定常を分けて報告する。

## 主張しないこと

- 1 ホスト・1 回の 20 秒窓。JIT/GC の影響を除いていない。
- 上の 22.56s は 1 回の測定。19.79s との差は 2.8s で、窓のばらつきより大きいが、
  同一 binary の反復は取っていない。
- 機能集合は等しくない（mc-spine の方が少ない）。同じ仕事の比較ではない。
- `FEAT_PROFILE` は `Instant` を feature ごとに取る診断で、その分の overhead を
  含む。順位付けに使い、絶対値の主張には使わない。
- end-to-end の 8 回は同一 binary だが、ホストの他プロセスと窓の内容で
  14.3-18.7s とばらつく。中央値だけを比較に使い、単発の最良値は主張しない。
- WARM=1 の 3 回は、移動列が決定的であること（warm と測定で別の区間を走る）に
  依存する。3 回だけなので中央値の信頼区間は広い。

## 2026-10-27: feature の地形読みをメモ化（103 → b77f1fd）

`FEAT_PROFILE=1 cargo test --release --feat_populate_perf`（seed 0、25 chunk）で、
feature の費用を「地形読み」と「feature 自身のロジック」に分けた後の内訳。

| | 変更前 | 変更後 |
|---|---|---|
| FEATPROF total（25 chunk） | 519.5 ms | **190.8 ms** |
| 地形読み | 52.1 ms（242,044 回、215 ns/call） | 52.8 ms（242,044 回、218 ns/call） |
| `ore_iron_upper` | 155.7 ms | **4.8 ms** |
| `ore_coal_upper` | 81.1 ms | **2.4 ms** |
| `ore_gravel` | 62.3 ms | 15.5 ms |
| `ore_dirt` | 30.0 ms | 19.1 ms |
| `monster_room_deep` | 10.7 ms | 10.5 ms |
| `populate9` wall | 720 ms | **582 ms** |

- 原因は「ロジック」ではなく **`getHeight(OCEAN_FLOOR_WG)` の再計算**だった。
  ore は vein の bounding square の各列（`ore_iron_upper` で約1,500回）について
  高さを問い合わせ、そのたびに列を `terrain_cell`（density + material rule +
  aquifer）で走査し直していた。列の WG heightmap は最後の feature 書き込み
  以降動かないので、`FC` でメモ化した。
- **正しさの罠**: vanilla は `setBlock` ごとに WG heightmap も更新する。
  メモを無効化せずに入れると `huge_fungus` のセルが変わった（hat は下の
  ブロックから伸びる）。`set` で書いた列のエントリを落とす実装にした。
  この無効化は mutation gate で固定する（`huge_fungus_matches_native_cells_and_rng`
  が赤になる）。gate の追加は、同じ `tools/mutation_check.sh` を別レーンが
  編集中のため保留。
- これで feature は **20.8 → 7.6 ms/chunk**。残りの上位は `ore_dirt` 0.76、
  `ore_gravel` 0.62、`monster_room(_deep)` 0.68 ms/chunk。

### 主張しないこと

- end-to-end の `load_compare.sh WARM=1` は再実行していない。**M3 の主張ではない**。
  定常状態の差（x1.65-2.85、中央値 x1.79）の原因とされる「feature の単一スレッド
  実行」はこの変更では消えていない。並列化の判断材料は
  [チャンク生成の並列化](../../../specs/chunk-parallelism/README.md)。
- FEAT_PROFILE は feature ごとに `Instant` を取る診断で、その overhead を含む。
- 1 seed・1 ホスト・1 回。25 chunk は 3x3 populate の 1 回分で、定常負荷ではない。

## 2026-10-27: 現行ビルドの feature 内訳（並列化の後）

`FEAT_PROFILE=1 cargo test --release feat_populate_perf`（seed 0、25 chunk）。
このブランチは evidence が x1.79 を記録した時点より feature が多い。

| | ms / 25 chunk | ms/chunk |
|---|---|---|
| FEATPROF total | 171.7 | 6.9 |
| うち地形読み (`FC::get`) | 43.8（242,044 回、181 ns） | 1.8 |
| WG heightmap のスキャン | 43,048 回 / 141,530 cell | 1.7 / 5.7k cell |

上位（ms / 25 chunk）: `trees_birch_and_oak` 17.5、`ore_dirt` 16.7、
`ore_granite_lower` 14.5、`ore_tuff` 14.3、`ore_gravel` 13.9、
`ore_diorite_lower` 13.8、`ore_andesite_lower` 13.3、`ore_coal_upper` 10.1、
`monster_room_deep` 9.4、`freeze_top_layer` 8.4。**ore 系で 64%**。

試して**効果が無かった**もの（いずれも測定で棄却、コミットしていない）:
- `FC` にチャンク列キャッシュを足す（1,700 回/chunk の `Gen::cols` 参照を短縮）:
  188.2 対 184.0 ms（ノイズ）。列参照は支配的でない。
- `ORE_CELL_TRACE` の `format!` を列ごとに評価しないよう hoist: 189.2 対 188.2 ms。
  `env_str!` が `None` のときは `is_some_and` が短絡するので、そもそも割り当ては無かった。
- チャンク単位の WG heightmap 一括構築: 1 チャンクあたり 1,722 回のスキャンが
  9 チャンクに分散（≈190/chunk）なので、256 列を先に作る方が高くつく。

残る費用は vanilla と同じアルゴリズムの実仕事（ore の列走査 + 楕円体セル）で、
vanilla が配列で O(1) で読むところを走査で払っている分。ここを削るには
永続 heightmap 配列（メモリ、2026-10-20 に一度撤回）か、feature 自体の削減が要る。

### end-to-end（`WARM=1 load_compare.sh 20 0`、同一ホスト）

| build | vanilla CPU | mc-spine CPU | RSS |
|---|---|---|---|
| 地形並列 workers=1 | 5.27 s | 21.91 s | 145 MiB |
| 地形並列 workers=4 | 4.90 s | 23.27 s | 162 MiB |

現行ブランチは **x4.2-4.7**（evidence の x1.79 は feature が少ない時点の値）。
並列化は CPU 秒を減らさない（仕事量は同じ、cold cache の重複で +6%）。
CPU 秒を下げるには feature の実仕事を減らす必要がある。

## 2026-10-28: 移動視点の CPU 内訳（feature は 7% だった）

`WARM=1 load_compare.sh 20 0` の条件（seed 0、view 4 / sim 5、移動視点、圧縮なし）で、
mc-spine 単体を同条件で走らせ、プロセス/スレッド/phase に分けて測った。
ツールは `MC_SPINE_CHUNK_PROFILE=1` と `MC_SPINE_TICK_PROFILE=1`（下記）。

### 総量と配分

| | 20 秒窓 |
|---|---|
| サーバ単体（クライアント無し） | **0.02 s** |
| 移動視点 1 人 | **29.8 s**（1.5 コア） |
| うち tick スレッド | 16.0 s |
| うち 接続スレッド（チャンク配信） | 14.0 s |
| うち 地形+feature 生成（90 chunk × 19 ms） | **約 1.7 s（6%）** |

切り分けは `/proc/<pid>/task/*/stat` を 1 秒ごとにポーリング（窓中に終了する
スレッドも拾う）。**視点の teleport 頻度は支配的でない**: 送信間隔を 1/20 に
落としても 29.48 s でほぼ同じ（＝チャンク配信側の仕事）。

### チャンク 1 枚（`MC_SPINE_CHUNK_PROFILE`、n=90）

| phase | ms/chunk |
|---|---|
| `map_chunk` 全体 | 103-162 |
| うち `chunk_data` | 65-94 |
| うち `terrain_section` 24 section（地形+feature） | **18-25** |
| うち `write_light_arrays` | **38-94** |
| 残り（overlay 走査・section エンコード・biome） | 約 40 |

### tick 本体（`MC_SPINE_TICK_PROFILE`、n=400）

| step | ms/tick |
|---|---|
| body 合計 | 11.4 |
| **environment（`keepalive_time_tick`）** | **10.8** |
| generated_fluids | 0.47 |
| 他 19 step 合計 | < 0.06 |

同じ実行の `[tick]` 統計: `avg_work_ms=19.2`、`max_work_ms=477`、
`over_budget=13/100`、tps 17.5-18.5。

`keepalive_time_tick` は world clock と 1 秒間隔の broadcast しかしない。
アイドル時は tick body 0.04 ms/tick なので、**この 10.8 ms は計算ではなく
待ち**（チャンク配信が世界/region ロックを保持している間）と解釈できる。
確定にはロック待ち時間の内訳計測が要る（`GenLock` と同じ形を region/world に足す）。

### 結論

- feature の地形読み削減（`b77f1fd`、20.8 → 7.6 ms/chunk）と地形並列（stage 2A）は、
  いずれも**この窓の 6% 側**を触っていた。M3（CPU 秒）は配信経路とロック待ちが支配。
- 次の候補は (1) `write_light_arrays` 38-94 ms/chunk、(2) `chunk_data` の
  overlay/エンコード約 40 ms/chunk、(3) tick スレッドの待ち 10.8 ms/tick。
- 地形並列の既定は 1（inline）のまま。配信経路を直した後に再測定する。
