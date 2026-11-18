# 通常スロットとチェスト転送

Horizon の `crates/horizon-client/src/menu.rs` (`pickup_inner`, `move_to`, `quick_move`) を参考に、既存の処理へ適応した最初の gameplay slice。
donor revision は `0564ab0bcd3235daa6001033261b79aef5f81de1`。MIT notice は
[`third_party/licenses/horizon.txt`](../../../third_party/licenses/horizon.txt) に保持する。
Horizon の crate や26.3データを runtime dependency にはしていない。

## 観測した欠落と変更

- 同一アイテムを `60 + 10` にする左クリックが、`64 / cursor 6` でなく `10 / cursor 60` になった。`red.txt` は修正前の失敗。
- 同一 item / components の満杯スロットは交換せず、入る分だけマージする。
- 右クリックは半分を切り上げて取り、1個ずつ置き、異種スタックは両ボタンで交換する。空きへの配置と交換も stack limit に従う。
- チェストの重複した通常クリック分岐を削除し、既存メニューと同じ `click_slot` を呼ぶ。
- チェストから inventory への Shift 移動は後ろから。inventory からラージチェストへの移動は両半分の既存スタックを先に埋める。
- 本番 wire 検査で、chest item に配置先 state が無く、何も置かなかったことが分かった。1.21.1 の default chest state への対応を追加した。向き・接続状態を決める完全な placement 実装ではない。

## 参照が証明する範囲

`vanilla.json` は固定 server jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) の**実クラス**を実行して採取した結果。
`tools/container-oracle/ContainerOracle.java` は公式 mappings で解決した次の entry point を呼ぶ。

- `crq.b(cuq,int)` = `Slot.safeInsert(ItemStack,int)`
- `cqc.b(cmx,int)` = `ChestMenu.quickMoveStack(Player,int)`。3行／6行の実メニューが作るスロット配置を使う。

82ケースは stone / snowball / diamond sword / max-stack-size component の境界、異種item・componentの拒否、単体・ラージチェストの双方向転送と満杯時の残留を含む。
Java 側でクリックアルゴリズムを再実装した oracle ではない。`safeInsert` は pickup 分岐全体のoracleではないため、分割・交換には回帰テストと bytecode 確認、本番 wire 検査を組み合わせている。

jar は **抽出・Java起動より前**に検証する。biome exporter と認証済みclasspathの所有者を `tools/vanilla_java.py` に一本化した。
署名は削除せず、プローブは独立packageで実行する。この2操作の参照ではサーバー起動・download・Playerの生成は不要。
同じJavaファイルにある `Inventory` 操作だけは、別fixtureでconstructorを迂回した実ServerPlayerを使う。
その前提と限界は[回収の証拠](../inventory-pickup/README.md)で区別する。

## 再現

```sh
# JDKと既存の固定jarが必要。既定は再採取した結果との比較であり、上書きしない。
python3 tools/container_oracle.py
# 意図して参照を再採取するときだけ:
python3 tools/container_oracle.py --output /tmp/container-vectors.json

# 普段のゲートはJDK不要。記録済みの全入力と期待値を読む。
cargo test --release --offline --locked -j 1 container_tests
python3 tests/container_oracle_test.py
python3 tests/container_wire.py target/release/mc-spine
```

`container_oracle_test.py` は欠落した末尾・空入力・余剰行・変更結果・別jarを拒否する。`zip` の短い側に合わせて比較を終えない。
`container_wire.py` はlocalhostの本番バイナリへ protocol 767 の creative-slot / block-place / open / click / close を送り、サーバーの slot reply と再オープンした内容を読む。client予測や内部 map の直接注入には頼らない。
両ゲートは `tests/verify.sh` とCIに接続する。

## 受け入れ境界

最初のslice時点の実行結果とファイルhashは `run.json`、対応する生ログはこのディレクトリに保存する。
Inventory oracle追加後の再実行は[回収の記録](../inventory-pickup/README.md)を参照。
チェストの入力・期待結果は変えず、共有Javaソースのprovenanceだけを更新した。
このsliceの結果は、1.21.1 datapack移行ブランチ全体の受け入れではない。

未検証・未移植: 開閉音、shulkerの中身制約、任意component patchの意味的同値、特殊結果スロット、
waterlogged chest の water tick 再予約、barrel の `open` state。
向き／left-right／異種チェストの接続判定、複数閲覧者の開閉同期、`isChestBlockedAt` 閉塞判定、
毎 tick `broadcastChanges` 同期、`recheckOpeners` 到達距離検証は
[チェストのワールド側契約](#チェストのワールド側契約)で閉塞した。

## チェストのワールド側契約

M2 slice 3 + 3c。二重チェストを「隣接」ではなく block state で判定し、開閉・閲覧者数・
破壊時の window 無効化と drop・永続化、さらに Slice 3c の閉塞判定・複数閲覧者同期・
到達距離再検証を 1.21.1 の機構に合わせた。
bytecode は `server-1.21.1.jar` (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) を
`javap -p -c` および jar probe で読み、mappings (`03f8985492bda0afc0898465341eb0acef35f570`) で解決した。
参照した機構とその根拠:

- `dhb.g(dtc)`（`ChestBlock` の BlockType 写像）は `single→SINGLE`, `right→FIRST`,
  `left→SECOND`。`dif.a` は own が FIRST なら own を先に、SECOND なら隣を先にして
  `dif$c$a(first, second)` を作り、`dhb$1` が `CompoundContainer(first, second)` を組む。
  `bqj.a(int)` は `slot < first.size` なら first。したがって結合 container の
  **slot 0..=26 が right 半分、27..=53 が left 半分**で、クリックした半分に依存しない。
- `ChestBlock.getStateForPlacement` は facing = `getHorizontalDirection().getOpposite()`、
  `type` は `candidatePartnerFacing`（`s.is(this) && TYPE==SINGLE` のときだけ facing を返す）
  から。sneaking + 水平面クリックは `clickedFace.getOpposite()` の block だけを見て、
  facing が直交する場合に接続する。非 sneaking は `getClockWise()` → LEFT、
  `getCounterClockWise()` → RIGHT の順。
- `ChestBlock.updateShape` は隣接半分の facet と `getConnectedDirection` の逆方向一致で
  pair を回復し、接続先が chest でなくなったら `single` に戻す。
- `Containers.dropContentsOnDestroy` は `!state.is(newState.getBlock())` で守られる。
  block identity の変化だけが中身を落とす。
- `ContainerOpenersCounter` は BE ごとの `openCount` を増減し、変化のたびに
  `openerCountChanged(prev, count)` → `dqo.a(dcw,jd,dtc,int,int)` が
  `level.blockEvent(pos, block, 1, count)` を呼ぶ。double chest は結合 container の
  `startOpen` が両方の BE に届くので、両半分がそれぞれ event を出す。
- `ChestBlock.isChestBlockedAt` (`dhb.a(Ldcx;Ljd;)Z`) は `isBlockedChestByBlock(level, pos)`
  (`dhb.a(Ldcc;Ljd;)Z`: `level.getBlockState(pos.above()).isRedstoneConductor(...)`) または
  `isCatSittingOnChest` (`dhb.b(Ldcx;Ljd;)Z`: `AABB(x, y+1, z, x+1, y+2, z+1)` に交差する
  `isInSittingPose()` な `Cat`)。`BlockStateBase.isRedstoneConductor` (`dtb$a.g`) を
  全 26,684 block state で検証し、`blocksMotion && isCollisionShapeFullBlock` に対して
  `mud`/`soul_sand` を `true`、ガラス類・葉・`beacon`/`glowstone`/`sea_lantern`/`redstone_block`/
  `tnt`/`observer`/`piston`/`sticky_piston`/`*copper_grate`/`*copper_bulb` を `false` とする
  `BReg::is_redstone_conductor` に一致させた。`ChestBlock.getMenuProvider` (`dhb.b` → `dif.a`) は
  double chest のどちらか片方が blocked なら `Optional.empty()` を返し、`EnderChestBlock.useWithoutItem`
  (`dip.a`) は `isBlockedChestByBlock` のみ検査し、`BarrelBlock` (`dfe.a`) と minecart chest は閉塞しない。
- `AbstractContainerMenu.broadcastChanges` (`cpu.d`): `ServerPlayer.tick` (`aqv.l`) 内で毎 tick
  viewer ごとの直前送信 slot (`Client.chest_sync`) と現在の container slots (`0..27` / `0..54`) を
  比較し、差分を単調増加 `stateId` 付きの `ClientboundContainerSetSlotPacket` (`acr` = `0x15`,
  `wid = 1`) として配信する。`0x0e` click 側の即時応答 (`send_chest_slot` / `resync_slot`) も
  同じ `chest_sync` の `last_slots` / `state_id` を更新して重複送出と逆転を防ぐ。
- `ChestBlockEntity.recheckOpeners` (`dqo.c`) / `ChestMenu.stillValid` (`cqc.b`) →
  `Container.stillValidBlockEntity` (`bqk.a(Ldqh;Lcmx;F)` `4.0f`) / `CompoundContainer.stillValid` (`bqj.a`) →
  `Player.canInteractWithBlock(pos, 4.0)` (`cmx.a(Ljd;D)Z`): 単位 block AABB から player eye
  (`1.62f` standing / `1.27f` crouching) までの二乗距離 `< (blockInteractionRange + 4.0)^2`
  (survival/adventure `8.5`、creative `9.0`)。double chest は両半分を検査し、ender chest は
  `chest_sync.anchor` を検査し、minecart chest は `ContainerEntity.isChestVehicleStillValid` (`cox.g`) の
  `canInteractWithEntity(boundingBox, 4.0)` (`< 7.0` survival / `< 9.0` creative) を検査する。
  失効時は `ClientboundContainerClosePacket` (`aco` = `0x12`, windowId 1) と `sync_chest_openers` (`0x08`) を送る。
- 1.21.1 play clientbound の `ClientboundBlockEventPacket` は 0x08。
  `agh.a` の登録順（`agg.a`=bundle 0x00 → `agg.c..agg.j`）で `agg.j` が `acc`
  （`ClientboundBlockEventPacket`）であり 9 番目。隣の 0x07 が block entity data、
  0x09 が block update、`agg.t`(0x12) が `ClientboundContainerClosePacket` で、
  既存の packet id と一致する。

観測点は 2 層に分けた。`src/tick_tests.rs` の 9 本 + Slice 3c の 4 本 (`chest_contract_*`) と
`src/main.rs::mineshaft_runtime_tests::chest_contract_live_tcp_blocked_broadcast_and_reach` は
block state（設置・pairing・3連・sneak・updateShape）、slot 順、drop 引き金、window 無効化、
閲覧者数と event、`isRedstoneConductor` / 閉塞判定 / 猫、`broadcastChanges` 毎 tick 差分、
`recheckOpeners` 境界（survival/creative・立位/しゃがみ・double chest 遠側半分・ender chest・cart）を
実 TCP と unit fixture で固定し、`tests/container_wire.py` は実 TCP で 2 client の開閉、
半分ごとの slot 所有、破壊時の `0x12` と drop、再起動後の left/right と中身を読む。
`tools/mutation_check.sh` には `chest_contract_*` 3 本（片側閉塞、27..53 同期、reach 再検証）を接続し、
`MUTATION_STABILITY=1` で 3/3 検出を確認した。

未検証・未移植（Slice 3c 後）: waterlogged chest の water tick、`LootTableSeed` の初回 open までの保持
（block chest 側）、ender chest / minecart chest の蓋・entity event 同期、barrel の `open` state。
続きは[移植順序](../../../specs/horizon-gameplay/README.md)を参照。
