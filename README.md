# Megu (`megu-mc`)

> Clean-room Rust engine & native GPU client for **Minecraft: Java Edition 1.21.1** (Protocol 767).
>
> **[Official Site](https://megu.megumin.tech/)** · **[Releases](https://github.com/nekomario28/megu-mc/releases)** · **[License: LGPL-3.0-or-later](LICENSE)** · **[Sponsors](https://github.com/sponsors/nekomario28)**

Minecraft: Java Edition 1.21.1 互換を目指す、**100% クリーンルームの Rust 製サーバー＆ネイティブ GPU クライアント（Vulkan / DX12 / Metal・OpenGL 依存ゼロ）** です。バニラ挙動の忠実な再現を最優先に、低メモリ・高速起動・安全な Mod 互換の両立を目指しています。

> **Alpha (`v0.1.0-alpha.1`)**: 開発初期のプレリリース版です。全バニラ要素や任意の複雑 Java/Mixin Mod を無条件にネイティブ実行できる段階ではありません。詳細は [ロードマップ](specs/ROADMAP.md) を参照してください。
>
> *Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.*（[TRADEMARK.md](TRADEMARK.md)）

## 特徴

- **バニラ 1.21.1 互換重視**: Protocol 767 の主要ゲームプレイ・レッドストーン・コマンド・サバイバル挙動を公式 1.21.1 と継続照合
- **軽量 Rust サーバー**: 高速起動・省メモリ設計、Tick 負荷ゼロの外部 TUI ダッシュボード付き
- **純ネイティブ GPU クライアント**: Linux (Vulkan) / Windows (DX12 & Vulkan) / macOS (Metal) 対応、OpenGL 依存なし、手元の `1.21.1.jar` からアセットを直接読込
- **Mod 互換＆安全検査**: Fabric / NeoForge のデータ駆動 Mod 読込と、JVM 不要の静的セキュリティ検査を内蔵
- **簡単導入**: 公式ランチャー・Prism Launcher・Modrinth・サーバー用の導入ツールを同梱

## クイックスタート

[GitHub Releases](https://github.com/nekomario28/megu-mc/releases) または [megu.megumin.tech](https://megu.megumin.tech/) からお使いの OS 向けアーカイブを取得してください。

### クライアント導入（公式ランチャー / Prism）

```sh
# 公式ランチャーへ導入（Windows は megu-installer.exe）
./megu-installer client --mc-version 1.21.1 --client-bin ./megu-client-gpu --bootstrap-jar ./megu-bootstrapper.jar

# Prism Launcher インスタンスを生成
./megu-installer prism --mc-version 1.21.1 --client-bin ./megu-client-gpu

# または GUI/CLI ブートストラッパーを実行
java -jar megu-bootstrapper.jar
```

### サーバー起動

```sh
# サーバーディレクトリを初期化（eula.txt は既定で eula=false）
./megu-installer server --dir ./megu-server --server-bin ./megu

# eula.txt を確認・同意したうえで起動（既定バインド: 127.0.0.1）
cd ./megu-server
./megu --port 25565
```

外部公開時の設定や運用手順は [運用ガイド](docs/operator-guide.md) と [SECURITY_NOTES.md](SECURITY_NOTES.md) を参照してください。

## 現在の対応範囲（`v0.1.0-alpha.1`）

| 対応済み | 未完了・今後の予定 |
|---|---|
| 1.21.1 基本ゲームプレイ・サバイバル・レッドストーン・Protocol 767 | 全バイオーム・全構造物・全エンティティ AI の完全網羅 |
| 純 Vulkan / DX12 / Metal GPU 描画・UI・サウンド | すべての外部 OptiFine / Iris シェーダーパック完全再現 |
| データ駆動 Mod 読込・静的セキュリティ検査 | 任意の複雑 Java / Mixin Mod の無条件ネイティブ実行 |
| Linux / macOS / Windows 3 OS ビルド＆インストーラ | 大規模マルチプレイヤー長期稼働・全 OS 実機 GPU 網羅検証 |

## ソースからのビルド

前提：Rust `1.98.0`（[rust-toolchain.toml](rust-toolchain.toml) 固定）・Python 3。

```sh
python3 tools/dev_check.py
cargo build --locked --release
```

検証データ生成・テスト・アーキテクチャの詳細は [開発ガイド](docs/development.md)・[クレート構成](docs/architecture-crates.md)・[CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。

## ドキュメント

- [運用ガイド](docs/operator-guide.md) / [開発ガイド](docs/development.md) / [Mod 作者向けガイド](docs/mod-author-guide.md)
- [ロードマップ](specs/ROADMAP.md) / [クレート構成](docs/architecture-crates.md) / [参加ガイド (DCO)](CONTRIBUTING.md)
- [セキュリティ](SECURITY.md) / [ライセンス詳細](LICENSING.md) / [第三者帰属](THIRD_PARTY.md) / [商標ガイドライン](TRADEMARK.md)

## 開発支援

Megu のクライアント・サーバー機能はすべて完全無料 OSS として提供しています。

- **個人・法人スポンサー**: [GitHub Sponsors](https://github.com/sponsors/nekomario28)
- **ホスティング・法人提携窓口**: `partners@megumin.tech`（[megu.megumin.tech/#support](https://megu.megumin.tech/#support)）

## ライセンスとクリーンルーム方針

- **ライセンス**: **`LGPL-3.0-or-later`**（[LICENSE](LICENSE)・付随 GPLv3 [docs/COPYING.GPL](docs/COPYING.GPL)・[NOTICE](NOTICE)・[LICENSING.md](LICENSING.md)）
- 実行時に動的ロードされる独立した外部 Mod JAR にはコピーレフトは波及しません。
- 100% クリーンルームのオリジナル Rust 実装であり、Mojang の逆コンパイルコード・アセットおよび PolyForm Shield / All Rights Reserved のソースコードやシェーダーは一切コピー・同梱していません。寄稿は `LGPL-3.0-or-later` 提供と DCO（`git commit -s`）を前提とします。
