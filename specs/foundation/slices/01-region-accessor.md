# 01 — Region accessor（所属判定を1箇所に集める）

Status: 実装済み。測定は [region-accessor](../../../evidence/time-contract/region-accessor/README.md)。
残り18箇所のper-item呼び出しは同じ形で順次移す。

## Why

`is_hot(x,z)` / `is_warm(x,z)` は呼ぶたびにmutexを取る。
要素ごとに呼ぶループが20箇所あり、要素数ぶんのlock/unlockが走る。

炉で測った実害：休止炉400個で **1tickあたり約35.6µs** が所属判定。
中身は `div_euclid(16)` とHashSet照合だけなので、費用の正体はlock。
炉だけ集約して **炉stageがx1.60**（[slice 18](../../time-contract/slices/18-membership-lock-hoist.md)）。

他19箇所は同じ形のまま。個別に直すと：
- 各ループにクロージャが増えて読みにくくなる
- 「今tickの集合」という前提が各所に散る
- 要素が0のときの早期returnを各所で書き忘れる（実際に1回踏んだ）

## 契約

**1tickのスナップショットを1つだけ作り、全システムが同じものを読む。**

```rust
struct RegionSnapshot { hot: HashSet<(i32,i32)>, warm: HashSet<(i32,i32)> }
impl RegionSnapshot {
    fn is_hot(&self, x: i32, z: i32) -> bool
    fn is_warm(&self, x: i32, z: i32) -> bool
}
```

- `region_snapshot()` が1tickに1回だけ構築する（`refresh_hot` の直後）。
- スナップショットは `&RegionSnapshot` として tick 関数に渡す。
  グローバルlockを読む経路を消す。
- 集合の中身・`div_euclid(16)` の意味は**変えない**。
- 空なら構築を省く（早期returnを1箇所に集約）。

## 非目標

- 集合の作り方を変える（それは slice 04 region-expansion で済み）
- 空間索引の導入。いまは集合の内容が同じであることが最優先。
- 描画距離に応じた動的な半径（それは chunk pipeline の slice）

## ゲート

- `region_membership_matches_hot_plus_skirt_across_input_changes` を維持。
  独立oracle（元のhot＋skirt）との一致を全ケースで継続。
- 新規: スナップショット経由の判定が、グローバルlock経由の判定と
  **全登録セルで一致**することを検証する。
- 費用: fixture の前後比較。炉stageに加え、水・ホッパー・作物を含む
  fixture（[entity-heavy](../../time-contract/slices/19-entity-fixture.md) を拡張）で測る。
  **x1.10未満は主張しない。**
- 全テスト、CI 4step、wire smoke。

## リスク

- tick 関数のシグネチャ変更が広い。1システムずつ移し、
  移すたびにテストを回す。一括置換しない（slice 18 で一括置換を踏んだ）。
- スナップショットの取り忘れ（1tick古い集合を使う）。
  `game_tick` が構築し、渡さないとコンパイルが通らない形にする。
