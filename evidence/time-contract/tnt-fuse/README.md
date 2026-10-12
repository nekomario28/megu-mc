# TNT導火線をgame tickへ

## したこと

`Primed.lit: Instant` を `left: i32`（server tick残数）に変えた。
vanillaは導火線をgame tick（通常80、鎖式は10..30）で持つ。
実時間で持つと負荷でtickが遅れた瞬間に結果が変わる。

- 通常: `server_ticks(80)` = 16 server tick = 4s（不変）
- 鎖式: `chain_fuse_left(jitter)` = 10..30 game tick = 0.5s..1.5s（不変）
- cold領域では**減らない**。旧実装は `lit += TICK_MS` でInstantを戻す荒業だった
- 永続化は残tick数をそのまま書く。旧形式の経過msは読まない

## 実装中の誤り

1. 鎖式の残り時間を `100 - jitter.min(200)` と書き、jitterが0..1023なので
   負値になり `server_ticks` が巨大な値になっていた。0.5s..1.5sの範囲に直した。
2. hot領域で減算する処理を書き忘れ、永久に爆発しなかった。テストが検出。

## 何を証明したか

`tnt_fuse_counts_down_in_server_ticks_and_pauses_when_cold` が
4s導火線が16tick目に爆発すること・coldでは減らないことを本番game_tick経路で検証。
負の対照5種すべて検出（[negative](time-contract-tnt-negative.log)）。

**検出限界を明記する。** 鎖式の1 game tick（50ms）のずれは4TPSでは検出できない。
10も11も2 server tick（500ms）に量子化されるため。250ms以上のずれは検出できる。
20TPSにすれば検出できるようになる。

serial/parallelとも27 passed・1 ignored。Fabric/NeoForge checkも合格。
独立レビューは未実行（外部の利用制限）。

## 残る実時間→game tick

- pickup遅延 500ms → vanilla 10 game tick
- Xp/Drop消滅 300s → vanilla 6000 game tick
- 被ダメ後無敵 500ms → vanilla `invulnerableTime` 20 game tick。
  ただし現在の500msは環境ダメージのクールダウンも兼ねており、
  vanillaの無敵20残存のロジック（>10で弱い再命中を拒否）とは別物。
  vanilla準拠にすると連続被ダメの結果が変わる。
