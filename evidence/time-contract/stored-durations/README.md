# 保存tick語の値の導出

## したこと

保存されるtick語の値を、vanillaのgame tick値から導出する形にした。
`server_ticks(gt) = gt / GAME_TICKS_PER_TICK`。周期を変えても実時間は変わらない。

- 炉の精錬完了: `40` → `server_ticks(200)`（vanilla 200 gt = 10s）
- 燃料: 炭/木炭 `320` → `server_ticks(1600)`、原木・板材 `60` → `server_ticks(300)`、
  棒 `20` → `server_ticks(100)`、炭ブロック `3200` → `server_ticks(16000)`
- 空気: `60` → `server_ticks(300)`（15s）
- 雨: `2400` → `server_ticks(12000)`（600s、vanillaの下限）
- 雷: `2400` → `server_ticks(3600)`（180s、vanillaの下限）
- backoff: `1 << fails` → `backoff_ticks(fails)`（10..80 game tick = 0.5s..4s）

**雷は実害の修正。** 旧実装は雨と同じ2400（600s）を使っていた。vanillaの雷は
3600..15600 game tickなので、600sではなく180sが下限。

## 実装中の誤り

最初 `server_ticks` を `period * GAME_TICKS_PER_TICK` と書いてしまい、
炭の燃料が320→1600 server tick（実時間400s）になった。既存の炉回帰テストが
検出した。除算が正しい。教訓：導出関数の両方向の誤りを別々の負の対照で潰した。

## 何を証明したか

- 実時間の性質 `server_ticks(gt) * TICK_MS == gt * 50` を10..16000 gtで検証。
- 現周期での値が旧リテラルと一致することも検証（40・320・3200・60・2400）。
- 負の対照：割る代わりに掛けると10 gtが12500msになり失敗
  （[negative](time-contract-stored-negative.log)）。
- backoffの梯子が0.5s..4sを保つことも検証。

serial/parallelとも25 passed・1 ignored。Fabric/NeoForge checkも合格。
[suite](time-contract-stored-suite.log)、[parallel](time-contract-stored-parallel.log)、
[features](time-contract-stored-features.log)。

## 独立レビューは未実行

`codex review` が**利用制限で中断**した。実行ログは
[review-attempt.log](time-contract-stored-review.log)。
このsliceは独立レビューのゲートを通っていない。コミットはその状態で行い、
receiptに `NOT RUN` と記録した。後続sliceで再実行すること。

## 残る切替作業

- **1tickあたりの割合**（割る方向）：作物成長 `1/240`、移動・重力・摩擦、雷の寿命。
  こちらは `GAME_TICKS_PER_TICK` で割る。周期の導出と向きが逆。
- **実時間→game tick**：TNT導火線4000ms・pickup 500ms・Xp/Drop 300s・被ダメ無敵。
  vanillaはgame tick。負荷でtickが遅れると結果が変わる。
- **永続化**：MET4/MET3の残差。旧形式の帰結をテストする。
- **効果の二重単位**：`Eff.left` はserver tick、wireはgame tick。game tickで持つと
  wireの変換が消える。効果が永続化されるかを確認してから決める。
