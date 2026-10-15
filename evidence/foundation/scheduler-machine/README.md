# スケジューラの backoff と `sched_ready` の関係（項目 g / T）

`flowgx` は状態機械の構造欠陥（終端に到達できない状態、出る辺の無い状態）を検査する。
**「バックオフ中のセルが受理状態 `removed` に到達できるか」**、つまり
**「期限が来ないまま訪問も削除もされない soft-lock は無いか」**を機械的に答えるために使った。

```
5 states, 8 transitions
accepting: removed
cycle: queued → visiting → queued
inspect: 1 problem (0 errors)
```

**到達可能性が言えるのはここまでである。** 「全状態が `removed` へ到達可能」は
**`sched_ready` が `false` を返さないことを意味しない。** 到達可能性は「いつかラダーが
期限に達する」ことしか言わず、キューが**期限より早く**セルを運んでくるかを見ていない。

## 実際の不変条件（実装から）

| 事実 | 出典 |
|---|---|
| `backoff_ticks(fails) = server_ticks(10 << (fails-1))` は 20TPS で `10,20,40,80,160,320,…` と**上限なく**倍化する | `main.rs:1346` |
| `TickQueue` の待ちは `MAX_DELAY`（4秒 = 80 tick）で**クランプ**される | `tick_queue.rs:23`、`bucket_of` |
| `requeue_fluid` は同じ backoff 表から遅延を取り、`schedule` がその待ちを `MAX_DELAY` に丸める | `main.rs:3535` |
| `sched_ready` は「エントリ無し」または「期限到来」で true | `main.rs:3518` |

`fails <= 4` では期限が 10/20/40/80 tick でキュー上限以内に収まるため、キューが来た時点で
期限は到来しており、`sched_ready` は true になる。しかし **`fails >= 5` では期限が 160 tick
以上**になる一方、キューは 80 tick ごとにセルを返す。このとき `sched_ready` は **false** を返し、
`!sched_ready` の分岐が **実際に発火する**。ガードが無ければ 160 tick 待つべきセルを 80 tick で
評価してしまい、変更の無い作業を早く・多く実行する。

したがって **`!sched_ready` ガードは冗長な安全網ではなく、4秒上限キューと無上限ラダーの
差を埋める load-bearing な条件である。削除しない。**

`backoff_ticks` のコメントの「up to 80 game ticks (4 s)」はラダーの上限ではなく
**キューの上限**を指しており、文言が誤解を招く。ラダー自体に上限は無い。

## 実装経路での対照

`tick_tests::a_backoff_past_the_queue_cap_waits_for_its_real_deadline` は
到達可能性モデルではなく**実際の `game_tick`/`water_tick`** を回す。
四方と下を石で囲った水セル（作業が無い）を置き、backoff 表の fail カウンタが増えた tick を
記録して、訪問間隔がラダー `10,20,40,80,160,320,…` に一致することを要求する。
`sched_ready` を常に true にする変異（ガード除去相当）では間隔が `80,80,80,…` になり、
テストが赤くなる（`tools/mutation_check.sh` に登録済み）。
キュー上限よりラダーが大きいことを assert し、対照が空振りしないようにしている。

## 副産物：状態機械が文書になった

`machine.py` は**スケジューラの到達可能性の仕様**である。状態や遷移を変える変更は
**このファイルを赤くする**（あるいは更新を強制する）。ただし前述のとおり、
これは liveness（soft-lock の不在）の検査であり、**`sched_ready` の冗長性の証明ではない。**
