# mc-spine

Minecraft Java 1.21.1（protocol 767）向けの実験的なRustサーバーです。小規模・低負荷の世界で、観測できる挙動を保ちながら計算量を減らし、必要な既存Modを接続できるかを試しています。**互換性・軽量性・任意Mod対応は未達で、完成品でも、vanillaより軽いと確認済みの製品でもありません。**

## 何ができて、何ではないか

| 限られた範囲で確認していること | まだ確認していないこと |
|---|---|
| 固定したseed・chunk・生成stageでのworldgen照合（対象fixtureのみ） | 全seed・全biome・全構造物・全dimensionの一致 |
| CLI経由の接続・操作の検査 | 画面を含むM2セッション全体の一致 |
| 限られたFabric／NeoForge契約（Seed Protect等） | 任意のModの互換 |
| 固定条件での計測 | vanillaより軽いという証明、20TPSの証明 |

現在の状態・限界・次の順序は [specs/README.md](specs/README.md) と [specs/ROADMAP.md](specs/ROADMAP.md) が正本です。このREADMEには状態表を複製しません。

## 試す

前提は Rust 1.98.0（[rust-toolchain.toml](rust-toolchain.toml) で固定）と python3 です。リポジトリ直下で実行します。

まず `python3 tools/dev_check.py` を実行してください。ビルドもダウンロードもせず、このチェックアウトで
何が動き何が欠けているか（と次に打つコマンド）を数秒で出します。

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
MC_SPINE_PORT=25699 cargo run --locked
```

通常のゲームlistenerは `127.0.0.1` にbindし、セーブはカレントディレクトリの `world_overlay.bin` に書きます。外部ネットワークに公開しないでください（[SECURITY_NOTES.md](SECURITY_NOTES.md)）。

ビルドとテストには、手元で用意・生成するデータを要求するものがあります。データの種類、生成手順（Minecraft EULAへの同意を含む）、生成データなしで走る確認、テストの段階は [開発ガイド](docs/development.md) にまとめています。テストが緑でも、vanillaとの全機能一致や性能優位を意味しません。

## どこから読むか

- 参加する: [CONTRIBUTING.md](CONTRIBUTING.md) → [開発ガイド](docs/development.md)
- 現在の作業と未解決事項: [specs/README.md](specs/README.md)
- 目標と合格条件: [specs/PLAN.md](specs/PLAN.md) / 次の順序: [specs/ROADMAP.md](specs/ROADMAP.md)
- 設計原則と比較の規律: [DESIGN.md](DESIGN.md)
- 測定値・再現条件: [evidence/](evidence/)
- 診断・固定具の道具: [tools/README.md](tools/README.md)
- ライセンスと出所: [LICENSING.md](LICENSING.md) / [THIRD_PARTY.md](THIRD_PARTY.md)
- セキュリティ: [SECURITY.md](SECURITY.md) / [SECURITY_NOTES.md](SECURITY_NOTES.md)

## ライセンスとデータ

コードは Apache-2.0（[LICENSE](LICENSE)、[NOTICE](NOTICE)）として**提案**されていますが、[LICENSING.md](LICENSING.md) §5 の項目が解消するまで確定していません。公開可能・リリース可能という状態ではありません。

貢献の条件は inbound = outbound（Apache-2.0 §5）、DCO の `Signed-off-by`、GPL／AGPL／LGPL のコード・データを写さない、Mojang由来データや第三者jarを追加しない、です。現行ツリーには方針文書と食い違う追跡データが残っており、その扱いは未解決です。**新しいデータを追加せず**、[CONTRIBUTING.md](CONTRIBUTING.md) の「データと来歴」に従ってください。
