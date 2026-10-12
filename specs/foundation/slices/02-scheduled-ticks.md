# 02 — Scheduled-tick queue（巡回とアロケーションを消す）

## Why

`water_tick` / `lava_tick` / `hopper_tick` は毎tick：

1. 全登録セルを **Vecにclone**（O(全セル) のアロケーション）
2. **全セルを巡回**
3. `sched_ready` で殆どをskip、`sched_result` でbackoffを更新

backoffが減らすのは**セル内の仕事だけ**。巡回とVec確保は減らない。
水10,000セルの世界では、実際に処理すべきセルが数個でも
毎tick10,000回の反復とVec確保が走る。

これは time-contract の契約
「scheduled tickの遅延値を、全セルの永久巡回周期に読み替えない」に反する。
**契約は「セルが時刻Tに自分を再予約する」と言っており、
実装は「毎tick全セルを訪ねる」になっている。**

## 参考にする設計（Pumpkin, GPL-3.0）

issue #3 の制約に従い**コードは写さない**。設計のみ参考にする。

Pumpkin `ChunkTickScheduler`：
- `MAX_TICK_DELAY` 長の**リングバッファ**（遅延で索引）
- `HashSet<(BlockPos, kind)>` で**重複排除**
- `step_tick` はオフセットを1つ進めて該当バケットを `take` するだけ
- chunk単位で lazily に確保（空chunkはゼロ費用）

## 契約

**1tickの費用を「そのtickに期限が来たセル数」に比例させる。**

```rust
struct TickQueue { ring: [Vec<Entry>; MAX_DELAY], queued: HashSet<Key> }
const MAX_DELAY: usize = 80;   // 4s。TICK_MS から導出する
```

- `schedule(key, delay_ticks)` — 重複は無視（同じkeyは同時に1つだけ）
- `due()` — 今tickぶんを取り出す。O(due)
- 遅延は **`server_ticks()` で導出**する。周期変更に追従する。
- 空のchunk/集合は確保しない。
- 既存の `BACKOFF` / `plugin_backoff` はこの上に載せ替え、
  **`sched_ready` をループ内で呼ぶ形を消す**。

## 対象（今日の消費者）

| 消費者 | 現在 | 移行後 |
|---|---|---|
| 水 | 全セル巡回＋backoff | dueのみ |
| 溶岩 | 6tickごとに全セル巡回 | dueのみ |
| ホッパー | 全セル巡回＋backoff | dueのみ |
| 作物 | 全作物巡回＋確率判定 | dueのみ（growthは独自確率のまま） |
| オブザーバー | `obs_unpower` のdrain（既に部分queue） | 統一 |
| 炉 | warm全炉巡回 | dueのみ |
| plugin cell | 全cell巡回＋`plugin_backoff` | dueのみ |

## 非目標

- 汎用のタスクscheduler。ブロック位置の遅延実行だけを扱う。
- 並列実行。tick順序が契約なので単一スレッドのまま。
- vanillaの優先順位付きtick完全再現。まず「巡回を消す」ことだけ。
  vanilla順序の一致は別sliceで、一次参照と比較してから。

## ゲート

- **等価性**: 同じ入力系列で、旧実装と新実装が**同じtickで同じセルを処理する**
  ことを検証する。決定論的な水和・溶岩流・ホッパー搬送のシナリオで
  セル単位の処理tick列を比較する。
- **重複排除**: 同じセルを2回scheduleしても1回だけ処理される。
- **境界**: delay 0 / MAX_DELAY / その先（clamp）を検証。
- **費用**: 10,000セルを登録し、そのうち数個だけdueな状態での
  1tick費用を前後で測る。**巡回が消えたことを数字で示す。**
  x1.10未満は主張しない。
- **契約の回復**: `sched_ready` をループ内で呼ぶ箇所が0件であることを
  検索で確認する。
- 全テスト、CI 4step、wire smoke。

## リスク

- 移行漏れ（あるシステムが古い巡回のまま残る）。検索で0件を確認する。
- 遅延の単位を取り違える。`TICK_MS` と `server_ticks()` から導出し、
  **生のtick数を書かない**（time-contract で4回踏んだ誤り）。
- 重複排除のキーに種類を含め忘れる。同じ位置で水と溶岩が
  別々に予約できる必要がある。
