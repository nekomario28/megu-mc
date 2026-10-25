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
署名は削除せず、プローブは独立packageで実行する。サーバーの起動・download・UnsafeによるPlayer偽装は不要。

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

検証の実行結果とファイルhashは `run.json`、対応する生ログはこのディレクトリに保存する。
このsliceの結果は、1.21.1 datapack移行ブランチ全体の受け入れではない。

未検証・未移植: 複数閲覧者への同期、window/state-id整合、閉塞判定・開閉音、正しい向き／left-right／異種チェストの接続判定、shulkerの中身制約、任意component patchの意味的同値、特殊結果スロット、全エンティティ挙動。
続きは[移植順序](../../../specs/horizon-gameplay/README.md)を参照。
