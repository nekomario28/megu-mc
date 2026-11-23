# Megu (`megu-mc`)

**[English (README.md)](README.md)** | **日本語**

> **Minecraft: Java Edition 1.21.1**（Protocol 767）向けのクリーンルーム Rust エンジン＆ネイティブ GPU クライアント。
>
> **[公式サイト](https://megu.megumin.tech/)** · **[ダウンロード (Releases)](https://github.com/nekomario28/megu-mc/releases)** · **[ライセンス: LGPL-3.0-or-later](LICENSE)** · **[Sponsors](https://github.com/sponsors/nekomario28)**

Megu は、Minecraft: Java Edition 1.21.1 互換を目指す **100% クリーンルームの Rust 製サーバー＆ネイティブ GPU クライアント（Vulkan / DX12 / Metal・OpenGL 依存ゼロ）** です。バニラ挙動の忠実な再現を最優先に、省メモリ・高速起動・安全な Mod 互換の両立を目指しています。

> **Alpha 版（`v0.1.0-alpha.1`）に関する注意**: 現在は開発初期のプレリリース版です。すべてのバニラ要素や任意の複雑な Java/Mixin Mod を無条件にネイティブ実行できる段階ではありません。現在の対応境界は [ロードマップ](specs/ROADMAP.md) を参照してください。
>
> *Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.*（[TRADEMARK.md](TRADEMARK.md)）

---

## 主な特徴

- **バニラ 1.21.1 互換重視**: Protocol 767 の主要ゲームプレイ・サバイバル・レッドストーン・コマンド挙動を公式 1.21.1 と継続照合
- **軽量 Rust サーバー**: 高速起動・低メモリ設計、サーバー負荷ゼロの外部 TUI ダッシュボード付き
- **ネイティブ GPU クライアント**: Linux (Vulkan) / Windows (DX12 & Vulkan) / macOS (Metal) 対応、OpenGL 依存ゼロ、手元の `1.21.1.jar` からアセットを直接読込
- **安全な Mod 互換**: Fabric / NeoForge のデータ駆動 Mod 読込と、JVM 不要の静的セキュリティ検査を内蔵
- **ワンクリック導入**: 公式ランチャー・Prism Launcher・Modrinth（`.mrpack`）・専用サーバー向けの単体ネイティブインストーラ（Windows `.exe` / Linux / macOS）を用意

---

## クイックスタート（3 つの導入方法から 1 つ選ぶだけ）

**[GitHub Releases](https://github.com/nekomario28/megu-mc/releases)** または **[megu.megumin.tech](https://megu.megumin.tech/)** から、お使いの環境に合わせて **どれか 1 つ** を選んでください：

| プレイスタイル | 導入手順 |
|---|---|
| **① 公式ランチャー / 単体クライアント**<br>*(Windows / Linux / macOS)* | **Windows**: **`megu-installer-windows-x86_64.exe`** をダウンロードしてダブルクリック（Java 不要）。<br>**Linux**: `curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh \| sh` を 1 行実行。<br>**macOS**: **`megu-installer-macos-arm64`**（または `.tar.gz` 内の `./megu-installer`）を実行。 |
| **② Prism Launcher / Modrinth App** | **`megu-1.21.1.mrpack`** をダウンロードしてランチャーにドラッグ＆ドロップ（または `megu-installer prism` を実行）。 |
| **③ 専用サーバー (`megu`)** | OS 向けアーカイブを展開し、サーバーフォルダ初期化 → `eula.txt` 同意（`eula=true`）→ 起動：<br>`./megu-installer server --dir ./megu-server`<br>`cd ./megu-server && ./megu --port 25565` |

ローカル（`127.0.0.1`）以外へサーバーを公開する前の設定は [運用ガイド](docs/operator-guide.md) と [SECURITY_NOTES.md](SECURITY_NOTES.md) を参照してください。

---

## 現在の対応範囲（`v0.1.0-alpha.1`）

| 対応済み | 未完了・今後の予定 |
|---|---|
| 1.21.1 基本ゲームプレイ・サバイバル・レッドストーン・Protocol 767 | 全バイオーム・全構造物・全エンティティ AI の完全網羅 |
| 純 Vulkan / DX12 / Metal GPU 描画・UI・サウンド | すべての外部 OptiFine / Iris シェーダーパック完全再現 |
| データ駆動 Mod 読込・静的セキュリティ検査 | 任意の複雑 Java / Mixin Mod の無条件ネイティブ実行 |
| Linux / macOS / Windows 3 OS ビルド＆インストーラ | 大規模マルチプレイヤー長期稼働・全 OS 実機 GPU 網羅検証 |

---

## ソースからのビルド

前提：Rust `1.98.0`（[rust-toolchain.toml](rust-toolchain.toml) 固定）・Python 3。

```sh
python3 tools/dev_check.py
cargo build --locked --release
```

検証データ生成・テスト・アーキテクチャの詳細は [開発ガイド](docs/development.md)・[クレート構成](docs/architecture-crates.md)・[CONTRIBUTING.md](CONTRIBUTING.md) を参照してください。

---

## ドキュメント

- **運用・利用ガイド**: [運用ガイド](docs/operator-guide.md) · [Mod 作者向けガイド](docs/mod-author-guide.md) · [セキュリティ運用ノート](SECURITY_NOTES.md)
- **開発・設計**: [開発ガイド](docs/development.md) · [クレート構成](docs/architecture-crates.md) · [ロードマップ](specs/ROADMAP.md) · [参加ガイド (DCO)](CONTRIBUTING.md) · [行動規範](CODE_OF_CONDUCT.md)
- **法務・セキュリティ**: [セキュリティポリシー](SECURITY.md) · [ライセンス詳細](LICENSING.md) · [第三者帰属](THIRD_PARTY.md) · [商標ガイドライン](TRADEMARK.md)

## 開発支援

Megu のクライアント・サーバー機能はすべて `LGPL-3.0-or-later` の完全無料 OSS として提供しています。

- **個人・法人スポンサー**: [GitHub Sponsors](https://github.com/sponsors/nekomario28)
- **ホスティング・法人提携窓口**: `partners@megumin.tech`（[megu.megumin.tech/#support](https://megu.megumin.tech/#support)）

## ライセンスとクリーンルーム方針

- **ライセンス**: **`LGPL-3.0-or-later`**（[LICENSE](LICENSE)・付随 GPLv3 [docs/COPYING.GPL](docs/COPYING.GPL)・[NOTICE](NOTICE)・[LICENSING.md](LICENSING.md)）
- 実行時に動的ロードされる独立した外部 Mod JAR にはコピーレフトは波及しません。
- 100% クリーンルームのオリジナル Rust 実装であり、Mojang の逆コンパイルコード・アセットおよび PolyForm Shield / All Rights Reserved のソースコードやシェーダーは一切コピー・同梱していません。寄稿は `LGPL-3.0-or-later` 提供と DCO（`git commit -s`）を前提とします。
