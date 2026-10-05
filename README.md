# mc-spine

Minecraft Java 1.21.1（protocol 767）向けの実験的なRustサーバー。保存されていたv2.37のソース、datapack、プロトコル設定を復元した。Minecraft全体との互換性が完成した実装ではない。

Cargoのpackage version `0.0.0` は保存時の値を維持している。`AUDIT_WIRE.md` と `SECURITY_NOTES.md` は当時の監査記録であり、現在の安全性や完成度を保証する資料ではない。

## ビルドと確認

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
```

設定とdatapackは相対パスで読むため、リポジトリ直下から実行する。既存ワールドは復元ソースに含めていない。

```sh
MC_SPINE_PORT=25699 cargo run --locked
```

ゲームlistenerは `0.0.0.0`、resource pack HTTPは固定ポート25598。起動前にポートの使用状況を確認する。`MC_SPINE_WG=old` で旧world generatorを選択できる。

## 依存値が安定した処理の共通化

`CachedLookup<K, V>` は直近の完全な依存キーと純粋な結果を一組だけ保持する。検索失敗も保持し、キーが変わればその呼び出しで再計算する。同期は呼び出し側が担う。

かまどの固定精錬結果と燃料時間に適用した。datapackレシピとingredient tagは変更可能なので、精錬候補の判定時に引き続き確認する。tick、燃焼時間、進捗、インベントリ更新は既存の処理で実行する。キャッシュは保存せず、ワールド読込時に空で作成する。

他の処理へ適用する際は、計算が純粋であること、キーがすべての依存を含むこと、変更と同じ呼び出しで再計算されることを確認する。変更可能なテーブルの参照を含める場合は、そのスナップショットと整合するrevisionも必要になる。

[復元と検証の記録](RESTORATION.md)を参照。

## Java mod互換層

[段階設計](specs/mod-compatibility/README.md)と[Fabric接続の診断](compat/fabric-probe/README.md)を追加した。世界の正本はRustに置き、実Fabric Loader・MixinからJNIで操作する。診断用modの基盤検証に加え、既存Seed Protect JARのRedirect契約と名前変換を検証した。通常サーバーへの統合、Vanilla参照との同等性、NeoForge実行は未検証。通常ビルドではJava実行機能を有効にしない。

## Selected Java mod contract

Java compatibility is opt-in. The pinned Seed Protect Fabric artifact and its NeoForge event counterpart now reach the normal player/adult-cow farmland landing path. Rust keeps the only authoritative world; one synchronous JVM owner executes callbacks, and a native commit replaces farmland only if its original state still matches. Cancellation preserves farmland while existing fall damage continues.

Build and validation instructions are in [Fabric contract](compat/fabric-probe/README.md) and [NeoForge event adapter](compat/neoforge-events/README.md). Set exactly one of `MC_SPINE_FABRIC_CLASSPATH` or `MC_SPINE_NEOFORGE_CLASSPATH` to its built absolute classpath-file path, and use the selected original JAR alone in that server working directory's `mods/`. Unknown mod bytes and required transformation failures reject startup before listeners open. Without those settings, the default Rust server loads no JVM.

This supports one pinned feature contract, not arbitrary Minecraft mods. Full FML/NeoForge startup, all-entity collision physics, crop/loot behavior and general Minecraft API object identity remain outside the verified scope. See [compatibility specification](specs/mod-compatibility/README.md) for the open gates.
