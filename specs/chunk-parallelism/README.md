# チャンク生成の並列化（R3 の次のスライス）

Status: 設計。実装前。owner の決定と検証契約を先に固定する。

## なぜ

`WARM=1`（spawn burst を除く移動視点）で mc-spine は vanilla の 1.65-2.85 倍の
CPU を使う（中央値 x1.79、[証拠](../../evidence/gameplay/worldgen-cost/README.md)）。
vanilla は 62-64 スレッドでチャンク生成を並列化し、mc-spine は 3 スレッドで
feature を単一実行している。個別 feature のループを削るより、同じ仕事を
並列化する方が桁が大きい。

## 決定実験（2026-10-27、実装前）

並列化に投資する前に、残差が「計算量」か「直列化」か「ロック待ち」かを切り分けた。
`b77f1fd` の前後で `FEAT_PROFILE=1 cargo test --release feat_populate_perf`
（seed 0、25 chunk）を取った結果:

| 項目 | 値 | 意味 |
|---|---|---|
| chunk_cols（地形） | 11.5 ms/chunk | 既存の測定。並列化の対象 |
| feature populate | 20.8 → **7.6 ms/chunk** | 地形読みのメモ化で削減済み |
| うち地形読み | 2.1 ms/chunk（28%） | `FC::get`/`terrain_cell` |
| うち feature ロジック | 5.5 ms/chunk | ore のセル走査・dungeon の箱 |
| 直列チャンク費用（合計） | **約 19 ms/chunk** | 1 スレッドで処理している量 |

つまり残差は **チャンクあたり約 19 ms の直列仕事**で、vanilla は同じ仕事を
62 スレッドに配る。個別ループの削減は feature 側の 5.5 ms が上限（すでに 63% 削った）
なので、桁を変えるには並列化しかない。

### ロック待ち（段1の計測、worktree `perf/worldgen-parallel`）

`GEN` は `Mutex<Option<Gen>>` 1 本で、tick・配信・生成が同じロックを奪い合う。
`GenLock` で待ち時間と保持時間を分けて取った（`FEAT_PROFILE` 時のみ、
`gen_lock_contention` は 3 スレッドで「列生成・populate・biome 読み」を模す）:

| | 値 |
|---|---|
| wall（3 スレッド） | 1,484 ms |
| ロック待ち合計 | **1,994 ms** |
| ロック保持合計 | 3,478 ms |
| 獲得回数 | 252 |
| 1 獲得あたり保持 | 13.8 ms（`chunk_cols` 11.5 + populate 7.6 が主） |

待ちはロック帰属時間の **36%**、3 スレッドの wall に直すと **約 45%** が
キュー待ち。つまり残差の大半は「1 本のロックに直列化されている」ことで、
per-chunk の所有に分ければ消える種類の時間。段 2/3 の効果はこの 2 つの
カウンタで「保持の減少」と「待ちの減少」に分けて報告する。

## 現在の所有

- `Gen`（`worldgen::GEN: Mutex<Option<Gen>>`）が世界生成の全状態を持つ:
  datapack 由来の不変データ（`dfs`/`noises`/`FReg`）、チャンク列キャッシュ
  (`col_c`)、feature 書き込み (`feat_c`)、深部状態メモ (`deep_c`)、乱数 (`world_rng`)。
- `chunk_data`（送信）は 1 チャンクについて `terrain_section` → `ensure_populated`
  （3x3 の feature）→ 配列の順に GEN ロック下で実行する。
- `feature::populate` は `FC`（`&mut Gen`）越しに地形を読み、`feat_write` で
  **座標の属するチャンク**の map に書く（生成中チャンクの外にも書く）。

## 目標と不変条件

1. **出力不変**: 同一 seed・同一 datapack で、並列化前後の中身が 1 バイトも
   変わらない（feature/carver なしの block diff 0、feature ありの diff も不変）。
2. **順序決定**: feature の評価順は step と step 内 index、および位置で決まり、
   worker の完了順に依存しない。
3. **世界の正本は 1 つ**: `feat_c` への反映は owner スレッドで、決められた順に
   行う。worker は読み取り専用の入力と、そのチャンクに閉じた出力だけを持つ。
4. **ロックを増やさない**: feature パスに新しいロックを持ち込まない
   （`deep_c`/`col_c` は worker ローカルにするか、読み取り専用スナップショットにする）。

## 設計案

1. **不変入力の切り出し**: `Gen` から datapack 由来のフィールド（`dfs`/`noises`/
   `rules`/`FReg` など）を `Arc<GenData>` にし、`Gen` は `Arc<GenData>` と可変キャッシュ
   を持つ。worker は `Arc<GenData>` を共有できる。
2. **ジョブ単位**: 1 チャンク = 1 ジョブ。
   - 入力: seed、チャンク座標、3x3 のバイオーム集合（読み取り専用）。
   - 出力: そのチャンクの base states（16x16xheight）と、座標がそのチャンクに
     属する feature 書き込みの一覧（`(step, gi, pos, state)` を順序キー付きで）。
   - 地形読みはジョブ内のスナップショット（`ChunkCols` と `base_cell`）で行い、
     `deep_c` はジョブローカルのメモにする。
3. **マージ**: owner がジョブを **チャンク座標順** に取り出し、
   (step, gi, pos) の順で `feat_c` に適用する。worker 間で同じセルへの書き込みが
   競合する場合は、順序キーで最後のものが勝つ（vanilla の逐次実行と同じ）。
4. **ワーカー数**: `std::thread::available_parallelism()` を上限に、既定は
   4-8。`MC_SPINE_GEN_WORKERS` で固定できるようにする（0/1 で逐次）。
5. **失敗時**: worker の panic は owner に伝播し、起動時と同じ fail-closed
   経路で停止する（部分的なチャンクを送らない）。

## 検証

- `tests/verify.sh` が緑（既存の feature capture digest・地形 0 セルを含む）。
- 決定性: 同一 seed・同一座標で、worker 数 1/4/8 の生成結果（チャンクの
  base states と feature 書き込みのハッシュ）が一致するテスト。
- ブロック差分: `chunk_block_diff.py` が feature/carver なしで 0 セルのまま。
- 費用: `load_compare.sh` の `WARM=1` 中央値が x1.10 未満。同一 binary・同一
  ホスト・20 秒窓で前後を取る。x1.10 未満の差は主張しない。
- 並行性: `feat_c` のマージ順を入れ替える変異で決定性テストが赤になること
  （gate が load-bearing であることの証明）。

## リスク

- メモリ: worker ごとの `ChunkCols`/`deep_c` で RSS が増える。上限を決めて測る。
- キャッシュ再利用率: 3x3 の base terrain を worker 間で共有しないと、同じ
  チャンクを複数回計算する。共有するなら読み取り専用の `Arc<BaseChunk>` を
  作る（チャンク単位の地形配列を撤回した理由 — burst での容量 — を再検討する）。
- `world_rng` の消費（構造物・植物の乱数）はジョブ間で共有できない。チャンク
  単位の決定的な子ストリームに分ける契約が必要。

## 最初のコミット

0. （済）feature の地形読みを削る決定実験とメモ化: `b77f1fd`、20.8 → 7.6 ms/chunk。
   WG heightmap は `setBlock` で動くので `set` で列単位に無効化する（`huge_fungus`
   が検出した罠）。mutation gate は `tools/mutation_check.sh` の共有の都合で保留。
1. `GenData`（不変）と `Gen`（可変）の分離。挙動不変、テストのみ。
   ここで GEN ロック待ちの計測（`FEAT_PROFILE` の拡張）も入れる: 並列化の
   効果を「直列仕事の削減」と「ロック待ちの削減」に分けて報告するため。
2. 1 チャンクの生成をジョブ構造体に切り出し、worker 数 1 で逐次実行（挙動不変）。
3. worker 数 >1、マージ順のテスト、`load_compare.sh WARM=1` の前後測定。

各段の出口では、既存の feature capture digest・地形 0 セル・`cargo test` 全体・
mutation check が緑であることを要求する。段 3 の perf 主張は `WARM=1` を
同一 binary・同一ホストの前後比較で取り、x1.10 未満の差は主張しない。

## 2026-10-28: 採用した最適化（この spec の段 2 までの結果）

`FEAT_PROFILE` / `CHUNKPROF` / `LOOPPROF` / `LOCKPROF` / `GENLOCK` と決定論的 bench
（`chunk_delivery_path_bench`, `chunk_delivery_digest`）を常設したうえで、段 1〜2 の
範囲を次のように実装した（すべて既定で有効）。

1. **生成 tick キューを `Gen` から出した**（葉・流体）。tick スレッドは 1 tick ごとに
   それを drain するが、`Gen`（ワールドロック）の下にあると**新しいチャンクの
   populate 待ち**になる。分離後: tick body 13.8 → **0.17 ms/tick**、`GENLOCK wait`
   1451 → 86 ms/100 tick、**20 TPS かつ予算超過 0/100**（以前は 17.5-19.4 TPS、
   8-13/100 超過）。総 CPU は不変（待っていただけだった）。
2. **チャンク単位の状態バンド**（`[min wtop-18, max wtop+18]`、`chunk_data` が
   計算済み states から install、書き込みは write-through patch、per-chunk version）。
   ライト経路のセル読みがワールド評価から配列読みへ: `block_at` 21,619-29,536 →
   **25-46 回/chunk**。
3. **anti-xray 露出テストを手持ちの配列から返す**（同一セクション＋繰り越した上下
   セクション）: `base_state` 6,795 → **482 回/chunk**。
4. **+x 隣チャンクのセクション先読み**（既定 on、`MC_SPINE_NO_PREFETCH_NEIGHBOUR=1` で
   off）: 露出テストが読む深い水平近傍を、隣の配信時に消費される形で先に計算する。
   encode 42 → **29 ms/chunk**（+z と対角まで広げると 12 ms まで下がるが、窓の CPU が
   29.6 → 32.1 s に増えるので +x のみ）。
   - **順序契約への影響を検証した**: 隣を先に計算する＝到着前に populate するので、
     feature 層が見る「先に populate されたチャンク」が変わりうる。`chunk_delivery_digest`
     が 2 seed × 3 掃引方向（60 掃引）で flag off/on の地形 states を比較し、
     **一致**することを assert する（control = 同じ設定 2 回の再現性も assert）。
   - 検証中に**別のバグ**を発見した: 状態バンドが `worldgen::init` を生き延び、
     次の generator の世界に対して前の世界の状態を返していた。`generation_seq` を
     導入して世代が違えばキャッシュを捨てる形にした（これが直る前は上の digest 比較が
     「世界が変わる」と誤って報告していた）。
5. 併せて: `chunk_data` の長い `world()` 保持の除去、ライト BFS の長保持の除去、
   感圧板ループの overlay のみ読み、scheduled-tick のセル読み一括化、advancement の
   イベント索引（全走査の廃止。導入時に自己デッドロックを作り、恒久ガードを追加）。

### 測定の規律（このホストで必要だったこと）

- end-to-end は同じバイナリでも 11.9-29.4 s と振れる（他レーン）。**wall clock の A/B では
  20% 級は分離できない**。→ 決定論的な bench と、負荷非依存のカウンタを常設した。
- 「速く見えた」例が 2 つあり、どちらも**計測の取り違え**だった: (a) 壊れたビルド
  （advancement のデッドロック）が 2.5 倍軽く見えた、(b) bench の 2 パス目だけ warm で
  順序仮説が 40% 良く見えた。どちらも control を足した瞬間に消えた。
