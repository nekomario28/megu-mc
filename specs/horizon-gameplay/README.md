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
続いて [ItemEntity playerTouch / target / spectator (Slice 2b)](item-touch.md) で、
`ItemEntity` の `target` / `thrower` UUID（`playerTouch` と `tryToMerge`）、1.21.1 `Player.aiStep` の
接触 AABB 交差、`isSpectator()` / `getHealth() > 0` 除外、`DRO1` 永続化を共通所有者
(`Drop::can_be_picked_up_by`) に閉塞した。
この段階は部分実装。擬似 player の表現統合、scheduler、統計／advancement（`THROWN_ITEM_PICKED_UP_BY_ENTITY` 等）は未検証。
次は actor の表現統合または後続スライスを独立に固定する。

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
- 閉塞判定は `ChestBlock.isChestBlockedAt`（`dhb.a` = `isBlockedChestByBlock` の
  `BlockStateBase.isRedstoneConductor` + `isCatSittingOnChest` の sitting `Cat` AABB 交差）
  および `ChestBlock.getMenuProvider`（`dhb.b` → `DoubleBlockCombiner` 経由で単体は自身、
  二重チェストは**どちらか一方**が閉塞していれば結合を拒否）に合わせる。`EnderChestBlock.useWithoutItem`
  （`dip.a`）は上空セルの `isBlockedChestByBlock` で拒否し、`BarrelBlock`（`dfe.a`）と
  minecart chest は上空ブロック閉塞を行わない。
- 複数閲覧者同期は `AbstractContainerMenu.broadcastChanges`（`cpu.d`）相当として
  viewer ごとの送信済み slot snapshot を保持し、毎 tick `chest_viewer_tick` が開いている
  コンテナ slot (`0..27` / `0..54`) を diff して変化 slot に単調増加の `stateId` 付き
  `0x15` (`ClientboundContainerSetSlotPacket`) を配信する。
- 到達距離の再検証は `ChestBlockEntity.recheckOpeners` / `Container.stillValidBlockEntity`
  （`bqk.a` → `Player.canInteractWithBlock(pos, 4.0)`: 単体／二重チェスト両半分の単位 AABB から
  eye position `1.62f` / crouching `1.27f` まで `< 8.5` survival / `< 9.0` creative、
  ender chest は操作した block anchor、minecart chest は `ContainerEntity.isChestVehicleStillValid`
  の `< 7.0` / `< 9.0`）を毎 tick 検査し、無効化または射程外となった viewer に `0x12`
  (`ClientboundContainerClosePacket`) を送って `sync_chest_openers` で `0x08` count を減算する。

未検証・未移植: waterlogged chest の water tick 再予約、`LootTable`/`LootTableSeed` の
初回 open まで保持（block chest 側）、ender chest の block event 経由の蓋アニメーション、
minecart chest の entity event 経路、barrel の `open` state。

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

実装済み（Slice 4a）: [Entity Collision & Step-Up (Slice 4a)](entity-collision.md) および
[検証記録](../../evidence/gameplay/entity-collision/README.md) で、固定 1.21.1 jar の
`Entity.collide` / `collideBoundingBox` / `collideWithShapes`（Y 先行、`|x| < |z|` で Z 先行、
同値で X 先行）/ `collectCandidateStepUpHeights` / `BlockCollisions`（`floor(min - 1e-7) - 1`
の 1.5 高 fence 窓）/ `Shapes.collide` (`1e-7`) / `Entity.move` 衝突フラグを共通所有者
[`src/entity_collision.rs`](../../src/entity_collision.rs) に閉塞し、`CartRuntimeWorld::move_cart`
と `mob_tick` の水平移動（`Attributes.STEP_HEIGHT = 0.6` と `EntityType` 寸法）へ接続した。
`LivingEntity.travel` の重力・流体・完全な落下統合および entity-entity 押し戻しは後続 slice へ分離。

実装済み（Slice 4b）: [Entity Fluid Height, Eye Immersion, Fluid Push & Lava Contact (Slice 4b)](entity-fluid.md) および
[検証記録](../../evidence/gameplay/entity-fluid/README.md) で、固定 1.21.1 jar の
`FlowingFluid.getOwnHeight` / `hasSameAbove` / `getFlow`、`Entity.updateFluidHeightAndDoFluidPushing`（`deflate(0.001)`、Player/Mob の正規化差と `0.014`/`0.007`/`0.0045000000000000005`）、`Entity.updateFluidOnEyes`（1.21.1 `bsr.H()` は `getEyeY()` 直接参照かつ `bubble_column` 除外）、`Entity.isInLava`（`bsr.bt()` の `fluidHeight[LAVA] > 0.0`）を共通所有者
[`src/entity_fluid.rs`](../../src/entity_fluid.rs) に閉塞し、`Client` 溺水／溶岩・`cart_in_water`・`mob_snap_y`（水中落下ダメージ無効化）・`mob_tick`（水流押し流し・溺水・溶岩接触ダメージ・水による不死系燃焼解除）へ接続した。`src/entity_collision.rs::mob_dimensions` の `cave_spider` ID（`13 -> 16`）も合わせて是正した。
`LivingEntity.travel` の完全な浮力・水泳速度統合およびネザー `lava` の `0.0023333333333333335` 次元分岐は後続 slice へ分離。
