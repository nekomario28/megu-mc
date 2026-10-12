# 11 — Drop/Xpの寿命とpickup遅延をgame tickへ

## なぜ

`Instant` はtickが遅れると勝手に進む。vanillaはentityのageをgame tickで持つ。

## 契約

- `Drop.age` / `Xp.age: Instant` → `i32`（server tick）。
- `PICKUP_DELAY_GT = 10`・`DROP_LIFE_GT = 6000`・`XP_LIFE_GT = 6000`。
- hotでのみ加齢。coldでは加齢しない。
- 永続化はtick数。旧形式の経過msは `ms / (1000/TICKS_PER_SECOND)` で換算。
- 4TPSでの実時間は不変。

## ゲート

加齢・cold停止・寿命消滅・pickup遅延を本番game_tick経路で検証。
負の対照5種すべて検出。

**既存の順序非対称を固定する。** Xp加齢は `keepalive_time_tick` にあり
`refresh_hot` より前。Drop加齢はその後。Xpは領域境界で1tick遅れる。
元々のstage順序であり今回の導入ではない。テストはSome(4)として明示固定。

## 残る実時間→game tick

被ダメ後無敵 500ms。vanillaは `invulnerableTime` 20で、`>10` の間は
同等以下の再命中を拒否する別モデル。現在の500msは環境ダメージのクールダウンも
兼ねる。準拠すると連続被ダメ・盾・効果の結果が変わる。
