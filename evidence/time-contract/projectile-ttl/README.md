# 飛翔体TTLの実時間復元 — 切替時の報告誤りを修正

## 何が起きていたか

20TPS切替のとき、TTLは「現在の実時間を保つ」側を選ぶと**報告した**。
しかし実際には生のtick数のまま残していた。

    ttl: 240  →  4TPS では 240 × 250ms = 60s
                  20TPS では 240 ×  50ms = 12s

弓矢の寿命が60sから12sになった。パールは12.5s→2.5s。
**報告と実装が食い違っていた。**

## 修正

vanilla game tick値として明示し、`server_ticks()` で導出する。
4TPSの実時間をそのまま game tick に直しただけなので、切替前の挙動に戻る。

| 対象 | 旧（4TPS tick） | game tick | 実時間 |
|---|---:|---:|---:|
| 弓矢 | 240 | 1200 | 60s |
| Mob弓矢 | 160 | 800 | 40s |
| パール | 50 | 250 | 12.5s |
| 雪玉 | 60 | 300 | 15s |

**vanilla準拠の撤去はしていない。** vanillaは飛翔ttlを持たず、
1200は接地中の消滅カウンタ。撤去は結果が変わりエンティティが蓄積するので別slice。

## なぜ見つかったか

記録しておいた検索パターンで `ttl: [0-9]+` を洗った。
これは同じ種類の**3例目**（溶岩 `t % 2`、air `60`、今回のttl）。

**「実時間を保つ」と宣言した変換は、宣言どおりか検索で確認すること。**
宣言だけでは実装が追いつかない。

## ゲート

`projectile_lifetimes_keep_their_real_time_at_any_cadence` が
各TTLの実時間を検証。負の対照：`ARROW_TTL_GT` を 240 に戻すと
`arrow no longer spans 60000ms / left: 12000` で失敗。

serial/parallelとも35 passed・1 ignored。Fabric/NeoForge checkも合格。
独立レビューは未実行。
