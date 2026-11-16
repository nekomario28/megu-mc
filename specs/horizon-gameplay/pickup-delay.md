# pickup delay — 実クライアントの回収に既存の遅延規則を届ける

## なぜ

1.21.1 の `ItemEntity` は spawn/throw 直後の一定tick間、プレイヤーに取られない。
このツリーはその規則を `PICKUP_DELAY_GT = 10`（`server_ticks` でserver tickへ換算）として
既に持っており、`.age` は `drop_tick` の加齢段が増やしている。

ところが規則を問い合わせていたのは擬似 player の `actor_tick` だけで、
実プレイヤーの回収経路 `drop_tick` は近接候補を位置だけで選び、`age` を見ていなかった。
結果として、spawn したtickに立っているクライアントがその場で回収できた。
同じ entity を actor は拒否する。規則は1つなのに消費者が2つあり、片方だけが問い合わせていた。

## 契約

- 判定は `Drop::pickup_delay_elapsed()` の1箇所。`age >= server_ticks(PICKUP_DELAY_GT)`。
  actor の吸収と実プレイヤーの回収の両方がこれを呼ぶ。
- 判定は **live な entity** に対して、回収のロック順（inventory → drops）の中で行う。
  候補列は位置のsnapshotであり、その後にspawnしたdropや、より新しい `age` に統合された
  dropを正しく表さない。snapshot側の絞り込みは最適化であって契約ではない。
- `age` の増加は既存の加齢段（回収の後）のまま。しきい値も単位も `TICK_MS` 由来の
  換算も変えない。10→40 のような値の変更、server頻度の変更、所有者別の遅延は行わない。
- 挿入規則そのもの（選択hotbar → offhand → 通常inventory、部分挿入、残量、`Inventory.add`
  の戻り値と通知）は変更しない。49ケースの実クラスfixtureが固定している。

## 検証

- 新規: `pickup_tick_refuses_a_drop_inside_its_pickup_delay`（age 0 としきい値-1 の拒否、
  そのtickで `age` がしきい値へ進むこと、次のtickで回収できること、拒否中はinventory・
  entity・socketが無変化）、`pickup_tick_collects_at_the_delay_and_reports_the_original_count`
  （しきい値ちょうどで全量回収、0x15 / 0x6f / 0x42 と 0x6f が運ぶ元個数）、
  `pickup_tick_delay_boundary_keeps_the_partial_insert_and_leftover`（部分挿入と残量6、
  0x15 / 0x58、take通知なし）、`pickup_tick_delay_keeps_the_next_player_eligible`
  （満杯の1人目が独占せず、aged後に2人目が受け取る）。
- 既存: `pickup_tick_drains_across_selected_offhand_then_hotbar` ほか pickup fixture、
  `entity_life_and_pickup_delay_count_server_ticks_and_freeze_when_cold`。
- `tools/mutation_check.sh` に1件追加。guard を外すと上記の拒否テストが赤になること。

## 未閉塞

所有者制限（thrower の UUID と `canPickup`）、`ItemEntity.playerTouch` 自体の実行、
接触範囲（AABB と entity 寸法）、spectator、scheduler、統計／advancement、
擬似 player と実プレイヤーの inventory 表現統合。擬似 actor の吸収は
「同じ遅延を問い合わせる」以外の意味論を変えていない。

永続化は既存形式（`Drop.age` を tick 数で保存し、負値もそのまま往復する）を再利用しており、
形式・schema の変更はない。
