# 1tickあたりの割合と周期スケジュール

## したこと

**方向が逆になる二種類を区別した。** これが「一律5倍」が使えない核心。

- **保存値・間隔**：`server_ticks(gt) = gt / GAME_TICKS_PER_TICK`（割る）
- **1tickあたりの確率**：`CROP_GROWTH_DENOMINATOR = 30 * TICKS_PER_SECOND`（掛ける）

作物成長を `hh % 120` から導出に変えた。4TPSでは120のままで、1/30毎秒を保つ。
割ってしまうと周期が上がるほど成長が遅くなる。

周期スケジュールをnamed constにした。それぞれがvanilla game tick値を名前に持つ。

| 対象 | 旧literal | game tick | 実時間 |
|---|---:|---:|---:|
| autosave | `t % 120` | 600 | 30s |
| 敵湧みcycle | `t % 40 == 20` | 200 | 10s |
| creeper導火線 | `fuse = 20` | 100 | 5s |
| bobber位置パケット | `age % 3` | 15 | 0.75s |
| 雷の描画寿命 | `(be, 2)` | 10 | 0.5s |

4TPSでの値と実時間はすべて不変。検索で旧literalの残存がないことを確認した
（[remaining-literals.log](remaining-literals.log)）。

## 何を証明したか

6種類の負の対照がすべて検出された（[negative](time-contract-rates-negative.log)）。
作物分母を割る・凍結する、各スケジュールのgame tick値をずらす。

**検出できないものを明記する。** 呼び出し側を旧literalに戻す変異は、
4TPSでは完全に等価なのでどの行動ゲートでも検出できない。保護は
「literalが残っていない」という記録済み検索であり、行動テストではない。
game tick値をずらす変異は検出できる。

serial/parallelとも26 passed・1 ignored。Fabric/NeoForge checkも合格。
[suite](time-contract-rates-suite.log)、[parallel](time-contract-rates-parallel.log)、
[features](time-contract-rates-features.log)。

## 独立レビューは未実行

`codex review` は外部の利用制限（10/18まで）で不通。2slice連続で未実行。
[review-attempt.log](time-contract-rates-review.log)。
コミットはその状態で行い、receiptに `NOT RUN` と記録した。

## 判断が必要な残件

**移動係数はvanillaと一致しない。** 弓矢 `dx*0.9` / `dy-=0.02`、
パール `0.7` / `-0.05`、bobber `0.6` / `-0.04`、Mob `0.22..0.28`。
コメントが「gravity-lite」と自分で言っている。vanillaはper-game-tickで
弓矢 `scale(0.99)` / `-0.05`。実時間あたりの動きが一致しない。

これをvanilla値に直すのは**結果が変わる修正**であり、周期変換ではない。
ユーザーの承認なしに選べない。2つの選択：

1. 現在の近似を実時間で保存する（等価な周期変換を書く）
2. vanillaの係数に置き換える（忠実性が上がるが結果が変わる）

**同様に判断が必要：** 弓矢ttl 240（60s）・パールttl 50（12.5s）・
Mob弓矢ttl 160（40s）・bobberの咬み `age > 15`（3.75s）・雷の描画0.5s。
vanillaは弓矢の1200がinGround消滅カウンタで、生成からのttlは存在しない。
釣りの待機は100..600 game tick（5..30s）。既存値はvanillaと大きく違う。
