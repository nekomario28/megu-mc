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
