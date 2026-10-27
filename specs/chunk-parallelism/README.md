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

未計測（実装時に取る）: サーバ実行時の **GEN ロック待ち**。現在のロックは
`Mutex<Option<Gen>>` 1 本で、tick スレッドと配信スレッドが同じロックを奪い合う。
並列化の第一段（`GenData`/`Gen` 分離）でこの待ちが減る量も測る。テスト内は
単一スレッドなので、この値はまだ無い。

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
