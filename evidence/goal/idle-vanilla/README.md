# 目標の測定：vanilla との同条件比較

再現：`bash tools/idle_compare.sh 12 0`

## 条件（goal が挙げる4つを揃えた）

| 条件 | 値 |
|---|---|
| seed | **0**（両方） |
| ワールド種別 | **通常の地形**（両方。下記の修正を参照） |
| view-distance | **4**（両方） |
| simulation-distance | **4**（両方） |
| プレイヤー | **0人**（両方） |
| 測定窓 | 起動完了後 **12秒**、`/proc/<pid>/stat` の utime+stime |

vanilla は公式 1.21.1 バンドラ（`server.jar`、SHA-1 `59353fb4…`、
バージョンマニフェストと照合済み）。リポジトリにある jar は**生の server jar** で、
必要な62個のライブラリ（jopt-simple、brigadier、datafixerupper、authlib）が
環境に無いため動かない。バンドラは自己展開するのでこちらを使う。

## 結果（連続測定）

| idle 12s | CPU (s) | RSS (MiB) | threads |
|---|---|---|---|
| vanilla 1.21.1 | **2.690** | **1094.3** | 63 |
| **mc-spine** | **0.000** | **30.1** | 3 |

vanilla の起動完了は `Done (2.040s)`、mc-spine は
`[wg] vanilla datapack worldgen active (seed 0)`。

## 測定の欠陥を1つ直した（記録）

**最初の版は flat で測っていた。** mc-spine を一時ディレクトリから起動したため
`./datapacks` が読めず、`[wg] worldgen datapack missing — flat generator` に
**黙ってフォールバック**していた。それに気づかず vanilla 側も flat に揃えて
「条件を揃えた」と報告していた。

**実際には両方が別のものを測っていた。** `datapacks`（1052ファイル、6.6MB）は
リポジトリに**存在しており**、cwd から見えていなかっただけ。

修正後、mc-spine は vanilla の noise settings から**同じ地形**を生成する。
影響は大きい：mc-spine の RSS は 5.2 → **30.1 MiB**、vanilla のアイドルCPUは
0.320 → **2.690s**（地形をtickしている分）。**両方とも測定が変わった。**

教訓：**「条件を揃えた」は、揃ったことを確認して初めて言える。**
mc-spine が地形を生成しているかは起動ログで確認できる。

## worldgen の比較はまだできていない

| | 値 | 計測モード |
|---|---|---|
| mc-spine | **9.3 ms/chunk**（441チャンクを 4.09s） | 単一スレッド、専用プロファイラ |
| vanilla | スポーン領域の準備に **1011 ms** | マルチスレッド、チャンク数がログに無い |

**この2つは比較できない。** スレッド数が違い、vanilla 側はチャンク数が
判明していない。並べると誤解を生む。

vanilla 側には**制御されたチャンク生成**が要る（RCON の `/forceload add` を
使うなど）。**これが次の一手**であり、worldgen は両方が完全に実装している
唯一の大きな仕事なので、**同じ仕事の比較**として最も価値がある。

## 示すこと / 示さないこと

**示す**：同seed・同ワールド種別・同視野/シミュレーション距離・プレイヤー無しで、
mc-spine のアイドル RSS と CPU が vanilla より桁で小さい。

**示さない**：
- **同じ仕事をしている比較ではない。** mc-spine は実装している機能が**少ない**。
  差の一部は「安くやっている」ではなく **「やっていない」**である。
- **プレイヤーが居る状態の比較ではない。** chunk送信・entity追跡・戦闘は未測定。
- **worldgen の比較ではない。** 生成はするが、上の理由で数値を並べられない。
- **機能あたりの費用ではない。** 実装済み部分（炉・水・ホッパー）は
  [recipe-cache](../time-contract/recipe-cache/README.md) の x2.52 のように
  前後比較でしか測っていない。

## vanilha 側の worldgen 費用はこの方法では測れない（否定的結果）

`spawn-chunk-radius` で生成チャンク数を制御し、「Preparing spawn area」出現から
`Done` までの CPU を測った：

| radius | チャンク数 | spawn生成 CPU | wall |
|---|---|---|---|
| 2 | 25 | **7.100 s** | 1.030 s |
| 4 | 81 | **6.380 s** | 1.020 s |
| 10 | 441 | **5.790 s** | 0.922 s |

**CPU がチャンク数に比例しない。** チャンクが 25 → 441（17.6倍）でも
CPU は 7.1 → 5.8 s と**減っている**。つまりこの窓の CPU は
**JIT・クラス読込・ワーカープール起動という一回きりの費用**が支配しており、
worldgen の費用ではない。wall も全て約1秒で、差が見えない。

**この数値を「vanilla は441チャンクに6秒CPU」と報告してはいけない。**
そう読むと、mc-spine の 9.3 ms/chunk（=4.09s/441）と桁で比較してしまい、
**両方とも間違い**である。

**必要な方法**：サーバーを**warming した後**に RCON で制御されたチャンク生成を
行い、その窓だけを測る。起動直後の窓は使えない。

## mc-spine 側の数値の但し書き

`MC_SPINE_WG_PROFILE` の 9.3 ms/chunk は
**地形の列のみ**（`chunk_cols` のコメントに "terrain layer only — no features"）。
vanilla の full chunk は地形＋feature＋ライト＋スポーンを含む。
**同じ仕事ではない。**
