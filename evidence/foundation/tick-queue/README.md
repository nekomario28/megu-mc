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

## 移行の等価性ゲート（用意した）

`water_spread_trace_is_stable_across_the_scheduler_migration` が、
現実装の水の広がりを**セル単位・tick単位**で固定する。

- 空中 y=150 に水源を置き、40tick の水セル集合を毎tick記録する
- assert: 各tickのセル数と最大レベル、最終的なセル集合

`per_tick = (2,8),(3,8),...,(41,8)`、最終集合は y=110..149 がレベル8、
水源 y=150 がレベル0。

**地形非依存にする必要があった。** 最初は y=70 に置いたが、並行実行で
水が8セルで止まった。**別のテストがworldgenの有効状態を変えるため**
地形の高さが変わり、8ブロックで床に当たっていた。
y=150 に上げて40tickでは地面に届かないようにした。

これは**移行の等価性ゲートであってvanilla parityの主張ではない。**
移行前の挙動を固定するもの。

## 見つけた共有fixtureの競合（修正した）

`src/landing.rs` の `stale_landing_does_not_replace_new_block` が
**WORLD_TEST_LOCKを取らずに `set_block` を呼んでいた**。
新しい水テストのassertが露見させた。ロックを取り、セルを元に戻すようにした。

並列実行で60 passed が5回連続で安定することを確認。

## 水の移行（完了）

`water_tick` を `due()` 主導にした。`waters()` の全件cloneと全件巡回は消えた。

- 登録2箇所（`set_block_if` と chunk再構築）で `schedule(pos, 1)`
- ループは `due()`。**5つの出口すべてで `requeue_water`** を呼ぶ
  （cold/not-ready、2つのsched_result(true)、lvl>=7、末尾）
- 登録から外す場合だけ re-queue しない（`due()` で既にキューから出ている）
- 遅延値は**backoff map が権威**のまま。queue は「今dueのセル」だけを答える。
  ラダー（`backoff_ticks`）の意味は変えていない

**等価性はトレースゲートで確認**：移行後も
`water_spread_trace_is_stable_across_the_scheduler_migration` が
移行前と同一のトレースを返す。

### 費用測定について

このfixture（`entity_heavy` 等）は**水を含まない**ので、水の移行は
fixtureでは測れない。したがって性能値は主張しない。
移行の証拠は**トレースの一致**であって速度ではない。
水セルを大量に含むfixtureを作るまで、費用は不明のまま。

## 残り：ホッパーと溶岩

| システム | 呼び出し | 状態 |
|---|---|---|
| 水 | 毎tick | **移行完了** |
| ホッパー | 毎tick（`step("hoppers")`） | **移行完了**。痕跡ゲートで等価確認 |
| 溶岩 | `TICK_IDX % server_ticks(30) == 0` | **移行完了**。痕跡ゲートで等価確認 |

### 溶岩の落とし穴

`TickQueue` の遅延は**tick単位**で、`due()` を1回呼ぶごとに1tick進む。
しかし `lava_tick` は30tickに1回しか呼ばれない。
そのままqueueを使うと、`due()` が30tickにつき1回しか進まず、
遅延の意味が30倍ずれる。

対処の選択肢：
1. 溶岩用のqueueは `due()` を**毎tick**呼び、dueしたセルを貯めておいて
   `lava_tick` のときに処理する
2. 遅延を**呼び出し回数単位**にし、`ceil((due - now) / period)` を渡す
3. 溶岩だけ移行せず、巡回のまま残す

**どれを選ぶかは測ってから決める。** 現状、水以外のfixtureが無く
費用が測れないため、溶岩の移行は急がない。

## ホッパーの移行（完了）

`water_tick` と同じ手順。登録2箇所で `schedule(pos,1)`、
出口4箇所（cold、powered、容器なし、末尾）で `requeue_hopper`。
**内側の `for (deid,item,dcp)` ループの `continue` は触っていない**
（外側の出口と混同すると、吸収できなかったdropを再scheduleしてしまう）。

痕跡ゲート `hopper_transfer_trace_is_stable_across_the_scheduler_migration`：
ホッパーに3個入れ、下のチェストへ1個ずつ移る様子を毎tick記録。
`(2,1),(1,2),(0,3),(0,3)...`。移行前後で一致。

費用は測っていない（fixtureにホッパーが無い）。証拠はトレースの一致。

## 溶岩の移行（完了）

**単位の落とし穴を解決した。** 訪問は `server_ticks(30)` の倍数でしか起きないので、
「tick T で due」と「T 以降の最初の訪問」は同じ意味になる。
したがって `ceil((due - now) / period)` が**近似ではなく厳密**。

```
delay = (*due - now + period - 1) / period     // period = server_ticks(FLUID_TICK_GT)
```

出口4箇所（cold、固化/流下、lvl>=3、末尾）で `requeue_lava`。

痕跡ゲート `lava_spread_trace_is_stable_across_the_scheduler_migration`：
30tickごとに落下溶岩(104)が1つ増え、最後に水源(96)が残る。
変化点だけ記録し、**形と時刻の両方**をassert（近似ではなく厳密であることの証拠）。

## 全件巡回は全て消えた

`waters()` / `lavas()` / `hoppers()` の
`.lock().unwrap().iter().copied().collect()` は**0件**になった。

### 残る `sched_ready`

3ループ内に `sched_ready` が残っている。queueのdue集合が既に
「ready」を意味するので**冗長**だが、
- 費用は**dueなセル数に比例**（全セルではない）ので目的は達成している
- 安全網として残した
将来の掃除候補として記録する。

## まだやっていないこと（重要）

**システムへの接続は未実施。** `water_tick`/`lava_tick`/`hopper_tick` は
まだ全セル巡回のまま。移行には各システムの登録箇所
（`waters().insert(...)` 等）すべてで `schedule` を呼ぶ必要があり、
**1つ漏らすとそのセルが黙って更新されなくなる**（起床条件漏れ）。

移行のゲートは「旧実装と同じtickで同じセルを処理する」セル単位の比較で、
水和・溶岩流・ホッパー搬送の決定論的シナリオを先に用意する。
**部品ができたことと、実害1が消えたことは別。**

59テスト（serial／8thread）、wire smoke、Java adapter×2 pass。
