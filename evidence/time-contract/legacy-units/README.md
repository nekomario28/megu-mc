# 旧MET2/MET3の単位変換（レビュー指摘の修正）

## 指摘

レビュー（issue #7）：
> old MET2/MET3 world counters beyond Drop/XP age shorten 5× on restart.
> loader converts old Drop/XP age (raw/TICK_MS) but uses raw persisted ttl for
> arrows/pearls, left for primed TNT, and raw RAINING/THUNDERING counters.
> Furnace burn_left/progress are also written/read as raw server ticks.

そのとおりだった。MET4化したのは drop/xp age だけで、
矢・パールの `ttl`、TNTの `left`、天候カウンタ、炉の `burn_left`/`progress` は
生のままだった。

## 途中で2回間違えた

**1回目：全counterを `raw / TICK_MS` で割った。**
`/ 50` になり、count系は5倍**短く**なった。指摘された回発と同方向。

**2回目：`GAME_TICKS_PER_TICK` を掛けた。**
これは game tick と server tick の比で、20TPSでは **1**。
掛けても無変化だった。旧形式の基準は「4TPSのtick」であって
「game tick」ではない。別の量。

## 正しい区別

旧形式には**2種類の単位**が混在していた。出典は `main` の旧実装：

| フィールド | 旧実装の書込 | 単位 | 変換 |
|---|---|---|---|
| drop age / xp age / TNT lit | `elapsed().as_millis()` | **ミリ秒** | `raw / TICK_MS` |
| arrow ttl / pearl ttl | `a.ttl.to_be` | **4TPS tick** | `raw * (250 / TICK_MS)` |
| furnace burn_left / progress | `fr.burn_left.to_be` | 4TPS tick | 同上 |
| RAINING / THUNDERING | `load().to_be` | 4TPS tick | 同上 |
| primed TNT left | `a.left.to_be` | 4TPS tick | 同上 |

`LEGACY_TICK_MS = 250` を導入した。**これはファイル形式の性質であって
実行中のサーバーの性質ではない。** `TICK_MS` から導出してはいけない。

## ゲート

`legacy_persisted_durations_resume_with_their_original_wall_time`：
- ミリ秒フィールドは `× TICK_MS` で元のミリ秒に戻る
- count系は `× TICK_MS` で `元tick数 × 250ms` に戻る
- 2つの変換が**交換可能でない**ことも検証（`assert_ne!`）

負の対照：`legacy_tick_count` を素通しにすると
`legacy budget 1 no longer spans 250ms` で失敗。

41テスト pass（serial／8thread）、wire smoke pass、Java adapter × 2 pass。
