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

## 2026-10-28: ライト配信経路の内訳（次に削る場所）

`MC_SPINE_CHUNK_PROFILE=1` を sky / block light / floors / `block_at` に分けた。

| 項目 | ms/chunk |
|---|---|
| `write_light_arrays` | 26-98 |
| うち sky light | 25-81 |
| うち block light | 0.9-1.4 |
| うち `sky_floors`（10 回 → キャッシュ後 1.6-2.3 回） | 21-51 |
| `block_at` 呼び出し | **21,619-29,536 回/chunk** |
| うち overlay に当たらず `base_state` に落ちる割合 | **ほぼ 100%** |

原因は `block_at` → `base_state` → `wg2_block` → `worldgen::block_state` =
**セルごとの完全な地形評価（density + material rule + aquifer、GEN ロック）**。
生成済みブロックは overlay（編集のみ）に入っていないので、地形の読みは
必ずこの重い経路に落ちる。1-2 µs/call × 21-29k = 25-50 ms/chunk が説明できる。

- `sky_floors` のチャンク単位メモ化（`FLOOR_EPOCH` で無効化）を入れた:
  呼び出しは 10 → 1.6-2.3 回/chunk に減ったが、`block_at` の総数は 21-29k のまま
  （残りは BFS とセクション詰め）。**壁時計の改善は測定できていない**:
  測定時に他レーンが負荷 12 で動いており、A/B が分離できない。
- 次の一手は「ライト経路の生成状態読みをまとめる」: チャンクの上位数セクションを
  `terrain_section` 相当の一括読みで取り、列ごとの不透明プロファイルを作る。
  セル単位 `block_at` をやめるのが本筋。

### 続き: 不透明バンド（`block_at` を 500-1000 倍削減）

ライト経路のセル読みを、チャンクごとの「不透明バンド」（`[min wtop-18, max wtop+18]`、
`terrain_section` の一括読み + overlay 上書き、`WORLD_EPOCH` で無効化）に置き換えた。

| 項目 | 前 | 後 |
|---|---|---|
| `block_at` / chunk | 21,619-29,536 | **25-46** |
| バンド参照 / chunk | — | 21,597-23,526（配列読み） |
| バンド新規構築 / chunk | — | 1.6-2.0 |
| 1 構築あたり | — | **13.3-15.4 ms**（≈17-20k cell、700-800 ns/cell） |
| sky light / chunk | 25-81 ms | 26-28 ms |

- **構造は直った**が**壁時計は動いていない**。残る費用は「まだ配信されていない
  隣チャンクのバンド構築」2 回 × 14 ms。配信済みチャンクは `chunk_data` が
  計算済みの states から行を install するので無料になる（その分は効いている）。
- バンドのセル単価 700-800 ns は、配信パスが同じセルに払う 200 ns/cell の
  3-4 倍。配信パスは `deep_state_cached`/`aq_lv` が温まっているため。
- 測定時のホスト負荷は 12-17（他レーン）。**A/B の分離はできていない**ので、
  上表の「後」は構造の証拠であって速度の証拠ではない。

次の候補: (a) 未配信チャンクのバンドを作らずに済ませる（spill の窓を
隣の floor と `wtop` から近似せず、そもそも必要セルだけ読む）、
(b) バンドを配信時の states から必ず作る（＝配信順に依存するが、窓の中心は
必ず配信済み）、(c) バンドのセル単価を配信パスと同じにする（同じ memo を共有）。

## 2026-10-28: tick 本体の正体は advancement の全走査だった

上の tick 内訳（`environment` 10.8 ms/tick）をさらに割ったら、`env/effects` =
`effects::tick_effects` → `adv_event(name, "tick")` に 8.5-8.9 ms が集中していた。

`adv_event` は**ロード済みの全 advancement × 全 criteria を毎イベント走査**し、
advancement ごとに `adv_done()` を 2 回ロックし、`pp.entry(id.clone())` で
ID 文字列を clone していた。tick ループは 1 プレイヤーにつき毎 tick `"tick"`
イベントを投げるので、これが tick 本体の大半になる。

修正: イベント名 → (advancement, criterion) の索引を起動時に 1 回作り、
一致する候補だけを visit する（完了判定は従来どおり全 criteria を見る。
criterion が一致しなければ新規完了はありえないので等価）。
`env/effects` の分割計測用に `fx/adv_call` / `adv/total` を追加。

| | 20 秒窓の CPU |
|---|---|
| 修正前（8 回の測定） | 27.9-30.8 s |
| 修正後（2 回） | **12.0 / 12.1 s** |

**ただしこの比較は交絡している**: 同じ 2 回でクライアントが読んだ packet 数が
157-158（修正前は 600-800）で、ホスト負荷（load 10-17、他レーン）でクライアント
自身が遅れていた。`env/effects` の値も 8.1 ms/tick のまま出ている。
つまり「構造として O(all advancements)/tick → O(matching)/tick になった」ことは
確かだが、**CPU 秒の改善幅は静かなホストで再測定するまで主張しない**。

## 2026-10-28: tick 本体の待ちは「世界ロックの長い保持」だった

`TICKPROF` を窓ごとにリセットするように直したら（それまではプロセス起動からの
平均で、アイドル tick 0.04ms が混ざって実測を 1 桁過小評価していた）、
1 tick の内訳はこうなっていた:

| step | ms/tick |
|---|---|
| body | 17.6-20.4 |
| `env/plates`（= 感圧板ブロック） | **17.5-19.6** |
| `plates/on_loop` | 19.6 |
| `generated_fluids` | 0.2-1.0 |

そのループは 1 tick あたり **16 スポット**（プレイヤー + mob）を、感圧板の判定のために
`block_at` で読んでいた。`block_at` は生成済みセルでは `base_state` → 完全な
worldgen 評価（+ GEN ロック）に落ちる。`plate_on=0.00`、`plate_off=0.00` なので
板には一度も乗っていないのに、このループだけで tick 本体が埋まっていた。

さらに `LOCKPROF` で world ロックの待ちが 1172ms（1359 回の取得）と出て、
原因は `chunk_data` が **セクションの 4096 セルを符号化する間ずっと `world()` を
保持**していたこと。tick スレッドは 16 回/tick そのロックに並ぶ。

対処:
- `overlay_block(x,y,z) -> Option<u32>`（overlay だけを見る読み）を追加し、感圧板
  ループは overlay のみで判定（板はプレイヤー設置なので overlay にある。生成物は
  構造物由来だが、このサーバは構造物をまだ置かない）。
- `chunk_data` はチャンクの編集マップを **1 回の短いロックでコピー**し、以降は
  ロック外で符号化。隣接読み（anti-xray の露出判定）は 1 回ごとの短いロックへ。

結果: `plates/on_loop` 19.6 → 10.9 ms、`scheduled_blocks` が 11.5 ms に浮上
（同じ形の待ち）。tick 本体はまだ約 23ms/tick で、**次の対象は `world()` を
ライト BFS の間ずっと保持している `block_light_sections`**、および GEN ロックを
保持し続ける生成パス。

| 20 秒窓の CPU | |
|---|---|
| advancement 修正前 | 27.9-30.8 s |
| advancement 修正後（ロック対処前） | 12.0-12.4 s |
| world ロック長保持の除去後 | 12.3 s |

CPU 総量はこの範囲では動いていない（待ちの場所が移っただけ）が、**tick 本体の
内訳が「待ち」であることが確定した**のがこの段の成果。

### 続き: ライト BFS の長保持を外し、待ちの正体を GEN ロックと確定

`block_light_sections` も `world()` を BFS 全体（1 つの松明で数万回のセル読み）に
わたって保持していたので、3×3 の編集マップを短いロックでコピーしてから読む形にした
（`overlay_band` も同様）。

| step | 前 | 後 |
|---|---|---|
| `env/plates` | 19.6 ms/tick | **0.002** |
| `plates/on_loop` | 19.6 | 0.001 |
| `scheduled_blocks` | 0.1 | **14.5**（待ちが移動） |
| tick body | 17.6-20.4 | 15.1 |

`GENLOCK` を tick 窓でも取るようにした結果（100 tick の窓）:

| | |
|---|---|
| 取得回数 | **2,756,858**（1 tick あたり 27,568 回） |
| 待ち合計 | 1681 ms（1 tick あたり 16.8 ms） |
| 保持合計 | 6450 ms（全スレッド合計） |

つまり tick 本体の残りは、**`scheduled_blocks` がセルごとに `worldgen::block_state`
（= 1 回 1 回 GEN ロック）を呼んでいる**こと。1 tick あたり 2.8 万回の取得と、
配信スレッドの生成パスが GEN を保持している間の待ちが積み上がっている。
次の一手はこのパスを「セクション単位の一括読み」か「per-chunk の状態キャッシュ」に
すること（ライトの不透明バンドと同じ形）。

### 続き: scheduled_blocks のセル単位ロックを一括読みに

`GENLOCK`（100 tick の窓）で **2.76 M 回 / 27,568 回 per tick** の取得と
**16.8 ms/tick の待ち**が見えたので、`scheduled_block_tick` が due な葉 tick ごとに
7 セル（自分 + 6 近傍）を `block_at`（= 1 セルにつき world + GEN の 2 ロック）で
読んでいたのを、**1 回の world ロック節（触れるチャンクの編集マップをコピー）+
1 回の GEN ロック節**（`worldgen::with_gen` の中で `block_state_at`）に置き換えた。
返る値は `block_at` と同一（overlay 優先、生成状態は `base_state` と同じ primitive）。

| | |
|---|---|
| 取得回数 | セル数 × 2 → **2 節/tick** |
| `scheduled_blocks` | 14.5 → 12.5 ms/tick |
| 20 秒窓の CPU | 13.3 / 13.4 / 14.4 s（3 回） |

**総 CPU は動いていない**（この変更の前後で 12.0-14.4 s の範囲に収まる）。
`block_at` の 21k 回削減、感圧板 19.6 ms/tick の除去、advancement の索引化と
合わせて、**構造としての無駄は確実に消えたが、窓の総 CPU はホスト負荷と
クライアント packet 数の変動（157-800）の中では分離できない**。
残るべきは「静かなホストで packet 数を揃えた交互 A/B」で、それまで数値の改善は
主張しない。現在の内訳（`CHUNKPROF`, n=90）: `map_chunk` 91 ms/chunk
（`chunk_data` 63 = 地形 16 + ライト 28 + 残り 19、`write_light_arrays` のうち
sky 26 はバンド構築 24 が主）。

## 2026-10-28: 訂正 — 交絡を取り除いた A/B と、その過程で見つけたデッドロック

`/var/tmp/mc-ab.sh`（交互 A/B、`taskset` で各サーバを 2 コアに固定、client は
`packets` 付きで記録）で、ブランチ開始 `dc62a60` と現在の HEAD を 3 ペア比較した。

| pair | A（dc62a60） | B（HEAD） |
|---|---|---|
| 1 | 30.99 s | 29.30 s |
| 2 | 31.21 | 29.46 |
| 3 | 31.20 | 29.30 |

**差は約 1.7 s（5.5%）**。以前「12 s 対 28 s（2.5 倍）」と見えたのは**計測ミス**で、
B が**デッドロックして仕事をしていなかった**ためだった（A/B で `packets` が
641-802 対 149-157 と食い違っていたのが兆候）。

### 見つけたデッドロック（advancement 索引）

`ef5f96b` で入れた `adv_event_index()` を、**`advancements()` のガードを保持したまま**
呼んでいた。索引の初期化子も同じ `advancements()` をロックするので、
非再入 Mutex の自己デッドロック。最初の `adv_event`（tick ごと・プレイヤーごと）で
サーバが停止し、チャンク配信が 8 パケットで止まる。

- 発見は capture harness（固定チャンクの受信バイト数と packet id ヒストグラムを
  取る `/var/tmp/mc-slow/capture_chunk.py`）。**テスト suite は緑のまま**だった
  （当該コミットで全 suite を回していなかった）。
- 修正: 索引を `advancements()` を取る**前に**構築する。
- 恒久ガード: `adv_event_returns_with_a_loaded_advancement_set`（別スレッドで
  `adv_event` を呼び、10 秒で戻らなければ赤）。**バグを戻すと赤、修正で緑**を確認。

### ついでに判明: 主な wire トラフィックはブロック差分（0x49）

固定チャンクの capture で、何もしないクライアントに **0x49 (MultiBlockChange) が
1,600-2,900 パケット/20 秒**届いていた。これは葉の distance 更新・作物・水などの
毎tick の書き込みが近傍へ配信されているもので、leaf tick が生成のたびに
（葉ごとに）積まれることが主因。vanilla は葉の distance/persistent を
**配置時に決める**ので、この churn は性能だけでなく挙動の差分でもある。
次の対象はここ（`scheduled_blocks` 12.5 ms/tick と 0x49 の大量送信が同じ根を持つ）。

## 2026-10-28: tick 本体の 14 ms は「世界ロック待ち」だった（生成キューを Gen から出す）

先の `sched/eligible` をさらに割ったら、14.1 ms/tick は
`worldgen::with_gen(|g| g.take_generated_leaf_ticks())` の**1 回の取得**に集中していた
（キューには 519 tick/tick しか入っていない）。つまりキューの中身ではなく、
**GEN ロック待ち**。保持しているのは配信スレッドの生成で、新しいチャンクの最初の
セルは `populate`（数十 ms）を GEN 保持のまま走らせる。

対処: 生成時に積む 2 つのキュー（葉 tick・流体 tick）を `Gen` から出し、それぞれ
専用のロックへ（`take_generated_leaf_ticks` / `take_generated_fluid_ticks` は
世界ロックを取らない）。`ProtoChunkTicks` 相当のキューがワールドロックの下に
ある必要はない。

| | 前 | 後 |
|---|---|---|
| tick body | 13.8 ms/tick | **0.17** |
| `scheduled_blocks` | 14.2 | 0.072 |
| `generated_fluids` | 13.6 | 0.002 |
| `GENLOCK wait`（100 tick 窓） | 1451 ms | **86 ms** |
| `[tick]` | tps 17.5-19.4、over_budget 8-13/100 | **tps 20.00、over_budget 0/100、avg_work 0.05-0.10 ms** |
| 20 秒窓の CPU | 28-29 s | 28.1 s（**不変**） |

**CPU は変わらない**（待っていた時間は元々他人が働いていた時間）。変わったのは
**tick の健全性**: 20 TPS を維持し、予算超過が 0 になった。つまり残る 28 s は
配信スレッドの実仕事（生成 + 配信: `map_chunk` 91 ms/chunk）で、そこが次の的。

検証: 全 suite **339 passed / 0 failed / 1 ignored**。
`leaf_edge_schedules_match_native_order`（葉 tick の native 順を固定するガード）は
キューの所有者が変わったため「ケースごとに drain して比較」に適応（意図は不変）。

## 2026-10-28: 決定論的ベンチ（配信経路）と、順序仮説の棄却

このホストの end-to-end は同じコードでも 11.9〜29.4 s と 2.5 倍振れる（他レーン）。
load client の pacing も加わるので、**wall-clock の A/B では 20% 程度の差は分離できない**。
そこで socket を介さない決定論的な尺度 `chunk_delivery_path_bench` を追加した
（`stream_chunks` と同じ順序 — section 地形 → band install → overlay → light arrays —
を固定の掃引で回し、pass ごとに generator を再 init して cold にする）:

| pass | 順序 | mean |
|---|---|---|
| 0 | natural | 30.2 ms/chunk |
| 1 | outer-first | 30.3 |
| 2 | outer-first | 31.0 |
| 3 | natural | 31.0 |

- **「バッチを外周優先で送れば隣の band/floors が温まっている」仮説は棄却**。
  cold 前提では差がない（最初の版は 2 パス目だけ warm で 40% 良く見えた — 計測の
  取り違えで、bench を cold 化したら消えた）。実装は入れていない。
- 同じ bench で「band 読みが on-demand 構築するか」も比較できる（次の判断材料）。

追加した計測: `LOOPPROF`（tick ループの body と sleep — スピンを見る）、
`CHUNKPROF relight`（隣接チャンクの再ライト送信 — 実測 0 回/chunk）。

## 2026-10-28: 配信経路の内訳を「セル単位」まで割った（次の一手の特定）

`CHUNKPROF` に `chunk_data` の phase 分割を足して測った（20 秒窓、seed 0、移動視点）:

| phase | ms/chunk |
|---|---|
| `chunk_data` 全体 | 58-67 |
| ├ **encode ループ**（24 section のうち実地形を通る ~14 の 4096 セル） | **42** |
| ├ biome_section | 2 |
| ├ `section_encode`（パレット詰め） | 0.3 |
| ├ band install / overlay copy | 0.01 |
| └ （残りは fast path と地形 pass 自体） | — |
| `write_light_arrays` | 27（band が全読みを処理: block_at 0 回） |

encode ループ 42 ms の正体は **anti-xray の露出テスト**（`is_hidden` なセル 360/chunk が
6 近傍を読み、`hidden_filler_with` も 6 セル読む → 8,900 読み/chunk）:

| | 前 | 後 |
|---|---|---|
| `base_state` への落下（= セル単位の地形評価） | **6,795** | **482** |
| その時間 | 36.5 ms | 37.3 ms（1 回 **75 µs**） |
| `is_hidden` の per-cell Mutex | 41,643 回 | 0（チャンク単位のスナップショット） |

- 同一セクションと上下セクションの読みを、**すでに手元にある `base` 配列**（および
  1 つ先/前のセクションを繰り越した配列）から返すようにした。世界評価は
  6,795 → 482 に減った。
- 残る 482 は**チャンク境界の水平近傍**で、しかも**深い**セル（鉱石）なので
  surface band の範囲外 → 1 回 75 µs の `base_state`。合計 37 ms。
- 近傍セクションを `terrain_section` でまとめて読む案も試したが **悪化**
  （encode 42 → 48 ms。1 セクション 4096 セルを計算する方が、必要な数セルより高い）→ 撤回。
- `MC_SPINE_NO_XRAY=1` で露出テストを切ると encode は **0.68 ms**。つまり encode の
  実質全部がこのテストで、**次の一手は「深い近傍セルの状態を安く読む」**＝
  フル高さのチャンク状態表現（band の全 y 版、メモリ試算 393 KB/chunk）。

### 追試: フル高さのチャンク状態ストア（撤回）

残る 482 読み（深い水平近傍）を、配信時に公開した**セクション単位の状態ストア**
（LRU 32 chunk ≈ 7 MB、`chunk_data` が計算済みセクションを publish、書き込みは
write-through patch）から返す版を実装して測った。

| | 個別 `base_state` | ストア |
|---|---|---|
| `base_state` への落下 | 482 | 291 |
| encode | 42 ms | **61 ms（悪化）** |
| 追加の `overlay_block` 読み | 0.02 ms | **9.7 ms** |

**撤回**。理由は測定に現れている: 一方向の掃引では**右/下の隣はまだ配信されていない**
のでストアに無く、8,855 読みが毎回ストアの Mutex を取ったうえで結局フォールバックする。
つまりこの経路のコストはキャッシュの形ではなく**順序**（消費する前に隣を materialise して
おくこと）の問題で、しかもその順序は配信順＝チャンク到着順に縛られている。

同じ理由で「隣セクションを `terrain_section` でまとめ読み」も悪化した（42 → 48 ms）。
この 2 つの測定から言える次の形は、(a) 消費側の**先読み**（隣チャンクのセクションを
配信前に materialise する、ただし feature の順序契約を壊さない形で）か、
(b) 露出テスト自体をやめて vanilla の xray 対策（エンジンモードでの難読化）に寄せる、
のどちらか。どちらも別スライスとして設計が要る。

### 追試2: 隣チャンクの先読み（opt-in、既定 off、parity 未検証）

「順序の問題」という結論に対して、配信時に **+x 隣チャンクの同じ y バンドのセクションを
先に計算して公開**し、その隣自身の配信時にはストアから消費する版を
`MC_SPINE_PREFETCH_NEIGHBOUR=1` として実装した（opt-in）。

| | 既定 | 先読み |
|---|---|---|
| `base_state` への落下 | 482 | **59** |
| その時間 | 37.3 ms | 25.5 ms |
| **encode** | 42 ms | **31.4 ms** |

隣のセクション計算は、その隣が配信されるときにストアから消費されるので**総仕事量は
ほぼ同じ**（20 秒窓の CPU は 29.7 s で不変）。効いたのは encode の 11 ms/chunk。

**既定 off のままにした理由（重要）**: 隣チャンクを先に計算することは、そのチャンクを
**到着より前に populate する**こと。feature 層は「先に populate されたチャンクの書き込み」を
読むので、populate の順序は**入力**であり、固定 capture は明示 source 順で取っている。
つまりこれは性能ではなく **parity 契約の変更**で、ユーザーの判断が要る。

parity の影響を実測で確かめようとして capture harness を使ったが、**静止クライアントには
チャンクパケットが送られない**（配信はチャンク移動で起動される）ため、flag on/off の
配信バイトを比較できなかった。→ 検証は「移動する決定的 driver で配信バイトを比較」か
「2 プロセスで `chunk_data` の出力を直接 diff」のどちらかが必要で、まだ行っていない。
したがってこのスイッチは**計測専用**として置き、既定は off。

## 2026-10-28: 先読みの parity 検証 → **別のバグを発見**（キャッシュが generator を生き延びる）

先読み（`MC_SPINE_PREFETCH_NEIGHBOUR`）が世界を変えるかを、決定論的な
`chunk_delivery_digest`（2 seed × 3 掃引方向、地形 states の MD5 を
flag off/on で比較。同じ設定を 2 回回す control 付き）で測った。

**1回目の結果は「変わる」**（seed 60007、chunk (2,-1)）。だが **control も同じ chunk で
落ちた** — つまり同じ設定を 2 回回しても一致しない＝**世界が再現しない**。切り分け:

- `chunk_data` を呼ばず `terrain_section` だけを回すと **control は通る**（地形自体は再現する）
- したがって原因は `chunk_data` が触る**プロセス全体のキャッシュ**

正体: `main.rs` の **state band が `worldgen::init` を生き延びる**こと。前の掃引で
`install_band_rows` が入れた band を、次の（別 generator の）掃引の anti-xray 露出テストが
読み、その band の状態が今の地形と食い違う → filler 判定が変わる → 配信バイトが変わる。

対処: `worldgen::generation_seq()`（init ごとに +1）を公開し、band と section store は
**世代が違えば全体を捨てる**（`BandCache::sync_generation`、store は generation 不一致で miss）。

修正後: **control 通過**、そして **flag off/on は 60 掃引すべてで一致**（`PREFETCH-DIFF 0`）
→ 先読みは（この範囲では）世界を変えない。以前の「parity を変える」という結論は
**この stale cache の artifact だった**ので撤回する。

`chunk_delivery_digest` は恒久ガードとして残す（同じ設定 2 回の再現性を assert し、
先読みが世界を変えたら赤くなる）。

### 採用（既定 on）: +x 先読みと、その CPU 上の位置づけ

| | 既定（=+x 先読み on） | `MC_SPINE_NO_PREFETCH_NEIGHBOUR=1` |
|---|---|---|
| encode | **29-31 ms** | 42 ms |
| `base_state` 落下 | 59 | 482 |
| 20 秒窓の CPU | 29.6-30.4 s | 28.0-29.6 s |

つまり **encode は 13 ms/chunk 速くなるが、窓の総 CPU はほぼ変わらない**（先に計算した
セクションは隣の配信で消費されるため）。M3 は CPU 秒で測るので、この変更は
**レイテンシ側（1 chunk を出すまでの時間）の改善**として記録する。

+z と対角も先読みすると encode は 12 ms まで落ちるが、窓の CPU は 32.1 s に増える
（対角のセクションは次に配信されないことが多い）。→ +x のみ採用。

### 次の一手の特定: anti-xray の露出判定そのもの（ablation で分解）

per-cell のタイマーは**それ自体が測定対象を支配していた**（1 chunk あたり 42,280 回の
`add()` は Mutex + 線形走査で ~700 ns/セル）。タイマーを外し、**ablation**（環境変数で
その部分だけ切る）で分解した:

| 構成 | encode | 読み | うち base_state |
|---|---|---|---|
| 既定 | **28.7-31.4 ms** | 8,862 | 59 |
| `NO_FILLER=1`（`hidden_filler_with` を切る） | **13.9** | 2,038 | 9 |
| `NO_XRAY=1`（露出判定ごと切る） | **0.27** | 0 | 0 |

読みの内訳（カウンタ）: `section=8,120`（同一セクション配列）、`store=477`、
`base_state=59`。つまり**読みはもうほぼ配列読み**なのに、なお 29 ms かかっている。
コストは次の 2 つに分かれる（どちらも ablation で確定）:

- **外側の露出テスト（6 近傍）: ~14 ms/chunk**（隠れセル 360 × 6 読み）
- **`hidden_filler_with`: ~15 ms/chunk**（隠れセルごとに HashMap を作り、近傍が
  隠れていれば**さらに 6 読み + ときどき `base_state`** という入れ子判定を回す）

したがって次の一手は、**セクション単位の露出マスク**（Paper の engine-mode 3 が採る形:
セクションのセルを一度走査して「露出しているセル」のビットマスクを作り、隠れセルと
filler の入れ子判定をそれを引くだけにする）に置き換えること。読みが配列でも、
**隠れセルごとの入れ子走査と HashMap が残っている限り 29 ms は消えない**。

### c1（セクション露出マスク）に進む前の設計メモと、いま測れている制約

ablation で分かったことを、そのまま設計に移す前に整理する。

**確かなこと**
- 露出判定は encode のほぼ全部（29 ms/chunk。切ると 0.27 ms）。
- 内訳は「外側の6近傍テスト ~14 ms」＋「`hidden_filler_with` ~15 ms」。後者は隠れセルごとに
  HashMap を作り、隠れた近傍に対して**6近傍テストを再帰的にもう一度**走らせる。
- 読みの 92% は**すでに手元のセクショ配列**（`section=8,120 / store=477 / base_state=59`）。

**説明できていないこと（設計の前提にできない）**
- 残る 14 ms は 2,038 読みに対して 6.8 µs/read。配列読みなら 2-3 桁小さいはず。
  カウンタ上は `base_state` が 59 回で、既存の測定では 1 回 75 µs（= 4.4 ms）。
  それでも 9 ms 余る。**この 9 ms の正体が分かるまで、マスクの設計根拠が立たない**。
- 候補: (a) `base_state` がこの位相では 200 µs 級（populate の一部を踏む）、
  (b) `HiddenSet::new()` のコスト、(c) `section_of()` の `Option<Box<[u32;4096]>>`
  経由の参照が最適化されていない。

**次の一歩は「マスクの実装」ではなく「残り 9 ms の特定」**。方法は 2 つ:
- `perf` 相当が無いので、ablation をもう一段細かく（外側テストの 6 読みを 1 つに固定、
  `HiddenSet::new` を外に出す、`read` を配列のみに固定）して、どの要素で時間が消えるか見る。
- もしくは `no_xray` 版と既定版で **`base_state` 1 回あたりの費用を直接測る**
  （カウンタで n と ns を同時に取り、1 回あたりを出す）。

その後、Paper の engine-mode 3 に倣う「セクション単位の露出マスク」に進む価値があるかを
判断する。マスクの利点は**入れ子走査の除去**なので、時間が `base_state` 側にあるなら
マスクは効かない（むしろ読みを増やす）。

### c1 の結論: マスクは効かない（コストは「読み」そのもの）

ablation を一段細かくした（すべて per-chunk スイッチ、既定 off。送信内容を変えるので
コミットはしていない）:

| 構成 | encode | reads |
|---|---|---|
| 露出判定ごと切る | 0.26 ms | 0 |
| 近傍ループを回すが `read` を呼ばない | **0.29 ms** | 0 |
| `read` を呼ぶ・filler なし | **13.5 ms** | 2,038 |
| 既定（filler あり） | **29 ms** | 8,862 |

- **ループ構造そのものは無料**（0.29 ms）。時間は `read` 呼び出しにある。
- 1 読みあたり **3.3-6.6 µs**。にもかかわらずカウンタでは読みの 92% が
  `section`（= `base` 配列）経由。配列読みが数 µs かかるのは説明がつかない。

したがって **Paper 型のセクション露出マスクを入れても 29 ms は消えない**。マスクが
消せるのは「入れ子走査」だけで、いま測れているコストは「読み 1 回あたりの費用」側に
ある。マスクはむしろ読みの回数を増やしうる（近傍マスクを作るために全セル×6 を読む）。

**未解決**: 1 読み 3.3-6.6 µs の正体。カウンタ上は配列経路。候補は
(a) `read` クロージャが `FnMut` で、呼び出しごとに最適化が効いていない、
(b) `section_of` クロージャ + `Option<Box<[u32;4096]>>` の二段参照、
(c) `rd`/`add` の atomic 増分が同一キャッシュラインで直列化、
(d) `base_state` がこの位相では 100-200 µs 級（9 回でも ms 級）。
次の一歩はこの特定（例: `read` を単純な `#[inline(always)]` 関数に開き、
カウンタを呼び出し側に出す／`perf` 相当のサンプリングを入れる）。

この branch の作業は他は緑（348 tests、mutation anchor 154/154）。
