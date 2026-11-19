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

## Collaborator クイックスタート（3 コマンド）

前提は Rust 1.98.0（[rust-toolchain.toml](rust-toolchain.toml) で固定）、Python 3、Java 21（NeoForge / Fabric 互換テスト用）です。開発ブランチ（`main` / `perf/worldgen-parallel`）にはテスト用の vanilla データが同梱されているため、clone 直後に以下の 3 ステップで開発・検証を開始できます：

```sh
# 1. ツールチェーンとローカル入力の診断（数秒）
python3 tools/dev_check.py

# 2. アーキテクチャ層・技術的負債・来歴（Provenance）ゲートの検証
bash tools/layers_check.sh && bash tools/debt_ratchet.sh && python3 tools/provenance_check.py

# 3. ビルド・単体テスト・サーバー起動
cargo test --locked -- --test-threads=1
MC_SPINE_PORT=25699 cargo run --locked
```

> **Note**: `tools/export_public_branch.sh` で生成されたクリーンエクスポート環境（Mojang 由来データを含まないツリー）から始める場合は、ステップ 1 の後に手元の 1.21.1 `server.jar` から 1 コマンドでデータを再生成できます：
> `bash tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/1.21.1-server.jar --fetch-fabric-mappings --accept-eula`

通常のゲームlistenerは `127.0.0.1` にbindし、セーブはカレントディレクトリの `world_overlay.bin` に書きます。外部ネットワークに公開しないでください（[SECURITY_NOTES.md](SECURITY_NOTES.md)）。データの種類、生成手順、テストの段階（T0 → T1 → T2）は [開発ガイド](docs/development.md) にまとめています。

## どこから読むか

- 参加する・並列作業の担当境界: [CONTRIBUTING.md](CONTRIBUTING.md) → [開発ガイド](docs/development.md)
- 現在の作業と未解決事項: [specs/README.md](specs/README.md)
- 目標と合格条件: [specs/PLAN.md](specs/PLAN.md) / 次の順序: [specs/ROADMAP.md](specs/ROADMAP.md)
- 設計原則と比較の規律: [DESIGN.md](DESIGN.md)
- 測定値・再現条件: [evidence/](evidence/)
- 診断・固定具の道具: [tools/README.md](tools/README.md)
- ライセンスと出所: [LICENSING.md](LICENSING.md) / [THIRD_PARTY.md](THIRD_PARTY.md)
- セキュリティ: [SECURITY.md](SECURITY.md) / [SECURITY_NOTES.md](SECURITY_NOTES.md)

## ライセンスとデータ

コード本体は **`GPL-3.0-or-later`**（[LICENSE](LICENSE)、[NOTICE](NOTICE)、[LICENSING.md](LICENSING.md)）で配布されています。また、寄稿コードおよびファーストパーティコードは `LGPL-3.0-or-later`（[COPYING.LESSER](COPYING.LESSER)）、`GPL-3.0-or-later`（[LICENSE](LICENSE)）、および `AGPL-3.0-or-later`（[COPYING.AGPL](COPYING.AGPL)）のトリプルライセンスで提供されるものとし、将来 `LGPL-3.0-or-later`・`GPL-3.0-or-later`・`AGPL-3.0-or-later` のいずれの間でも自由に行き来できる柔軟性と、C2ME (`MIT`)・Lithium (`LGPL-3.0`)・NeoForge (`LGPL-2.1`) との完全な互換性を確保しています。Fabric / NeoForge 互換レイヤ経由で動的ロードされる独立した外部 Mod JAR には本体のコピーレフトは波及しません。

貢献の条件は、寄稿コードの `LGPL-3.0-or-later` / `GPL-3.0-or-later` / `AGPL-3.0-or-later` トリプルライセンス提供、DCO の `Signed-off-by`（`git commit -s`）、Mojang 逆コンパイルコードや第三者 JAR を混入させないことです。詳細は [CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。
