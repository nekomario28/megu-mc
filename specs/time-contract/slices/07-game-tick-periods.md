# 07 — ゲーム時間周期の導出

## なぜ

環境ダメージと回復の間隔はtickリテラル（`t % 4`、`t % 2`）で書かれていた。
20TPSではこの除算がそのまま5倍速く発火する。周期の所有者から導出すれば、
実時間の間隔を保ったまま周期変更できる。

## 契約

- `server_tick_period(gt)` がgame tick周期→server tick周期を導出。
- 不変条件は `period * TICK_MS == gt * 50`。vanillaのN game tickはN*50ms。
- 1 server tickより短い周期は0ではなく1になる。moduloの0除算を防ぐ。
- 変換した箇所は値が変わらない。実時間の間隔・発火tick・乱数消費は不変。

対象：プレイヤーのエリトラ耐久・自然回復・溺死・溶岩・炎・虚空・窒息・
サボテン・マグマ・スイートベリー、Mobの窒息と溺水ダメージ。

## ゲート

- 間隔の性質を検証。負の対照は「game tick数をそのままserver tick周期に使う」
  切替の定番ミスで、10 game tickが1000msになり失敗。
- 0/1の周期ガードも検証。
- 現周期での値が旧リテラルと一致することも検証（2・4・16）。
- serial/parallel全テスト、Fabric/NeoForge check、独立レビューを通す。

## 未接続（20TPS切替slice）

- **保存されたtick語の値**：炉progress 40・燃料tick・効果持続・`RAINING`/`THUNDERING` 2400・
  `air` 60・backoff上限16。これらはtick数なので切替時に `GAME_TICKS_PER_TICK` 倍する。
- **1tickあたりの割合**：作物成長 `1/240`・移動・重力。切替時に `GAME_TICKS_PER_TICK` で割る。
  周期の導出と同じ向きではない。ここが「一律5倍」が使えない理由。
- **`TICK_IDX % 6` の液体tick**・observerの6tick。vanillaのランダムtick近似なので
  game tick周期として扱えるか一次参照で確認する。
- **実時間→game tick**：TNT導火線4000ms・pickup 500ms・Xp/Drop 300s・被ダメ無敵。
  vanillaはgame tickで持つ。負荷でtickが遅れると結果が変わるためtick語へ戻す。
- **永続化**：MET4/MET3の残差。旧形式の帰結をテストする。
