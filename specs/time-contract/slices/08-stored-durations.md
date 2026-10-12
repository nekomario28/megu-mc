# 08 — 保存tick語の値の導出

## なぜ

炉・燃料・空気・天候・backoffはtickリテラルで保存されていた。切替時に
「一律5倍」を適用すると実時間が5倍になる。vanilla game tick値から割って導出すれば、
周期を変えても実時間は不変。

## 契約

- `server_ticks(gt) = gt / GAME_TICKS_PER_TICK`。0にはならず1で止める。
- 不変条件は `server_ticks(gt) * TICK_MS == gt * 50`。
- 定数はvanillaのgame tick値そのものを書く。精錬200・炭1600・炭ブロック16000・
  原木/板材300・棒100・空気300・雨12000・雷3600。
- 保存されるcounterの単位はserver tickのまま。単位は変えない。
- 雷は雨と別の値を持つ。旧実装は同じ2400を使っており、これは実害の修正。

## ゲート

- 実時間の性質を10..16000 game tickで検証。
- 現周期での値が旧リテラルと一致することも検証。回帰がないことの証拠。
- backoffの梯子が0.5s..4sを保つことも検証。
- 負の対照は掛け算にする誤り。既存の炉回帰テストも実装中の同じ誤りを検出した。
- serial/parallel全テスト、Fabric/NeoForge checkを通す。
- **独立レビューはこのsliceでは未実行**（`codex review` が利用制限で中断）。
  通ったと言わない。後続で再実行する。

## 残る切替作業

- **1tickあたりの割合**（割る向き）：作物成長 `1/240`・移動・重力・摩擦・雷の寿命。
  `GAME_TICKS_PER_TICK` で割る。ここが「一律5倍」が使えない核心。
- **実時間→game tick**：TNT導火線4000ms・pickup 500ms・Xp/Drop 300s・被ダメ無敵残存。
- **永続化**：MET4/MET3残差。旧形式の帰結をテストする。合成fixtureを使う。
- **効果の二重単位**：`Eff.left`（server tick）とwire（game tick）。
  game tickで持てばwire変換が消える。効果の永続化の有無を確認してから決める。
