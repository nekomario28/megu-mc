# 06 — 実時間境界の接続

## なぜ

keepalive等はvanillaが実時間で定義する。tick除算で書くと周期変更で間隔が変わり、
tick位置でずれる。20TPSでは `t % 40` が5sになり、しかもtick境界に依存する。

## 契約

- `every_ms` が唯一の実時間ゲート。anchorは前回発火の`Instant`。
- keepalive 15000ms（`ServerCommonPacketListenerImpl`）。
- 時刻通知・コンテナ数ログは既存の間隔を保ち、tick除算から実時間へ移すのみ。
- ネットワーク演出を間引かない。時刻通知は既存挙動を維持。
- 周期は変更しない。

## ゲート

- 40tick連続実行でkeepalive・時刻通知が**出ない**ことを検証。
  40と4は旧除数なので、tick実装なら必ず発火する。
- anchorを周期より過去へ巻くと次のtickでちょうど1通ずつ発火することを検証。
  これが実時間ゲートである直接証拠。
- anchorは毎回初期化する。他テストが昔初期化したanchorを継承すると
  1s周期が正当に発火してしまう。復元はassertionの前。
- 負の対照で `t % 40` に戻して失敗を確認。
- serial/parallel全テスト、Fabric/NeoForge check、独立レビューを通す。

## 残る実時間境界（切替sliceで決める）

`hurt_at` 500ms・shield 5s・pearl cooldown・Xp/Drop 300s・pickup 500ms・
TNT導火線 4000ms は既に`Instant`ベースで周期非依存。値は変えていない。

ただしTNT導火線・pickup遅延・Xp/Drop消滅・被ダメ無敵残存は、vanillaが
**game tick**（80 / 10 / 6000 / 20）で持つ量である。実時間で持つと
負荷でtickが遅れたときに結果が変わる。tick語へ戻すかを20TPS切替sliceで
vanilla一次参照に照らして決める。ここでは触らない。
