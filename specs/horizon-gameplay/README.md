# Horizon の gameplay 再利用

対象を worldgen に限定せず、チェスト・インベントリ・エンティティにも広げる。
ただし移植の単位は crate ではなく **1.21.1 の観測可能な契約**とする。
Horizon の 26.3 の registry、packet ID、データ、式を丸ごと採用しない。
ライセンスと参照 revision は [再利用調査](../../evidence/foundation/horizon-reuse/README.md)が正本。

## 共通の出口条件

- 既存の所有者を差し替える。同じ規則を旧経路と新経路に二重実装しない。
- 公式 mappings で 1.21.1 のクラス・overload を確認する。
- 実クラス oracle → Rust の同一入力 → 本番経路の順で検査する。
- 入力・結果の欠落、比較ゼロ、別版 jar は失敗。故障注入が指定した assertion を落とすこと。
- gameplay の部分合格を、未完の datapack 移行やサーバー全体の合格に置き換えない。

## 小さく移植する順序

### 1. 通常スロットとチェスト転送

Horizon `horizon-client/src/menu.rs` の `pickup_inner` / `move_to` を参考に、
既存の通常スロット処理に組み込む。別のメニューエンジンは追加しない。

対象は部分マージ、右クリックの分割・1個配置・異種交換、stack limit、
単体／ラージチェストの Shift 移動先順序と両側へのマージ。
実クラス参照は `Slot.safeInsert` と `ChestMenu.quickMoveStack`。
[Pinned fixture と検証範囲](../../evidence/gameplay/containers/README.md)を参照。
チェスト item が既存の default-state 配置経路に届かない問題も、wire 検査の前提として扱う。

### 2. アイテムエンティティとプレイヤーの受け取り

donor は `horizon-world/src/inventory_pickup.rs` / `player_inventory.rs` と
`tools/test_inventory_pickup_parity.py`。
次の観測点は選択 hotbar → offhand → 通常 inventory のマージ優先順位、
空きスロット順序、部分回収、満杯時の entity 残量、pickup delay。
既存の `drop_tick` と擬似 player の `actor_tick` は別の inventory 表現を使うため、
添字を混ぜない。`Inventory.add` の実クラス参照で優先順位・残量・戻り値を固定し、
実プレイヤーの `drop_tick` に接続した。[検証範囲](../../evidence/gameplay/inventory-pickup/README.md)を参照。
部分挿入と成功通知は別の契約なので、戻り値を「入った個数」に置き換えない。

pickup delay は[独立した slice](pickup-delay.md)で閉塞した。実プレイヤーの `drop_tick` と
擬似 player の `actor_tick` が同じ age 述語を問い合わせ、しきい値の境界、満杯の先行プレイヤーが
次の適格者を塞がないこと、部分挿入時に `age` の加齢段が保全されることを検査している。
この段階は部分実装。擬似 player の表現統合、所有者制限（thrower の UUID / `canPickup`）、
`ItemEntity.playerTouch` 自体の実行、接触範囲・scheduler・統計／advancement は未検証。
次は item entity の所有者・接触条件を独立に固定し、その後で actor の表現を統合する。

### 3. チェストのワールド側契約

開閉・複数閲覧者同期・ブロック更新でのメニュー無効化・破壊 drop・永続化を、
既存の container 保存処理と結合する。二重チェストは隣接だけで判定せず、
同じ block kind、facing、left/right の状態を 1.21.1 と照合する。
1 の普通のスロットが通っても、この段階の合格にはしない。

実装済み（1.21.1 の bytecode で確認した機構。詳細と未検証項目は
[検証範囲](../../evidence/gameplay/containers/README.md#チェストのワールド側契約)）:

- 設置は `ChestBlock.getStateForPlacement`。facing は player の yaw の逆、`type` は
  `candidatePartnerFacing`（同じ `Block` かつ `TYPE=single` だけが候補）、
  waterlogged は置き換える fluid が水源かどうか。sneaking の水平面クリックは
  クリックした block だけを候補にし、facing が直交する場合に限り接続する。
- 隣接 block の書き込みで `ChestBlock.updateShape` を回し、survivor は `single` へ戻る。
  pairing の修復は state の書き込みであり、中身には触れない。
- 結合 container の slot 順は `DoubleBlockCombiner` の順（`dhb.g` で right=FIRST、
  `CompoundContainer(first, second)`）で、**slot 0..=26 が right 半分、27..=53 が left 半分**。
  どちらをクリックして開いても同じ。
- 破壊 drop の引き金は block identity の変化（`Containers.dropContentsOnDestroy` の
  `!state.is(newState.getBlock())`）。同一 block の state 変化では中身を捨てない。
- `container_exists` は world の block を見る。container が消えた瞬間に開いている window は
  `0x12` で閉じ、閉じた window への click は container map を作り直さない。
- 閲覧者数は開いている window から導出し（永続化しない）、半分ごとに
  `ClientboundBlockEventPacket`（0x08, action 1, param=count）を送る。

未検証・未移植: 開いている複数閲覧者への毎 tick の `broadcastChanges`
（A が動かした item は B の開いた window に次の tick では出ない）、
`isChestBlockedAt`（上に載った block / 猫で結合を拒む）、waterlogged chest の
water tick 再予約、`LootTable`/`LootTableSeed` の初回 open まで保持、
視界外に出た閲覧者の `recheckOpeners`、ender chest と minecart chest の
viewer 同期（entity event 経路）、barrel の `open` state。

### 4. エンティティの属性・移動・接触

候補は `horizon-world/src/attributes/entity.rs`、`entity_move.rs`、
`entity_collision.rs`、`entity_fluid_tick.rs` とそれぞれの parity harness。
属性 default は 1.21.1 の `DefaultAttributes` から採取し直す。
移動は collision、支持面、着地、fluid、game event の呼出し順が契約であり、
world callback を偽の値で埋めて「移植済み」にしない。
先に 2 entity / 1 接触面の fixture を作り、最後に既存 tick へ接続する。

AI・経路探索・戦利品・乗り物なども再利用対象から除外しないが、
必要な registry / world callback が見える段階で独立した slice に分ける。
ここで候補に挙げたことは、Horizon 全 suite の実行成功や対応完了を意味しない。
