# 実時間境界の接続

## したこと

keepalive・時刻通知・コンテナ数ログをtick除算から実時間の所有者へ接続した。
`every_ms` は前回発火をanchorにした実時間ゲートで、tick位置や周期に影響されない。

- keepalive: `t % 40`（4TPSで10s）→ **15000ms**（vanilla `ServerCommonPacketListenerImpl`）
- 時刻通知: `t % 4`（1s）→ 1000ms。表示同期は既存挙動を保つ
- コンテナ数ログ: `t % 20`（5s）→ 5000ms。診断のみ

周期は250msのまま。keepalive間隔は10s→15sに変わる。これはvanilla準拠の修正であり、
tick除算を残すと20TPSで5sになり、さらにtick位置でずれる。

## 何を証明したか

負の対照：keepaliveを `t % 40` に戻すと、40tickで0x26が発火し失敗
（[negative](time-contract-wallclock-negative.log)）。
正の動作：anchorを周期より過去へ巻くと次のtickでちょうど1通ずつ発火する。
これは「実時間ゲートである」ことの直接的な証拠。

テストはanchorを毎回初期化してから実行する。他テストが昔初期化したanchorを
継承すると1s周期が正当に発火してしまう。復元はassertionの前に行う。

serial/parallelとも23 passed・1 ignored。Fabric/NeoForge checkも合格。
[suite](time-contract-wallclock-suite.log)、[parallel](time-contract-wallclock-parallel.log)、
[features](time-contract-wallclock-features.log)、[review](time-contract-wallclock-review.log)。
独立レビューは共有anchorとassertion順を指摘し、両方修正した。レビューは実行検証していない。

## 残る実時間境界

`Instant` ベースで既に周期非依存なもの。値を変えていない：

- 被ダメ後無敵 `hurt_at` 500ms、shield 5s、pearl cooldown、`last_melee`
- Xp/Drop 消滅 300s、 pickup遅延 500ms、TNT導火線 4000ms

このうちTNT導火線・pickup遅延・Xp/Drop消滅はvanillaが**game tick**で持つ量
（80 / 10 / 6000）であり、tick語へ戻すべきかを20TPS切替sliceで決める。
無敵20残存のような量はgame tick語なので、切替時にvanilla一次参照で確認する。
