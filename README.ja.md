<div align="center">
  <img src="website/logo.svg" width="84" height="84" alt="Megu Logo">
  <h1>Megu</h1>
  <p><strong>Minecraft: Java Edition 1.21.1 向け クリーンルーム Rust サーバー＆ネイティブ GPU クライアント（Vulkan / DX12 / Metal）</strong></p>
  <p>
    <a href="https://github.com/nekomario28/megu-mc/releases"><img src="https://img.shields.io/github/v/release/nekomario28/megu-mc?include_prereleases&label=release&color=e05d44" alt="Latest Release"></a>
    <a href="https://megu.megumin.tech/"><img src="https://img.shields.io/badge/minecraft-1.21.1-2ea44f" alt="Minecraft 1.21.1"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-LGPL--3.0--or--later-blue" alt="License: LGPL-3.0-or-later"></a>
    <a href="https://github.com/sponsors/nekomario28"><img src="https://img.shields.io/github/sponsors/nekomario28?label=Sponsors&color=db61a2" alt="GitHub Sponsors"></a>
  </p>
  <p>
    <a href="README.md"><strong>English</strong></a> · <strong>日本語</strong> · <a href="https://megu.megumin.tech/"><strong>公式サイト</strong></a> · <a href="https://github.com/nekomario28/megu-mc/releases"><strong>ダウンロード</strong></a> · <a href="docs/operator-guide.md"><strong>ドキュメント</strong></a>
  </p>
</div>

---

Megu は、省メモリ・高速起動・OpenGL 依存ゼロを目指して設計された、**Minecraft: Java Edition 1.21.1** 対応のクリーンルーム Rust 製サーバー＆純 `wgpu` ネイティブ GPU クライアントです。

> **Alpha 版（`v0.1.0-alpha.2`）**: 初期プレリリース版です。1.21.1 の基本ゲームプレイ、純 Vulkan / DX12 / Metal 描画、データ駆動 Mod が動作します。全バニラ要素の完全網羅や任意の複雑な Java Mixin 実行は開発進行中です（詳細: [ロードマップ](specs/ROADMAP.md)）。

> **まずはこちら:** **[運用ガイド](docs/operator-guide.md)** で、専用サーバー（`megu`）とネイティブ GPU クライアント（`megu-client-gpu`）の明確な役割分担、初回起動、Mod、管理コマンドを確認できます。

## 使い方（プレイヤー向け）

**[ダウンロードページ](https://megu.megumin.tech/#downloads)** または **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases)** から、プレイスタイルに合わせて **以下のカテゴリのどれか 1 つ** を選んで導入します（よく使われる順）：

### 1. 公式ランチャー＆単体クライアント導入（`megu-installer` — GUI / CLI 両対応・Java 不要）

ネイティブ GPU クライアント（`megu-client-gpu`）を `<minecraft_dir>/bin/` へ自動配置し、公式 Minecraft ランチャーに **Megu (1.21.1)** プロファイルを登録します（`gui` / `--gui` 引数で GUI 画面、引数なしで 1 クリック自動導入）：

> **プレイ前にターミナル作業は不要です。** `megu-client-gpu` は初回起動時に、公式ランチャー / PrismLauncher / Modrinth App の既存 1.21.1 クライアント JAR を再利用するか、Mojang の CDN からバックグラウンドで取得して SHA-1 検証します。`megu-installer` はバイナリ配置とランチャープロファイル登録だけを行う補助ツールです。
>
> **Java のメモリ設定は不要です。** Megu 本体は純 Rust 製（JVM ゼロ・OpenGL ゼロ）のため、公式ランチャーや Prism 側の Java メモリ割り当て（`-Xmx`）調整は**不要**です。既定の「自動」のままで動作し、必要に応じてゲーム内の **設定 → ビデオ** からキャッシュ上限スライダー（OS メモリ逼迫時の自動解放ガード付き）をいつでも調整できます。

| OS | ファイル名 / コマンド | 説明 |
| :--- | :--- | :--- |
| **Windows (x86_64)** | **[`megu-installer-windows-x86_64.exe`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-installer-windows-x86_64.exe)** | Java 不要の単体 `.exe` インストーラ（通常版 `%APPDATA%\.minecraft` ＆ Microsoft Store 版に自動対応。`megu-installer-windows-x86_64.exe gui` で GUI 起動） |
| **Linux (x86_64)** | `curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh \| sh`<br>*(または **[`megu-installer-linux-x86_64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-installer-linux-x86_64)**)* | 通常版 `~/.minecraft` と Flatpak 版（`~/.var/app/com.mojang.Minecraft/.minecraft`）を自動判別して導入 |
| **macOS (Apple Silicon)** | **[`megu-installer-macos-arm64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-installer-macos-arm64)** | `~/Library/Application Support/minecraft` 向け単体ネイティブインストーラ |

### 2. Modrinth App / Prism Launcher 導入（Modpack・インスタンス形式）

| ランチャー | ファイル名 / コマンド | 説明 |
| :--- | :--- | :--- |
| **Modrinth App / Prism Launcher** | **[`megu-1.21.1.mrpack`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-1.21.1.mrpack)** | 標準 Modrinth `.mrpack` パッケージ — ランチャー画面へドラッグ＆ドロップするだけで導入 |
| **Prism Launcher（ネイティブ ZIP）** | **[`megu-prism-1.21.1.zip`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-prism-1.21.1.zip)** | Prism 用インスタンス ZIP（または `megu-installer prism` でローカル自動生成） |

## 検証済み機能のハイライト（次期 alpha.3）

- **サーバー:** Vanilla 1.21.1、既定ポート **25565**。GameRule 51/51、エンチャント 42/42、効果 39/39、バニラの燃料と死亡メッセージに対応。純 Rust 製の TTY 自動判定 TUI / プレーン CLI（`--cli`）、`systemd` `sd_notify`、Hub の `/server`・`/hub`・`/world`、Survival Moderator の `/megu mod`、認証・DDoS 対策・入口での unwrap 排除、`.mmod` / `.jar` 識別。
- **クライアント:** 純 `wgpu` の目に優しい描画（`comfort_exposure`、`indirect_light_bounce`、Snell の法則による水の屈折、朝夕の大気光学、半透明 3 層雲）。エンティティ・Mob・ブロックエンティティのパーツ別照明、Mob のカスケード影と接触影、バニラ・レッドストーン・Mod の光源に対応するハードコード不要の動的照明、3D/2D エンチャントグリント。初回音量はマスター 50% / 音楽 45%、`Loud Sound Limiter` は dB 表示。Mod Menu は Installed / `[+ Browse]` とプロセス内 Soft Reload に対応。E2EE フレンドと LAN は `127.0.0.1:0` の隔離ホストを使用。OS 言語・クリップボード・日本語 IME・4 行の看板編集に対応。
- **明示的な拡張（厳密な Vanilla 互換のため既定ではオフ）:** 3D 方向性影光源のゲームプレイ（`directional_shadow_light`、`/megu shadowlight on|off`、クライアント設定のローカルトグル）、チャンクローダー不要の O(1) 非ロード中追いつき処理（`lazy_catchup`、`/megu lazycatchup on|off`、クライアント設定のローカルトグル）、自由形状のネザーポータル（`MEGU_CUSTOM_PORTAL_SHAPES=1`）。

全バニラのワールド生成・AI や任意の Java Mixin 実行まで保証するものではありません。運用上の制限は [運用ガイド](docs/operator-guide.md) を参照してください。

## 使い方（サーバー管理者・全 CLI ツール一式）

専用サーバー `megu`・GPU クライアント `megu-client-gpu`・各種 CLI ツール一式を同梱した OS 別フルアーカイブ（`megu-<version>-<target>.tar.gz`）を **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases/tag/v0.1.0-alpha.2)**（`v0.1.0-alpha.2`）から取得します。1 リリースあたり 8 ターゲットを公開しています：`windows-x86_64`、`windows-arm64`、`linux-x86_64`、`linux-arm64`、`linux-x86_64-musl`、`linux-arm64-musl`、`macos-arm64`、`macos-x86_64`。各アーカイブには `megu`（専用サーバー）、`megu-client-gpu`、`megu-client`、`megu-installer`、`megu-mod-check`、`megu-world-migrate`、`megu-init`、`megu-admin` が同梱されます。`*-musl` アーカイブは静的リンクの **サーバー専用** 版（`megu` + `megu-installer`）です。

| ターゲット | フルアーカイブ | 同梱バイナリ |
| :--- | :--- | :--- |
| **`windows-x86_64`** | **[`megu-v0.1.0-alpha.2-windows-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-windows-x86_64.tar.gz)** | `megu.exe`、`megu-client-gpu.exe`、`megu-client.exe`、`megu-installer.exe`、`megu-mod-check.exe`、`megu-world-migrate.exe`、`megu-init.exe`、`megu-admin.exe` |
| **`windows-arm64`** | **[`megu-v0.1.0-alpha.2-windows-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-windows-arm64.tar.gz)** | `megu.exe`、`megu-client-gpu.exe`、`megu-client.exe`、`megu-installer.exe`、`megu-mod-check.exe`、`megu-world-migrate.exe`、`megu-init.exe`、`megu-admin.exe` |
| **`linux-x86_64`** | **[`megu-v0.1.0-alpha.2-linux-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-x86_64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-client`、`megu-installer`、`megu-mod-check`、`megu-world-migrate`、`megu-init`、`megu-admin` |
| **`linux-arm64`** | **[`megu-v0.1.0-alpha.2-linux-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-arm64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-client`、`megu-installer`、`megu-mod-check`、`megu-world-migrate`、`megu-init`、`megu-admin` |
| **`linux-x86_64-musl`** | **[`megu-v0.1.0-alpha.2-linux-x86_64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-x86_64-musl.tar.gz)** | サーバー専用（静的リンク）: `megu`、`megu-installer` |
| **`linux-arm64-musl`** | **[`megu-v0.1.0-alpha.2-linux-arm64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-linux-arm64-musl.tar.gz)** | サーバー専用（静的リンク）: `megu`、`megu-installer` |
| **`macos-arm64`** | **[`megu-v0.1.0-alpha.2-macos-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-macos-arm64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-client`、`megu-installer`、`megu-mod-check`、`megu-world-migrate`、`megu-init`、`megu-admin` |
| **`macos-x86_64`** | **[`megu-v0.1.0-alpha.2-macos-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.2/megu-v0.1.0-alpha.2-macos-x86_64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-client`、`megu-installer`、`megu-mod-check`、`megu-world-migrate`、`megu-init`、`megu-admin` |

> **どのバイナリを使えばいい？** `megu-client-gpu` がプレイ用のネイティブ GPU ゲームクライアント（`megu-client` はヘッドレス / CLI クライアント兼ステータス確認用）、`megu` が専用サーバーです。`megu-client-gpu` は初回起動時に Vanilla 1.21.1 アセットを自身で取得するため、`megu-installer` はバイナリ配置とランチャープロファイル登録の補助ツールであり、必須ではありません。

```sh
./megu-installer server --dir ./megu-server
# ./megu-server/eula.txt を確認して eula=true に変更後、起動：
cd ./megu-server && ./megu                 # 既定のポートは 25565 番
```

- サーバー設定・外部公開時の注意点: **[運用ガイド](docs/operator-guide.md)** · **[SECURITY_NOTES.md](SECURITY_NOTES.md)**
- ポート開放なしのフレンド招待・`FRL3` エンドツーエンド暗号化・信頼ストア: **[フレンドプレイと E2EE](docs/friend-play-e2ee.md)**
- 既存 Fabric / NeoForge Mod・タグ・ケイパビリティ・Mod 入りセーブの扱い: **[Mod 互換性](docs/mod-compatibility.md)**
- Docker ビルド: `docker build -f docker/Dockerfile .`

## 使い方（Mod・シェーダー作者向け）

- データ駆動の Fabric / NeoForge Mod を `mods/` に配置するか、`megu-mod-check --scan-mods-dir ./mods`（またはブラウザ上の **[Modpack 診断](https://megu.megumin.tech/#checker)**）で JVM 不要の事前検査を行えます。
- `.mmod` 形式と Modrinth / CurseForge 向け `--compat-jar`: **[Mod SDK ガイド](docs/mod-sdk-guide.md)** · **[SDK 開発ガイド](sdk/AGENTS.md)**
- 制作ガイド・WGSL シェーダー仕様: **[Mod・シェーダー作者向けガイド](docs/mod-author-guide.md)**
- 内包 JAR・タグ名前空間・Mod 同期・Mod 入りセーブの扱いの全体像: **[Mod 互換性](docs/mod-compatibility.md)**

## ソースからのビルド

ビルドには **Rust `1.98.0`**（[rust-toolchain.toml](rust-toolchain.toml) 固定）と Python 3 が必要です：

```sh
python3 tools/dev_check.py
cargo build --locked --release
```

- クレート構成・テスト手順: **[開発ガイド](docs/development.md)** · **[クレート構成](docs/architecture-crates.md)**

## コントリビュート（Pull Request）

**[参加ガイド (CONTRIBUTING.md)](CONTRIBUTING.md)** と **[行動規範 (CODE_OF_CONDUCT.md)](CODE_OF_CONDUCT.md)** を参照してください。すべてのコミットには DCO 署名（`git commit -s`）が必要です。脆弱性の非公開報告は **[SECURITY.md](SECURITY.md)** を参照してください。

## 開発支援（Support Us）

Megu は `LGPL-3.0-or-later` の完全無料 OSS です。継続的な CI 維持・実機 GPU 検証・互換検証はコミュニティの支援によって支えられています：

- **個人・法人スポンサー**: **[GitHub Sponsors (`github.com/sponsors/nekomario28`)](https://github.com/sponsors/nekomario28)**
- **ホスティング事業者・法人提携窓口**: **[partners@megumin.tech](mailto:partners@megumin.tech)**

## ライセンス

**[LGPL-3.0-or-later](LICENSE)** で配布されます（付随 GPLv3 全文は [docs/COPYING.GPL](docs/COPYING.GPL)、詳細は [LICENSING.md](LICENSING.md)・[NOTICE](NOTICE)・[THIRD_PARTY.md](THIRD_PARTY.md)）。実行時に動的ロードされる外部 Mod JAR にコピーレフトは波及しません。

*Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft. ([TRADEMARK.md](TRADEMARK.md))*
