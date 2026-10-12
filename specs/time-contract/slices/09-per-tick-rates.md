# 09 — 1tickあたりの割合と周期スケジュール

## なぜ

切替で最も事故るのは方向の取り違え。保存値は割る、1tickあたり確率は掛ける。

## 契約

- `server_ticks(gt) = gt / GAME_TICKS_PER_TICK`。間隔と保存値に使う。
- `CROP_GROWTH_DENOMINATOR = 30 * TICKS_PER_SECOND`。1tickあたり確率に使う。
  不変条件は `denominator * TICK_MS == 30 * 1000`。毎秒の確率が1/30で不変。
- 周期スケジュールはvanilla game tick値をnamed constで持ち、`server_ticks` で導出。
- 4TPSでの値と実時間はすべて不変。

対象：作物成長の分母、autosave 600gt、敵湧みcycle 200gt、creeper導火線100gt、
bobber位置パケット15gt、雷の描画寿命10gt。

## ゲート

- 各スケジュールについて「旧literalと一致」かつ「実時間が不変」を検証。
- 負の対照6種：作物分母を割る・凍結する、各game tick値をずらす。すべて検出。
- 旧literalの残存がないことを検索し、記録する。
- serial/parallel全テスト、Fabric/NeoForge checkを通す。
- **独立レビューは未実行**（外部の利用制限）。通ったと言わない。

## 検出の限界を明記する

呼び出し側を旧literalに戻す変異は4TPSで完全に等価なので、行動ゲートでは
検出できない。保護は記録済み検索のみ。game tick値をずらす変異は検出できる。
この限界はreceiptに残す。

## 判断が必要な残件

移動係数（弓矢0.9/-0.02、パール0.7/-0.05、bobber0.6/-0.04、Mob0.22..0.28）は
「gravity-lite」近似でvanillaのper-game-tick係数（弓矢 scale(0.99) / -0.05）と
実時間あたり一致しない。選択は2つ：

1. 現在の近似を実時間で保存する等価な周期変換を書く
2. vanilla係数に置き換える。忠実性は上がるが**結果が変わる**

同様に、弓矢ttl 240・パールttl 50・Mob弓矢ttl 160・bobber咬み `age > 15`・
雷描画0.5s はvanillaと大きく違う。弓矢の1200はinGround消滅カウンタで
生成からのttlではない。釣りの待機は100..600 game tick。
これらは周期変換ではなく忠実性の修正なので、ユーザーの承認なしに選べない。
