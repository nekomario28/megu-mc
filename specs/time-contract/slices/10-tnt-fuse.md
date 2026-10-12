# 10 — 導火線をgame tickへ

## なぜ

導火線を実時間（`Instant`）で持つと、tickが遅れた瞬間に爆発タイミングが変わる。
vanillaはgame tickで持つ。cold領域の一時停止も `lit += TICK_MS` というInstantを
巻き戻す荒業ではなく、単に減算しないだけで表現できる。

## 契約

- `Primed.lit: Instant` → `left: i32`（server tick残数）。
- `TNT_FUSE_GT = 80`（4s）。`chain_fuse_left(jitter)` = 10..30 game tick（0.5s..1.5s）。
- hotでのみ1減算。coldでは減らない。`left <= 0` で爆発。
- 永続化は残tick数をそのまま書む。旧形式の経過msは読まない。
- 4TPSでの実時間は不変。

## ゲート

`tnt_fuse_counts_down_in_server_ticks_and_pauses_when_cold` が本番game_tick経路で
4s導火線が16tick目に爆発すること・coldでは減らないことを検証。
負の対照5種すべて検出。

**検出限界。** 鎖式の1 game tick（50ms）のずれは4TPSでは検出できない。
10も11も2 server tickに量子化される。250ms以上は検出できる。20TPSで検出可能になる。

## 残る実時間→game tick

- pickup遅延 500ms → vanilla 10 game tick
- Xp/Drop消滅 300s → vanilla 6000 game tick
- 被ダメ後無敵 500ms。vanillaは `invulnerableTime` 20 game tickで、
  `>10` の間は同等以下の再命中を拒否する別ロジック。現在の500msは環境ダメージの
  クールダウンも兼ねる。vanilla準拠にすると連続被ダメの結果が変わるので、
  効果や盾との関係を確認してから決める。
