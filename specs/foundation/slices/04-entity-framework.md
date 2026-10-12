# 04 — Entity framework（8種の重複を1つにする）

## Why

エンティティ8種のフィールドを並べると、共通部分と重複が明確になる。

| 種 | フィールド数 | 共通部分 | 固有部分 |
|---|---:|---|---|
| Arrow | 19 | eid owner x y z dx dy dz ttl | dmg fire punch kind loy ret_item ret_comps pierce last_hit stuck |
| Pearl | 9 | eid owner x y z dx dy dz ttl | （なし） |
| Snowball | 10 | eid owner x y z dx dy dz ttl | egg |
| Bobber | 10 | eid owner x y z dx dy dz age | bit |
| Xp | 6 | eid x y z age | val |
| Drop | 8 | eid x y z age | item cnt comps |
| Primed | 5 | eid x y z | left |
| Mob | 10 | eid pos | uuid etype hp last_hit hurt_at fuse love air |

**全8種が「識別子・位置・寿命カウンタ」を持つ。**
飛翔体4種は更に「owner と速度」を共有する。

### 実際に起きた害

- **物理が4経路に複製され、1つだけ直された。**
  レビューは Arrow と Pearl を指摘し、`Snowball` は
  **dragが皆無・重力も0.05**のまま残った
  （[slice 26](../time-contract/slices/26-contradictions.md)）。
- **興味管理が種ごとに別実装。** `interest_tick` に
  「spawn streams via interest_tick」が種の数だけ並ぶ。
- **protocol が種ごとに別関数。** `spawn_arrow_pkt` / `spawn_typed_pkt` /
  `drop_meta_pkt` / `xp_orb_pkt` / `spawn_entity_pkt`。
- **保存が種ごとに別節。** 各々が独自のバイト列を書く。
- **Mob だけ別系統。** `Mob` は `eid`＋`pos`（Mutex）を持ち、
  他と共通の位置・興味・保存の形を使わない。家畜を足すには
  もう一度この別系統を複製することになる。

## 契約

**共通部分を1つにし、種ごとの差を「物理」と「データ」に分ける。**

```rust
/// 全エンティティが持つ共通部分
struct Entity {
    eid: i32, uuid: [u8; 16], kind: EntityKind,
    pos: Vec3, vel: Vec3,
    lifetime: i32,        // age / ttl / left / fuse を統一
    owner: Option<i32>,
}
/// 種ごとの固有データ
enum EntityData { Arrow { .. }, Pearl, Snowball { egg: bool }, Bobber { bit: bool },
                  Xp { val: i16 }, Drop { item, cnt, comps }, Primed, Mob { .. } }
```

- **物理は1箇所**。drag と重力をパラメータにし、`ThrowableProjectile` 系は
  同じ積分器を通る。種ごとに物理を書けない形にする。
- **興味・保存・protocol も1経路**。種は `EntityKind` のデータとして扱う。
  新しいMobは「データを1つ足す」であって「構造体とtickと保存を複製する」ではない。
- `lifetime` の単位は **server tick**。値は `server_ticks()` から導出し、
  生のtick数を書かない（time-contract で4回踏んだ誤り）。
- 位置は `Mutex<Vec3>` に統一するか、所有権を1スレッドに閉じるかは
  **slice 01/02 のtick文脈で確定する**（下記「未確定」）。

## 非目標

- ECS の導入。コンポーネントシステムを作ること自体を目的にしない。
- 全エンティティ種の実装。骨組みだけ作り、種は必要になった順に足す。
- AI の一般化。Mob のAIは当面 `mob_tick` のままでよい。
- 既存8種の挙動を変えること。等価性が先、整理は後。

## 未確定（slice 01/02 が先に決める）

- tick 関数が受け取る文脈（region snapshot、tick queue ハンドル）の形。
  entity tick のシグネチャはこれに従う。**今は決めない。**
- 位置のロック戦略（per-entity Mutex か、単一スレッドでの所有か）。
  現状は per-entity Mutex で、tick queue が単一スレッドである以上
  不要になる可能性がある。**測定してから決める。**

## ゲート

- **移行の等価性**: 8種それぞれについて、移行前後のtickが
  **同じ軌道・同じ寿命・同じ保存バイト列**を生むことを検証する。
  特に4飛翔体は**同じパラメータで同じ結果**になること
  （Snowball と Pearl が一致すること）。
- **物理の単一性**: 種ごとの積分器が1つであることを検索で確認する。
  同型の重複が0件であること（この誤りを2回踏んだ）。
- 保存の round-trip（既存の `persistence_tests` を拡張）。
- 全テスト、CI 4step、wire smoke。

## リスク

- 大規模な書き換えになる。**種ごとに1つずつ移し、その都度テストを回す。**
  一括置換は slice 18 で踏んだ（デッドロックを作った）。
- 等価性の検証が薄いまま進むと、見えない挙動差が残る。
  種ごとの決定論的シナリオ（軌道・衝突・期限）を先に用意する。
