# TickQueue（slice 02 の部品本体）

## 何を作ったか

遅延で索引するリングバッファと重複排除の組。`src/tick_queue.rs`。

- `schedule(pos, delay)` — 既に待っていれば**追加しない**（近隣更新の連打で
  締切が先送りされ、いつまでも走らないセルを作らないため）
- `reschedule(pos, delay)` — 走った後で意図的に間隔を伸ばす場合に移動する
- `cancel(pos)`
- `due()` — 今tickぶんだけ返す。空のバケットは空Vecのswap 1回

`MAX_DELAY` は **`MAX_DELAY_TICKS = 4 * TICKS_PER_SECOND`**。
4秒という意味を周期から導出する（生のtick数を書かない）。

## なぜこれが実害1を解消するか

現状の `water_tick`/`lava_tick`/`hopper_tick` は毎tick：

1. 全登録セルをVecにclone
2. 全セルを巡回
3. `sched_ready` で殆どをskip

backoffが減らすのはセル内の仕事だけで、**巡回とVec確保は減らない**。
遅延で索引すれば、1tickの費用は**dueなセル数**に比例する。

## 設計の出典

Pumpkin `ChunkTickScheduler`（遅延インデックスのリングバッファ +
`(pos,kind)` の重複排除集合）を**設計の参考**にした。
Pumpkinは **GPL-3.0** なのでコードは写していない
（issue #3 の provenance 方針）。独立実装。

## ゲート（7テスト）

| テスト | 何を固定するか |
|---|---|
| `a_cell_becomes_due_after_exactly_its_delay` | delay d は **d回目のdue** で発火。以後発火しない |
| `a_repeated_schedule_keeps_the_earlier_deadline` | 重複scheduleは**拒否**し、締切を先送りしない |
| `rescheduling_moves_the_deadline` | 旧締切は発火せず、新締切で1回だけ発火 |
| `cancelling_removes_it_from_its_bucket` | 取り消したセルは発火しない |
| `delays_are_clamped_rather_than_dropped` | 0も過大も**捨てずに**clamp。捨てれば黙って止まるセルになる |
| `the_ring_wraps_without_losing_entries` | MAX_DELAY件が全て1回ずつ発火 |
| `an_empty_queue_costs_nothing_to_poll` | 空でも各dueが空を返す |

### 負の対照

`bucket_of` から `- 1` を外すと**5テストが失敗**する
（[negative-control.log](negative-control.log)）。実装直後にも
この off-by-one で4テストが赤くなり、そこから直した。

## まだやっていないこと（重要）

**システムへの接続は未実施。** `water_tick`/`lava_tick`/`hopper_tick` は
まだ全セル巡回のまま。移行には各システムの登録箇所
（`waters().insert(...)` 等）すべてで `schedule` を呼ぶ必要があり、
**1つ漏らすとそのセルが黙って更新されなくなる**（起床条件漏れ）。

移行のゲートは「旧実装と同じtickで同じセルを処理する」セル単位の比較で、
水和・溶岩流・ホッパー搬送の決定論的シナリオを先に用意する。
**部品ができたことと、実害1が消えたことは別。**

59テスト（serial／8thread）、wire smoke、Java adapter×2 pass。
