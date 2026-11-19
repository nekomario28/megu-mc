# チャンク生成の並列化（R3 の次のスライス）

Status: 段 1〜2 の最適化を実装中。以下の設計案は当初の計画であり、
feature 並列実行や vanilla parity の達成を示すものではない。

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

1. **生成 tick キューの drain をワールドロックから分離した**（葉・流体）。
   キューの内容は各 `Gen` が所有し、runtime は稼働中の世界のハンドルだけを共有する。
   独立した generator や再初期化前の世界の ticks は混ぜない。
   tick スレッドは 1 tick ごとに drain するが、ワールドロックの下にあると**新しいチャンクの
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

### tick キューの検証境界

ロックの分離と世界の所有権は別の契約である。drain が生成を待たないことだけでなく、
独立した generator の tick 順序・重複排除・restore が相互に干渉しないことを検証する。
`src/worldgen.rs` の runtime drain テストは、別スレッドがワールドロックを保持した状態でも
完了することを要求する。`tools/mutation_check.sh` は、キューの共有先を取り違える変異と
ワールドロックを取り直す変異の両方が赤になることを確認する。

上記の速度値は導入時の測定であり、所有権の修正後に再測定した値ではない。
また、記録した populate 順序の replay 一致は、通常の ticket scheduler や
配信パケット・light の vanilla 一致の証明にはならない。

### 測定の規律（このホストで必要だったこと）

- end-to-end は同じバイナリでも 11.9-29.4 s と振れる（他レーン）。**wall clock の A/B では
  20% 級は分離できない**。→ 決定論的な bench と、負荷非依存のカウンタを常設した。
- 「速く見えた」例が 2 つあり、どちらも**計測の取り違え**だった: (a) 壊れたビルド
  （advancement のデッドロック）が 2.5 倍軽く見えた、(b) bench の 2 パス目だけ warm で
  順序仮説が 40% 良く見えた。どちらも control を足した瞬間に消えた。

## 2026-11-18: 段 3 の最初のスライス（terrain のみ、実装済み）

`evidence/foundation/performance/README.md` の "Parallelising the terrain stage" が測定と
ゲートの一次記録。要点だけ:

- **対象は地形ステージだけ**（density / material / aquifer / surface・deep rules）。
  population dispatch・promotion・`hasPostProcess` マーク・feature 順序は不変。
  carvers は引き続き直列（`±8` seed 走査が chunk 境界を越えて書くため）。
- **既定で並列**: `MC_SPINE_GEN_JOBS`（0/1 で直列）を読む。既定は
  `min(available_parallelism, 8)`。dump（このスライスの測定路）は既定で並列、
  配信路（tick スレッド上で走る）は `MC_SPINE_GEN_JOBS` 指定時のみ並列 —
  tick の latency を測っていないため。
- **勝ちは wall clock**: terrain-only の 64 chunk dump が 2.633 → 0.557 s（8 workers、
  4.73x）。CPU は +13%（terrain-only）。full（feature 込み）は 1.15x / CPU +88% で、
  残りはこのスライスが直列のまま残したステージと、worker 側の `cols`/beardifier の
  重複。`MC_SPINE_GEN_JOBS=1` が CPU 重視の比较用の逃げ道。
- **構造キャッシュは per-thread だった**（`STARTS` / `RING_CACHE`）。per-round 共有に
  変えた（process 全体の共有は fixture を壊すため不可、証拠は上記 README）。

## M3 超軽量化 5段階計画（P1〜P5、承認済み）

### 現在のボトルネック整理

- **既に勝っている面**: RSS は vanilla の約 1/8（142 対 1,160 MiB）、tick 本体は 0.05〜0.17 ms/tick（20 TPS、予算超過 0/100）。
- **CPU 1.6〜4.7 倍差の主因**:
  1. 生成済みチャンクを省メモリなパレット済み section として保持せず、ライト（`write_light_arrays` 27〜33 ms/chunk）・anti-xray 深い近傍読み・保存/再送で `base_state` / `terrain_cell` / `block_at` 再計算へ落ちている。
  2. `CARVERS` / `FEATURES`（~56 ms/chunk）が直列のまま。
  3. Terrain Slice 1（`3921aa5` / `1667aa3`）で `full` モード CPU が +88% 増加している（内訳は `terrain` のみ +13% に対し **`full − NOFEAT` が +156%** であり、まず Slice P1a のロード非依存カウンタで主因を確定する）。
  4. 密度・材質・帯水層をセル単位で評価しており、4×8 格子・SIMD バッチ化が未導入。

### 5段階の実行計画（各段でバイト一致・`cargo test`・選択 mutation を必須とする）

> **進捗同期（2026-11-19, `2e95617` 時点）**: `P1a`（`3135354`）・`P1b`（`b588c03`、full CPU ペナルティ完全解消）・`P2a`（`43a463b`、`src/paletted.rs` 単体）・`P3a`（`1befd35`）・`P4`（`06abd5f`、AVX2 4-lane `DfProg`）・`P5-wire/light`（`ea59c4e`、section encode -89% / sky spill -52〜63%）まで統合済み。64 チャンク release dump（`CPUQuota=400%`）は `full`: `j=1 8.07s / j=8 5.48s`、`NOFEAT`: `j=1 3.84s / j=8 1.58s`、`terrain`: `j=1 2.24s / j=8 0.529s`。

| Phase | 対象 | 状態（`2e95617`） | 内容と設計制約 | 出口・効果目標 |
|---|---|---|---|---|
| **P1** | Terrain Slice 2（`P1a` 計測＋`TerrainShare`） | **済** (`3135354`, `b588c03`) | M1〜M5 カウンタ＋round 単位 `TerrainShare`（`bases 64 / beards 128 / cols 128`）＋single-flight `starts`/`rings`＋`Arc` `terr_c` | `full` CPU +88% ペナルティ解消（達成: `j=1 9.78s → j=5 8.78s CPU`） |
| **P2** | 常駐 `PalettedSection` ストア | **P2a 済** (`43a463b`) / **P2b–f 未** | Single/Indirect/Direct パレット section（~48 KiB/chunk）を `src/main.rs` へ接続し、配信（zero-repack）・ライト・anti-xray・feature 近傍読み・tick 書込み・保存/アンロード回収を単一正本化 | `write_light_arrays` 27〜33 ms/chunk と `block_at` 再計算を大幅削減、RSS 低位維持（384 チャンク ≈ 18 MiB） |
| **P3** | `CARVERS` / `FEATURES` の決定性並列化 | **P3a 済** (`1befd35`) / **P3b 未** | P3a（dense `CarveRun` + 3×3 リング並列）は完了。残る P3b（Layer A3）で全バッチ Carver 並列＋Feature 色分け（チェッカーボード）並列を導入 | 直列テール解消・8 コア飽和＋全 fixture バイト一致 |
| **P4** | `DfProg` SIMD / バッチ評価と早期枝刈り | **P4a 済** (`06abd5f`) / **Layer A1+A2 未** | P4a（AVX2 4-lane `DfProg`, full -16%, NOFEAT -29%）完了。残る Layer A1/A2 で Aquifer フラット化・`beard` 削減・SIMD `Perlin`/`NormalNoise`・2D/3D 分離 Corner Grid・AVX-512 8-lane を導入 | `full j=1` 8.07s → ~2.0s、全 arm `to_bits` 完全一致 |
| **P5** | ライト・M2 物理・圧縮配信の仕上げ | **P5-wire 済** (`ea59c4e`) / **仕上げ未** | P5-wire/light 完了。残りは P2 接続後の O(1) 空光スキップ、共有圧縮キャッシュ（将来的に `zlib-rs`/`libdeflate` 任意切替）、大量 Mob SoA、PGO、`WARM=1` 代表負荷比較 | `WARM=1` CPU で vanilla 同等以下、RSS 軽量性維持、20 TPS 維持 |

### 超高効率計算アーキテクチャ計画（Layer A1〜A4、2026-11-19 `2e95617` 基準）

`2e95617` の `M5` 実測（64 チャンク `full j=1 = 8,070 ms`：`beard` 3,285 ms [40.7%]、`cols` 非 beard 2,200 ms [27.3%]、`fill`/Aquifer 1,423 ms [17.6%]、`carver` 1,854 ms [一部 `cols` 重複]、`feature` 595 ms [7.4%]）および Zen 5（Ryzen 7 9800X3D, フル 512-bit AVX-512, 96 MiB L3）の特性を踏まえ、以下の 4 層で**ビット完全一致のまま**計算効率を桁レベルで引き上げる。

| Layer | 対象と具体施策 | 狙い（理論目標・全段 SHA-256 不変） |
|---|---|---|
| **A1: データ構造・アルゴリズム無駄の根絶** | ① **Aquifer `AqChunkGrid` 化**: `aq_rpos_c` の 6,300 万回 SipHash＋16,384 要素 `Vec::remove(0)` を廃止し、チャンク毎 `3×35×3` フラット配列＋`aq_near4` ベクトル化（`aq_lv_ins`/`col_ord` の `Vec::remove(0)` も撤去）。② **`beard` 4 点削減**: `ring_chunks` の `(world_seed, preset)` 純粋キャッシュ化、`beard_chunk_data` の前段 `frequency`/`exclusion` フィルタ＋`adapting_sets` 専用 start 経路、24×24×24 LUT、`rigids.is_empty()` 用 Beard 無し `DfProg`。③ **Biome `u64` ビットマスク化 & `SurfaceProg` コンパイル**: `populate_body` の `Vec<String>`/`HashSet<String>` 比較を `u64` 1 命令化し、`SurfaceRule` をフラット命令列へコンパイル | `full j=1`: **8.07s → ~3.5s**、`j=8`: **5.48s → ~2.0s** |
| **A2: 2 階層 SIMD（AVX-512 8-lane + AVX2 4-lane）** | ① `src/generation_math.rs` に 4/8 レーン SIMD `Perlin` / `NormalNoise`（コーナーバッチ時は TLS `RefCell` `NCACHE` をバイパス、FMA 禁止で IEEE-754 厳密一致）。② `build_cols_body` の `5×5×49` コーナー格子を「2D ノード事前計算（列ごと 1 回）＋3D 本体（`y` 方向 8/4 点 SIMD 一括）」へ分離コンパイルし、インタプリタ＋`HashMap` memo を撤去。③ `DfProg` の AVX-512 8-lane (`__m512d`, `trilinear_x8`) 拡張（runtime dispatch、非対応環境は AVX2 / Scalar へ自動フォールバック） | `full j=1`: **~3.5s → ~1.8–2.2s**、`j=8`: **~2.0s → ~1.1–1.4s** |
| **A3: ロックレス 3 段ウェーブフロント並列化** | ① **Carver 全バッチ並列化**: 3×3 リング（平均 1.6 並列）から脱却し、外周 `10×10` 全チャンクの Terrain 完了後に全チャンクを 8 コア完全並列 carve → 規定順一括コミット。② **Feature チェッカーボード（色分け）並列化**: 書込み半径 `±1` を利用して `(cx % 3, cz % 3)`（安全側 `% 4`）で非交差バッチを完全並列化（明示 `POP_ORDER` リプレイ時は従来の規定順を維持し parity 契約を不変に保つ） | `full j=8`: **~1.1–1.4s → 0.4–0.8s**（6〜12 ms/ch） |
| **A4: ランタイム常駐・配信・大群 Mob・PGO** | ① `PalettedSection`（M3 P2b–f）単一正本配線。② 共有圧縮チャンクキャッシュ（複数プレイヤー再圧縮ゼロ）＋オンライン依存許可時の `zlib-rs`/`libdeflate` 任意切替境界。③ Mob/Entity SoA + 16×16 空間グリッド + 2 フェーズ並列 AI/A*。④ `llvm-profdata` PGO ビルドレシピ | `WARM=1` CPU <= vanilla、20 TPS、RSS ~1/8 維持 |

### OSS / Mod アルゴリズム参照のライセンス境界

- **参照＋設計/コード参考にしてよい許容ライセンス（帰属・ヘッダ遵守）**: `Soju06/hyperchunk`（MIT、`/home/yuu/mc-spine-local/reference/hyperchunk-387c9ad/`）、`valence-rs/valence`（MIT）、`MCHPR/MCHPRS`（MIT）、および `0564ab0bcd3235daa6001033261b79aef5f81de1` 以前の Horizon（MIT）。詳細は [OSS survey](../../evidence/foundation/oss-survey/README.md)。
- **アルゴリズム概念の参照のみ（コードのコピー・翻訳・派生移植は厳禁）**: C2ME、Starlight / ScalableLux、Lithium、Paper / Moonrise、Pumpkin（GPL-3.0）、SteelMC（AGPL-3.0）、`fb1135c6` 以降の Horizon（LGPL-3.0）。

### 大量エンティティ・Mob・Mod 負荷への構造的防衛（M2 / M3 / R5 接続）

[Entity framework](../foundation/slices/04-entity-framework.md) の方針（汎用 ECS の導入自体は目的にしない・G2 の vanilla 挙動不変を守る）を維持したまま、Mob 密集や大規模工場での構造的ラグを次の設計で防ぐ:

1. **連続配列＋細粒度ロック撤去（M2→M3）**: 現行 `mob_tick` のようなエンティティ毎の複数 `Mutex` 獲得を廃し、tick 所有スレッド上の連続配列／データ配列（位置・速度・状態）＋読み取り/更新フェーズ分離へ移行する（測定で比較）。
2. **チャンク／サブチャンク空間グリッド索引**: 押し合い・範囲判定・アイテム回収・ターゲット探索を全件総当たり（`O(N^2)`）から 16×16 セル近傍走査（`O(N)`）へ変える。
3. **P2 + P5 への直結**: Mob の足元・視線・A* 経路探索が `block_at` ロックや地形再計算を踏まず、P2 `PalettedSection` と P5 事前計算済み `VoxelShape`/遮蔽テーブルを O(1) で直接参照する。
4. **AI / 経路探索の 2 フェーズ読み取り並列化**: センサー走査・A* 計算を「不変ワールドスナップショット上の純粋計算」として並列化し、位置・ダメージの確定だけを規定順で直列コミットする（挙動を変えずに計算をマルチコアへ逃がす）。※ Paper 型の遠距離 AI 休止（Activation Range）は挙動が変わるため、将来入れる場合も既定 OFF の明示オプションに分離する。
5. **Interest Management と同期バッチ化**: 視界外・微小変動パケットの同報を間引き・バッチ化し、通信エンコードによる tick 遅延を防ぐ。
6. **R5（NeoForge / Create 規模 Mod）のバッチ JNI 境界**: Create 等の Java バイトコード内計算・GC 自体は JVM shim だけでは消えないため、S3（BlockEntity / tick）以降は 1 セル毎の JNI 往復を避け、チャンク／セクションスナップショットと変更差分のバッチ単位で JNI 境界を跨ぐ契約を採り、同期圧縮・保存・リージョン負荷隔離を Rust 側で吸収する（[shim-plan](../mod-compatibility/shim-plan.md)）。
