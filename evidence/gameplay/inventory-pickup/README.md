# Inventory へのアイテム回収（部分実装）

Horizon `inventory_pickup.rs` / `player_inventory.rs` とその parity harness を参考に、
**既存の実プレイヤー `drop_tick`** の挿入規則を置換した。
donor revision・MIT notice は[再利用調査](../../foundation/horizon-reuse/README.md)が正本。
別のentity/menu engineや26.3のruntime依存は追加しない。

## 再現した欠落

修正前の本番tickでは、10個を近くのプレイヤーへ回収すると、1つの部分スタックへ4個だけ入れ、
残り6個をentityに残した（`red.txt`）。挿入先はHashMapの反復順に依存し、
空きの検索もhotbarよりmain inventoryを先にしていた。

新しい規則は選択hotbar → 非空のoffhand → hotbar/mainの順でマージし、
その後hotbar/mainの空きを検索する。残量が減る限り繰り返す。
装備スロットへは入れず、空のoffhandも挿入先にしない。
component不一致とstack limitを既存の `insert_into_slot` で扱い、
損傷したdamageable itemはnativeのcopy-and-clear分岐に従う。

**部分挿入したことと、`Inventory.add` の戻り値は別である。**
この1.21.1実装は最後の反復の前後を比較するため、少し入った後で空きが尽きると
`false` を返す。creativeの満杯時は残りを消費して `true` になる。
戻り値・最終inventory・source残量を別々に比較する。

## 実クラス参照とその前提

`vanilla.json` は固定1.21.1 server jarの実 `Inventory` / `ItemStack` を実行した49ケース。
公式mappingsとbytecodeで、次のoverloadとslot配置を確認した。

- `cmw.f(cuq)` / `cmw.c(int,cuq)` = `Inventory.add`
- `cmw.e(cuq)` = `getSlotWithRemainingSpace`、`cmw.h()` = `getFreeSlot`
- native indexはhotbar 0–8、main 9–35、armor 36–39、offhand 40。
  本番のmenu-map（hotbar 36–44、main 9–35、offhand 45）へ変換して比較する。

`Inventory` と `ItemStack` は実constructorを呼ぶ。一方、失敗／creative分岐が読むPlayerは
`Unsafe.allocateInstance` で**実ServerPlayerのconstructorを迂回**し、
実constructorで作ったAbilitiesだけを注入している。
このjarの `Player.hasInfiniteMaterials` (`cmx.fL`) は
`abilities.instabuild` (`cA.d`) を読むだけであり、native実装をそのまま呼ぶ。
ゲームメソッドのoverrideや偽のLevelは無いが、**通常のPlayer生成を通した検証ではない**。
JDKはこの反射操作に警告を出す。将来JDKで禁止された場合はoracleが失敗し、skipしない。
jarはJava起動・抽出より前に認証し、署名を削除しない。

参照は通常／16個stack／非stackable／上書きlimit、空・満杯・部分挿入、
選択/offhand/通常スロットの競合、armorの除外、損傷・unbreakableを含む。
任意のcomponent patchやitem prototype全体の一致は意味しない。
共有Javaソースを拡張したため、従来のチェスト82ケースも再実行した。
入力と期待結果が構造的に不変であることを確認し、source hashのみ更新した。

## 本番経路と通知

`drop_tick` は候補を探した後で**live entityとinventoryを同時に更新**する。
最初の近接プレイヤーが満杯でも次を試し、消費済みsnapshotから二重挿入しない。
ロック順序は既存actorと同じinventory → dropsとし、socket送信はロックを離して行う。

本番tickの3回帰テストは、複数スロットへの分配、部分回収後のentity残数とTCP通知、
満杯の先行プレイヤーと二重回収の防止を検査する。
部分回収ではslot/entity更新を確認し、成功take packetが無いことも確認する。
これは実 `ItemEntity.playerTouch` を実行したoracleではない。
通知条件と元のcountを使う点は `cjh.b_(cmx)` のbytecodeに基づく。
既存の本番プロセス用container/gameplay wire検査も再実行するが、
それを新しい回収シナリオのvanilla-wire比較と呼ばない。

## 再現とゲート

```sh
# JDKと既存の固定jarが必要。既定動作は完全な再実行／比較。
python3 tools/inventory_oracle.py
python3 tools/container_oracle.py
# 記録は明示した場合だけ。通常の比較でfixtureを更新しない。
python3 tools/inventory_oracle.py --output /tmp/inventory-vectors.json

# 日常のCIはJDK無しでも記録済み全ケースを比較する。
cargo test --release --offline --locked -j 1 pickup_
python3 tests/container_oracle_test.py
# 部分実行も正本のscriptを使う。filterに一致するgateが無ければ失敗する。
MUTATION_FILTER=pickup_ MUTATION_RELEASE=1 CARGO_BUILD_JOBS=1 bash tools/mutation_check.sh --offline
```

既存の `tests/verify.sh` / CIに接続済みのRust suiteとPython provenance検査が両fixtureを読む。
空・欠落末尾・余剰行・入力改変・結果改変・stale oracle・別jarを拒否し、
別jarで失敗したCLIは既存出力を保存する。
`tools/mutation_check.sh` は反復打ち切り、selected引数の固定化、通知条件の混同、
最初のプレイヤーで候補探索を止める故障、creative引数の固定化を登録する。
一度scratchへscriptを複製してcwdが `/` になり、baselineが実行されなかった。
その結果はproofに数えず、正本script内に選択実行を追加した。
選択ゼロの失敗と正の選択はgate self-testが検査する。
実行した選択mutationとrestored controlの結果は `mutations.txt` / `run.json` が正本。

## 受け入れ境界と次の段階

このsliceは**部分実装で止める**。source・実行結果・hashは `run.json` と生ログを参照。
全suiteは既知のdatapack移行失敗を残すため、全体の受け入れでもcanonical統合でもない。
終了後に回収されたtransient unitの `systemctl show` は既定のsuccessを返し得るため、
それを完了証拠にしない。`run.json` のunit情報は同じInvocation IDのjournal終了markerと
accountingに結び付け、`unit.jsonl` に保持する。

未実装／未検証：pickup delay、所有者制限、接触bounding box、scheduler、統計・advancement、
stack animation/pop time、全prototypeのdamage/component意味論、menu開放中のstate-id同期。
擬似playerのactorはheldと0–35 inventoryが別表現なので、今回のmenu-map規則を流用していない。
actor側の旧規則は残っている。先に時間・所有者・接触条件を実クラスで固定し、
その後でactorの表現と責務を統合する。

## 後続: pickup delay

上の本文・`run.json`・`mutations.txt`・未実装／未検証の一覧は、**このsliceを測った時点**の
記録であり、書き換えない。当時 pickup delay は未検証で、actor 側にだけ規則があった。

その後 [pickup delay slice](../../../specs/horizon-gameplay/pickup-delay.md) が
`Drop::pickup_delay_elapsed` を1箇所に置き、実プレイヤーの `drop_tick` と擬似 player の
`actor_tick` の両方から同じ規則を問い合わせるようにした。対象は live entity に対する
しきい値の境界（spawn tick と しきい値-1 の拒否）、満杯の先行プレイヤーが次の適格者を
塞がないこと、部分挿入時に `age` の加齢段が保全されることまで。
所有者制限（thrower の UUID / `canPickup`）、`ItemEntity.playerTouch` 自体の実行、
接触bounding box、scheduler、統計／advancement は引き続き未実装で、このsliceの
「部分実装で止める」という結論も変えない。

## 後続: ItemEntity playerTouch / target UUID / spectator (Slice 2b)

その後 [item-touch slice](../../../specs/horizon-gameplay/item-touch.md) が、固定 1.21.1 jar
(`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) と公式 mappings (`03f8985492bda0afc0898465341eb0acef35f570`) の
`cjh.b_(cmx)` / `cjh.a(cjh)` / `cmx.m_()` / `bsu.a(DDD)` / `ewx.a(DDDDDD)` に基づき、
`Drop` に `target` / `thrower` を追加し、`Drop::can_be_picked_up_by`・`player_touch_aabb` で
1.21.1 接触 AABB・`!isSpectator()` / `hp > 0`・`target` UUID 一致・`merge_drops` の `target` 一致・`DRO1`
永続化を閉塞した（`tools/mutation_check.sh` 9/9 `MUTATION_STABILITY=1` 証明、全 670 test 緑）。

