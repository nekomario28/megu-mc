# Drop/Xpの寿命とpickup遅延をgame tickへ

## したこと

`Drop.age` / `Xp.age: Instant` を `i32`（server tick）に変えた。
`Instant` は自動で進むため、tickが遅れた瞬間に消滅タイミングが変わっていた。

- pickup遅延: 500ms → `server_ticks(10)` = 2 tick（不変）
- Drop寿命: 300s → `server_ticks(6000)` = 1200 tick（不変）
- Xp寿命: 300s → `server_ticks(6000)`（不変）
- cold領域では**加齢しない**。未読込のワールドのアイテムが消えない
- 永続化はtick数をそのまま書む。旧形式の経過msは `ms / (1000/TICKS_PER_SECOND)` で換算

## 発見した事前からの順序の非対称

Xpの加齢は `keepalive_time_tick`（environment stage）にあり、`refresh_hot` より
**前**に実行される。Dropの加齢は `drop_tick` で `refresh_hot` より後。
したがってXpは領域境界で1tick遅れる。これは今回導入したものではなく
元々のstage順序。テストはこの実態（Some(4)）を明示して固定している。
隠さず、vanillaが両方game tickで持つ点と併せて記録する。

## 何を証明したか

`entity_life_and_pickup_delay_count_server_ticks_and_freeze_when_cold` が
加齢・cold停止・寿命消滅・pickup遅延を本番game_tick経路で検証。
負の対照5種すべて検出（[negative](time-contract-life-negative.log)）。

serial/parallelとも28 passed・1 ignored。Fabric/NeoForge checkも合格。
独立レビューは未実行（外部の利用制限）。

## 残る実時間→game tick

被ダメ後無敵 500msのみ。vanillaは `invulnerableTime` 20 game tickで、
`>10` の間は同等以下の再命中を拒否する別ロジック。現在の500msは環境ダメージの
クールダウンも兼ねる。vanilla準拠にすると連続被ダメ・盾・効果との結果が変わる。
