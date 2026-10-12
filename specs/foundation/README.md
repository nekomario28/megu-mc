# 基盤設計 — 共通部品を「消費者が居る順」に作る

**現在の状態と次の判断は [specs/README.md](../README.md)（集約文書）を見る。**
この文書は基盤の設計と、各部品の消費者・実害・順序を所有する。

Status: slice 01 完了、slice 02 完了（3システム移行済み）、03〜06 は設計のみ。

## なぜ今これを書くか

20TPSの時間契約は完了した（[time-contract](../time-contract/README.md)）。
次の段階は遊べる機能を増やすことだが、その前に**基盤を揃える**必要がある。
現状は1ファイル13,900行に、システムごとの構造体・Vec・tick関数・保存節が
並んでいる。機能を足すたびに同じ機構を書き直す形になっている。

[DESIGN.md](../../DESIGN.md) は「初めから世界全体の依存グラフや汎用schedulerを
作ることは前提にしない」と定めている。**この設計はそれに従う。**
各部品は「今日すでに消費者が2つ以上いる」ものだけを作る。
将来のための抽象化は入れない。

## 調査で見つかった、基盤が無いことによる実害

### 1. backoffは仕事を減らすが、巡回とアロケーションを減らしていない

`water_tick` / `lava_tick` / `hopper_tick` は毎tick：

```rust
let cells: Vec<(i32,i32,i32)> = waters().lock().unwrap().iter().copied().collect();
for (x,y,z) in cells {
    if !is_warm(x,z) || !sched_ready(x,y,z) { continue }   // 殆どここで抜ける
    ...
}
```

- **全登録セルをVecにclone**（毎tickのアロケーション、O(全セル)）
- **全セルを巡回**（O(全セル)）
- backoffが減らすのはセル内の仕事だけ

水10,000セルの世界では、実際に処理すべきセルが数個でも毎tick10,000回の
反復とVec確保が走る。time-contract の契約
「scheduled tickの遅延値を、全セルの永久巡回周期に読み替えない」に
**実装が反している**。

### 2. 所属判定が要素ごとにmutexを取る

`is_hot` / `is_warm` は呼ぶたびにmutexを取る。要素ごとに呼ぶループでは
要素数ぶんのlock/unlockになる。炉で測定したところ、400炉で
**1tickあたり約35.6µs**を占め、個別修正で炉stageがx1.60になった。
同じ形のループが20箇所残っている（[slice 18](../time-contract/slices/18-membership-lock-hoist.md)）。

### 3. 同じ役割の経路が複数あり、片方だけ直される

飛翔体の積分が4経路あり、レビューは2経路だけ指摘し、
残る `Snowball` は**dragが皆無**のまま残っていた
（[slice 26](../time-contract/slices/26-contradictions.md)）。

### 4. ブロック/アイテムの知識が巨大なmatch表に散っている

`state_to_item` / `item_to_state` / `base_state` / `block_hardness` /
`hidden_states` / タグ / 発光 / 音 がそれぞれ別の表。
新しいブロック（レッドストーン部品など）を足すたびに複数の表を直す。

## 作る部品と、その消費者

**消費者が今日2つ以上いるものだけ。**

| 部品 | 今日の消費者 | 解く実害 |
|---|---|---|
| **Scheduled-tick queue** | 水・溶岩・ホッパー・作物・オブザーバー・炉・plugin | 実害1 |
| **Region accessor** | 全tickシステム | 実害2 |
| **Block/Item registry（revision付き）** | 状態↔アイテム・硬さ・タグ・遮蔽・protocol id・cache key | 実害4 |
| **Entity framework** | Arrow・Pearl・Snowball・Bobber・Xp・Drop・Primed・Mob | 実害3 |
| **永続化の section 登録** | 既存の全保存対象 | MET4で既に強制されている |
| **Chunk送信 pipeline** | 既存の `stream_chunks` + ビュー距離の設定 | プレイヤー体験（別PR #6） |

## 順序

依存が少なく効果が測れるものから。

1. **[Region accessor](slices/01-region-accessor.md)** — 全システムが使う。
   測定済みの費用があり、効果が数字で出る。
2. **[Scheduled-tick queue](slices/02-scheduled-ticks.md)** — 実害1を消す。
   消費者が既に7つ。time-contract の契約違反もここで解消する。
3. **[Registry](slices/03-registry.md)** — 実害4。レッドストーン部品を
   足す前に必要。revision は既存の `CachedLookup` の依存キーに接続する。
4. **[Entity framework](slices/04-entity-framework.md)** — 実害3。
   家畜・村人を足す前に必要。
5. **永続化 section 登録** — 新しい状態を持つ部品ごとに必要になった時点で。
6. **Chunk pipeline** — PR #6 と統合。

## 共通部品の「最大化」とは何を最大化することか

抽象化の数を増やすことではない。**同じ機構を二度書かないこと**と、
**その機構が測った費用を持つ場所に一度で効くこと**。

- region accessor は20箇所のlock取りを1箇所に集める
- scheduled-tick queue は7システムの巡回・アロケーション・backoffを1つにする
- registry は表の重複を1つにし、キャッシュのrevisionを1つにする
- entity framework は物理・保存・興味管理・protocolを1つにする

## 検証の原則

- 各部品は**既存の挙動を変えずに**導入する。等価性は独立oracle付きの
  回帰テストで示す（`region_membership` の前例に倣う）。
- 費用は **fixture で前後を測る**。ノイズは4%以上あるので
  **x1.10未満の主張はしない**（[slice 18](../time-contract/slices/18-membership-lock-hoist.md)）。
- 新しい消費者を足すときは、その部品の既存テストが通ることを確認する。
- **`main` に直接入れない。** 段階ごとにbranchを切り、CIを通す。

## Pumpkin の扱い

[issue #3](https://github.com/nekomario28/mc-spine/issues/3) の制約に従う。

- Pumpkin は **GPL-3.0**。server コードと asset は**貼らない**。
- 参考にするのは高レベル挙動と設計。独立実装し、出典を issue に残す。
- 参考にした点は slice に「何を参考にし、何を変えたか」を書く。

scheduled-tick queue は Pumpkin の `ChunkTickScheduler`
（遅延インデックスのリングバッファ + `(pos, kind)` の重複排除集合）を
**設計の参考**にする。コードは写さない。
