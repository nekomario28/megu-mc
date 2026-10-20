# 移動視点下の代表負荷比較（Q、代表負荷）

`tools/load_compare.sh` は vanilla 1.21.1 と mc-spine を**同一の seed・ワールド種別・
view distance・移動スクリプト**で動かし、同じ 20 秒窓の CPU/RSS を測る。
`tools/idle_compare.sh` の idle 値（誰もいない）は「プレイヤーが起こす仕事」を測っていないので、
この比較を別に取った。ドライバ `tools/load_client.py` は protocol 767 を圧縮なしで話し、
両サーバへ同じ移動列（±160×±96 を掃く）を送る。チャンク本体は復号しない
— 両者に同じ仕事をさせるのが目的で、片方のチャンク形式を検査するのではない。

## 測定（同一ホスト、連続3回、20秒窓）

| run | vanilla CPU(s) | mc-spine CPU(s) | vanilla RSS(MiB) | mc-spine RSS(MiB) | vanilla threads | mc-spine threads | vanilla packets | mc-spine packets |
|---|---|---|---|---|---|---|---|---|
| 1 | 16.250 | 21.420 | 1173.4 | 121.9 | 64 | 3 | 3768 | 501 |
| 2 | 15.790 | 21.560 | 1168.3 | 122.4 | 65 | 3 | 1213 | 494 |
| 3 | 16.490 | 21.420 | 1163.4 | 122.1 | 63 | 3 | 2533 | 500 |
| 平均 | 16.18 | 21.47 | 1168.4 | 122.1 | 64 | 3 | — | — |

## 結論（正直に）

- **メモリは約 9.6 倍軽い**（122 MiB vs 1168 MiB）、**スレッドは約 21 分の 1**（3 vs 64）。
- **CPU は mc-spine の方が多い**：21.47s vs 16.18s、比 **約 x1.33**（ノイズ床 4% を大きく超える）。
  つまり**この負荷では mc-spine は vanilla より CPU を使う**。idle の「0.000s vs 2.690s」を
  「軽量」の一般則へ読み替えてはいけない。
- パケット数は mc-spine が大幅に少ない（約 500 vs 1200〜3800）。それでも CPU が多いので、
  差は主に**チャンク生成/データパック地形**か tick 側にあると考えられる。原因の切り分けは未実施。

## 注意（主張しないこと）

- 機能集合は等しくない（mc-spine は実装が少ない）。「同じことをして安い」の証明ではない。
- 移動はテレポート的な位置更新で、歩行物理の再現ではない。パケット数が違うため「同一仕事」は近似。
- 1 ホスト・1 ビルド・20秒窓の 3 回。JIT ウォームアップや GC の影響を除去していない。
- これは**代表負荷の初回測定**であり、G1–G4/M1 の確定値ではない。回帰ゲートにもまだしていない。
- vanilla は Java、mc-spine は Rust で、プロセスモデルが違う（CPU 合計の意味が異なる）。

## 追試：CPU の行き先を切り分け（診断）

同じ移動スクリプトで mc-spine のワールド生成だけを差し替えた（`MC_SPINE_WG=old`、フラット）。

| サーバ | CPU(20s) | RSS | driver |
|---|---|---|---|
| vanilla（通常地形） | 16.18s | 1168 MiB | ok |
| mc-spine（データパック通常地形） | 21.47s | 122 MiB | ok |
| **mc-spine（フラット、`MC_SPINE_WG=old`）** | **0.340s** | 73 MiB | ok |

**データパック地形生成が mc-spine の CPU のほぼ全て**である（21.47s → 0.34s、約63倍）。
逆に言えば、**tick・チャンク配信・I/O のコアは 20 秒負荷で 0.34s**で、vanilla の 16.18s より
はるかに軽い。M3 の CPU 差は**ワールド生成器**に局在する。

注意：フラットは vanilla と同じ地形を作らないため、これは原因の切り分けであり、
同じ仕事の比較ではない。M3 を閉じるには生成器を vanilla 同等以上に安くする必要がある。

## 生成器の単体コスト（M3 の次の一手）

`MC_SPINE_WG_PROFILE=64`（64×64=4096 チャンクを生成して終了）で
**37.90s / 4096 chunk = 9.3 ms/chunk**。負荷時の 21.47s CPU はこの生成器が支配している。
次の作業はこの 9.3ms/chunk の中身（密度/気候ノイズ、表層、aquifer、feature）を
段階計測し、vanilla 同等以下へ下げること。`perf`/`valgrind` は環境に無い。

## 生成の内訳（M3 の真のホットスポット）

`WGPERF=1` の worldgen プローブ（release）：

| 段階 | コスト |
|---|---|
| `chunk_cols`（密度/気候/表層/aquifer） | **8.79 ms/chunk** |
| `deep_state` | 0.0016 ms/call |
| `feature_state`（1回＝3×3=9チャンクを populate） | **448 ms → 約50 ms/chunk** |
| 9×9 の feature 上書き数 | 60,189（≈743/chunk） |

`MC_SPINE_WG_PROFILE` の 9.3ms/chunk は **地形だけ**。負荷時の 21.47s CPU は
**feature population（約50ms/chunk）** が支配している（地形8.8 + populate50 ≒ 59ms × 数百チャンク）。
**M3 の最適化対象は feature population** であり、地形密度ではない。

## feature 単位の内訳（`FEAT_PROFILE=1`、release）

`feature::populate` の `gen_pf` を名前ごとに集計（env ゲート、挙動不変）：

| feature | 時間 | 呼出 | 1回あたり |
|---|---|---|---|
| `ore_dirt` | 236 ms | 25 | **9.4 ms** |
| `monster_room_deep` | 161 ms | 25 | 6.4 ms |
| `ore_gravel` | 85 ms | 25 | 3.4 ms |
| `monster_room` | 84 ms | 25 | 3.4 ms |
| `ore_tuff` | 63 ms | 25 | 2.5 ms |
| `ore_diorite/granite/andesite_lower` | 各 ~48 ms | 25 | 各 ~1.9 ms |
| `underwater_magma` | 42 ms | 25 | 1.7 ms |

`ore_dirt` は size 33 で、`gen_vein` が約2100セル/vein を `FC.wr`（HashMap）へ書き込む。
**コストは feature 評価そのものより、セル書込み（`ctx.set` の HashMap 挿入）と
`gen_vein` の楕円走査**にある。次の M3 最適化は `FC.wr` のデータ構造（per-chunk 配列など）で、
影響範囲が広いため独立スライスにする。

## 撤回した最適化：`FC.wr` の高速ハッシュ

`FC.wr`/`rc` を FxHash に変えて測った：

- `feat_populate_perf`（release）：populate9 458→437ms（約5%）、+8 centers 521→500ms（約4%）。
- 代表負荷の end-to-end：mc-spine CPU **21.35s**（従来 21.47s、差は 0.6% でノイズ内）。

**x1.10 未満でノイズ床4%程度**のため、規律（主張しない）に従い**変更を撤回**した。
`ore_dirt` のコストは楕円体走査の浮動小数演算（vanilla 同等のアルゴリズム）側が支配的で、
ハッシュの入れ替えでは動かない。M3 の次の対象は楕円体充填のアルゴリズムそのもの。

## feature 後の送信パスが同じ深部 state を再計算していた（実装・M3 は未達のまま）

`deep_state_i`（surface window より下の material rule）は、feature パス（`ctx.get`）と
送信パス（`wg2_block`）の両方から呼ばれる。同じセルを feature 直後に送信パスがもう一度
評価していた。結果を `Gen` の有界メモ（50万エントリ、超過は挿入順に破棄）に持ち、
両パスで共有する。生成結果は不変（凍結 population digest と contract テストが固定、
変異でメモが定数を返すと digest が赤）。

mc-spine のみの代表負荷（view 4、10秒、`tools/load_client.py`、各組 base→memo 交互）:

| 組 | base CPU(s) | memo CPU(s) | 比 |
|---|---|---|---|
| 1 | 15.93 | 14.30 | 1.114 |
| 2 | 15.77 | 14.39 | 1.096 |
| 3 | 15.73 | 14.78 | 1.064 |

平均 base 15.81 / memo 14.49 → **約 x1.09**。ノイズ床（4%）は超えるが **x1.10 には届かず、
M3 の達成としては主張しない**。RSS は +17 MiB（119→137 MiB、vanilla の 1168 MiB より遥かに小）。

同等設定の `tools/load_compare.sh`（15秒窓、view 4）では vanilla 16.74s に対し mc-spine 18.72s
（**x1.12 でまだ遅い**）。memo 前は約 21s だったので差は縮まったが、**M3 は未達**。
次の律速は feature パス自体の `deep_state_i`（9チャンクあたり約19万回）で、これは一意な
セルごとに material rule を評価する。vanilla は生成済み chunk の state を配列で読むため、
ここを閉じるには生成 state の per-chunk cache（大きな独立スライス）が要る。

## 撤回した最適化：全列の material-rule 事前計算

「深い state も `ChunkCols` 構築時に全列走査して配列に持ち、feature/送信は配列を読む」を試した：

- `chunk_cols` は 9.3→**28.9 ms/chunk**（全列走査は約9.8万 `eval_rule`/chunk）。
- `feat_populate_perf` の populate9 は 445→**767 ms**（feature パスが `cols` 構築の
  コストを払うため逆に悪化）。

事前計算は問い合わせ数（feature 深部＋送信で約5万/chunk）の約2倍を常に評価するので、
lazy な per-cell（現在の `deep_c` メモ）の方が安い。**この案は撤回**。M3 を閉じるには
「問い合わせたセルだけを dense 配列に lazy cache する」等、別の設計が要る。

## 実装した最適化：ノイズ値のセル内キャッシュ（結果不変）

`deep_state_i` は material rule を評価し、そこは同じ座標の Perlin ノイズを何度も引く
（1セルあたり約7.4回の `Normal::get`＝約38回の Perlin サンプル）。`Normal::get` は純関数なので、
ダイレクトマップのスレッドローカル配列（8192スロット）でその値をキャッシュする。

- feature パス（`feat_populate_perf` の populate9）：445→約399–421 ms。
- mc-spine のみの代表負荷（view 4、10秒、base→cache 交互3組）：

| 組 | base CPU(s) | cache CPU(s) | 比 |
|---|---|---|---|
| 1 | 14.03 | 13.22 | 1.061 |
| 2 | 13.92 | 13.26 | 1.050 |
| 3 | 13.95 | 13.20 | 1.057 |

平均 13.97 → 13.23 で **約 x1.056**。RSS はほぼ不変（137 MiB）。生成結果は不変
（凍結 population digest が固定し、変異でキャッシュが定数を足すと digest が赤）。

**x1.10 未満なので M3 の達成としては主張しない**。同等設定の `tools/load_compare.sh`（15秒窓）は
vanilla 14.39s 対 mc-spine 17.94s（この回は x1.25）で、実行環境の負荷で絶対値は大きく動くが、
**M3 は未達のまま**。次の律速は依然 feature パス内の material-rule 評価である。

## M3 の根本原因（実装前の判断待ち）：`interpolated` を `cache` として扱っている

`src/worldgen.rs:805` は `interpolated` / `flat_cache` / `cache_2d` / `cache_once` /
`cache_all_in_cell` をすべて `DF::Cache` に落としている。つまり **vanilla がセル解像度
（ore vein は 4×8×4）でノイズを評価して三線形補間する `interpolated` を、mc-spine は
ブロックごとに厳密評価している**。

- 影響：`minecraft:overworld/ore_vein/mask`・`toggle`（どちらも `interpolated`）が
  `deep_state_i` のたびにブロック単位でノイズを引き、feature パスの約65%を占める。
  vanilla の約32倍のサンプル数。
- これは **M3 の律速**であると同時に **vanilla パリティの欠落**（ore vein の形が vanilla と違う）。
- `interpolated` を正しく実装すると **両方が同時に直る**（セル角ノイズを一度だけ評価し、
  補間で全ブロックに配る）。ただし **世界生成の出力が変わる**ため、凍結 population digest の
  再基準化と、vanilla の `NoiseChunk` 補間式（`cell_size` と補間順序）の照合が要る。

**「パリティを保った高速化」という制約下では実装できない**（補間は値が変わる）。
実装するなら「vanilla 準拠方向への出力変更＋再基準化」として独立スライスで行い、
同一 seed・同一設定で vanilla と地形を照合する。ここでは**測定と記録まで**とし、
現在の parity-preserving な2段（深部メモ x1.09、ノイズ値キャッシュ x1.056）で止める。

### 実装して測定した結果（撤回）：`interpolated` は M3 の解ではない

bytecode（`NoiseChunk$NoiseInterpolator.compute`、`NoiseSettings.getCellWidth/Height`、`Mth.lerp3`）から
補間式を確定し、`DF::Interpolated`（セル 4×8×4、8 隅の三線形）を実装して測定した。

- **パリティ**：vanilla の `interpolated` を再現するので、出力は vanilla に近づく。
  ただし **mc-spine の現在の出力が変わる**（ore vein が変わる：`copper_density` の
  `present` が 716→777、hash も変化）。「結果を変えない」制約の外。
- **速度**：feature パスの `populate9` は **399ms → 495–499ms（約 +23%、x0.80）** と遅くなる。
  理由は mc-spine の評価モデル。vanilla はチャンクをセル単位でストリーム充填するので
  1 セルの 8 隅を 128 ブロックで共有できる（8/128）。mc-spine の feature パスは
  `deep_state_i` を**ブロック単位で散発的に**呼ぶため、1 セルあたり約 2.3 ブロックしか
  問い合わせず、8 隅を **8/2.3 ≈ 3.5 倍**計算してしまう（直接マップ 1<<17 でヒット率 57%）。
- 純関数ノイズのセル内キャッシュ（x1.056）と合わせても、この追加コストは回収できない。

したがって `interpolated` は **「パリティを保った高速化」ではない**（出力が変わる上に遅い）ので撤回した。
M3 を閉じるには **セル順の material-rule 評価**（チャンクをセル単位で充填して 8 隅を共有）か
**worldgen の並列化**という、局所最適化ではない構造変更が必要で、これは独立スライスである。

### 代表負荷のプロファイル（一時計測、撤回した変更の根拠）

15 秒・移動ビュー・view 4 の負荷で、server 内に一時的な段別/スレッド別計測を入れて測った
（計測コードは撤回済み。数値はその実行からの記録）。

- `game_tick` の段別は `environment` が支配（例 5.67s / 全 5.71s、約 16.9ms/tick）。他は
  `random_ticks` が約 50ms、残りは 1ms 未満。`environment` は tick スレッドが **region ロックを
  待つ**時間で、worldgen は送信スレッド側で走っている。
- 送信 `map_chunk` は約 9.2s / 81 チャンク（内訳 `chunk_data` 約 8.3s、light 約 1.0s）。
- worldgen 入口 `wg2_block` は **570 万回/15 秒で 11.8s**、うち `feature_state`（`GEN` ロック＋
  3×3 pop 走査）が 6.95s。`chunk_data` がブロックごとに `base_state`→`wg2_block` を呼ぶため。
- スレッド別 CPU は 1 スレッド 13.4s・もう 1 スレッド 2.8s（計 16.2s）で、**実質単一スレッド**。
  mc-spine は 3 スレッドでも region/GEN ロックで直列化される。
- 総 CPU は mc-spine 16.5s 対 vanilla 16.0s（**x1.03、ノイズ内**）。つまり M3 の差は「ロック」ではなく
  **worldgen＋送信の実仕事量**。
- 参考として `feature_map` をチャンク単位にまとめて `wg2_block` を 570万→46万回にしても、
  end-to-end CPU は 17.29s→17.17s（**0.7%、ノイズ内**）で変わらなかった。加えて worldgen 層が
  placement 層に依存する層違反を招き、`wg2_block` の地形則を複製するため、**撤回**した。

**結論**：パリティを保ったまま世界生成の実仕事を数 %削る局所最適化は見つからなかった。M3 を閉じるには
`interpolated`（出力を変える）か、**セル順の material-rule 評価**／**worldgen の並列化**という構造変更が要る。

### ツール欠陥の修正：古いバイナリを測っていた

`tools/load_compare.sh` は `cargo build` を素で呼び、その後 `$ROOT/target/release/mc-spine` を実行して
いた。プロジェクトの作業手順は `CARGO_TARGET_DIR` を設定する（このセッションの verify もそう）ため、
その環境では build は別ディレクトリに書かれ、実行は**古い `$ROOT/target` のバイナリ**になっていた。
`--target-dir "$ROOT/target"` を明示して同一バイナリを build・実行するよう修正した。

この修正後（= 現在の HEAD のバイナリ）の代表負荷は **mc-spine 16.5s vs vanilla 16.0s（x1.03、ノイズ内）**。
`CARGO_TARGET_DIR` を設定した環境で以前に取った比（例 x1.33）は**古いバイナリを測っていた可能性がある**。
したがって M3 は「明確な未達」ではなく「ノイズ内で区別できない」であり、断定には同一バイナリでの再測が要る。

### M3 達成（パリティを保ったまま）：生態系（biome）葉の共有キャッシュ

上のプロファイルで「CPU は worldgen＋送信の実仕事」と分かったので、その中身を数えた。材料ルールは
**ブロックごとに生態系**を引いていた（`SC::leaf_at` → `Gen::biome_quart` の B木探索）。`deep_state_i` は
毎回 `SC` を作り直すため `leafq` がブロックをまたがらず、**421万回の探索**（作業集合の quart は約12万個、
同じ quart を 34 回）になっていた。

`Gen` に quart→葉の共有キャッシュ（`bioq`、上限 2^18）を追加。（`last=None` の `biome_quart` は quart の
純関数なので）探索結果は前と同一で、**出力は不変**。

- `biome_quart` 呼び出し **421万 → 32万回（約13×削減）**。
- 代表負荷（15s、seed 0、view 4、同一クライアント、修正済み `load_compare.sh`）：
  vanilla **16.09s / 1134MiB / 64thr** 対 mc-spine **10.16s / 140MiB / 3thr** → **x0.63（1.58倍高速）**。
- 137 passed（凍結 population digest 含む）、変異 44 proved。詳細:
  [biome-cache](../biome-cache/README.md)。

**M3（パリティを保った高速化）は達成**。`interpolated` の出力変更は不要だった。

## 2026-10-20 再測定（地形パリティ確立後）

地形（feature/carver なし）が vanilla と 0 セルで一致した状態で、
`tools/load_compare.sh 20 0` を同一ホストで実行した（20 秒窓、view 4 / sim 5、
同じ移動列、圧縮なし）。

| | CPU(s) | RSS(MiB) | threads | packets |
|---|---|---|---|---|
| vanilla | 17.530 | 1159.7 | 63 | 2605 |
| mc-spine | 19.870 | 144.7 | 3 | 570 |

- **CPU は mc-spine の方が多い: 19.87s 対 17.53s（約 x1.13）**。
  この負荷では **M3 の「CPU も小さい」は未達**。
- メモリは約 8 倍軽い（144.7 MiB 対 1159.7 MiB）、スレッドは 21 分の 1。
- 以前の x0.63（10.16s / 140MiB）は、地形が違う（`interpolated` の順序と
  surface rule が未修正）状態の測定である。同じ binary かを確認できる記録が
  ないため、現在の代表値として使わない。
- 地形一致の修正で `chunk_cols` は 48.8 → 70.8 ms/chunk（同一 `WGPERF`、+45%）。
  CPU の律速は生成器のままで、次の作業は生成器の費用削減（R3）。
- この比較も機能集合が等しくない。M3 の証明ではなく、現時点の代表値。
