<div align="center">
  <img src="website/logo.svg" width="96" height="96" alt="Megu ロゴ">
  <h1>Megu</h1>
  <p><strong>Minecraft: Java Edition 1.21.1 向け クリーンルーム Rust サーバー＆ネイティブ GPU クライアント — JVM ゼロ・OpenGL ゼロ。</strong></p>
  <p>
    <a href="https://github.com/nekomario28/megu-mc/releases"><img src="https://img.shields.io/github/v/release/nekomario28/megu-mc?include_prereleases&style=flat-square&label=release&color=e05d44" alt="Latest Release"></a>
    <a href="https://github.com/nekomario28/megu-mc/actions/workflows/ci.yml"><img src="https://github.com/nekomario28/megu-mc/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <a href="https://github.com/nekomario28/megu-mc/actions/workflows/cross-platform.yml"><img src="https://github.com/nekomario28/megu-mc/actions/workflows/cross-platform.yml/badge.svg" alt="Cross-Platform"></a>
    <a href="https://megu.megumin.tech/"><img src="https://img.shields.io/badge/minecraft-1.21.1-2ea44f?style=flat-square" alt="Minecraft 1.21.1"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-LGPL--3.0--or--later-blue?style=flat-square" alt="License: LGPL-3.0-or-later"></a>
    <a href="https://github.com/sponsors/nekomario28"><img src="https://img.shields.io/github/sponsors/nekomario28?label=Sponsors&color=db61a2&style=flat-square" alt="GitHub Sponsors"></a>
  </p>
  <p>
    <a href="README.md"><strong>English</strong></a> · <strong>日本語</strong> · <a href="https://megu.megumin.tech/"><strong>公式サイト</strong></a> · <a href="https://github.com/nekomario28/megu-mc/releases"><strong>ダウンロード</strong></a> · <a href="docs/operator-guide.md"><strong>ドキュメント</strong></a> · <a href="https://github.com/nekomario28/megu-mc/discussions"><strong>Discussions</strong></a> · <a href="https://megu.megumin.tech/#support"><strong>サポート</strong></a>
  </p>
</div>

---

Megu は、**Minecraft: Java Edition 1.21.1** のクリーンルーム再実装です。専用サーバー（`megu`）と純 `wgpu` ネイティブ GPU クライアント（`megu-client-gpu`）を純 Rust で構築し、省メモリ・高速起動・OpenGL / JVM 依存ゼロを実現しています。

> **Alpha 版（`v0.1.0-alpha.4`）**: 初期プレリリース版です。1.21.1 の基本ゲームプレイ、純 Vulkan / DX12 / Metal 描画、データ駆動 Mod が動作します。全バニラ要素の完全網羅や任意の複雑な Java Mixin 実行は開発進行中です（詳細: [ロードマップ](specs/ROADMAP.md)）。

> **まずはこちら:** **[運用ガイド](docs/operator-guide.md)** で、専用サーバー（`megu`）とネイティブ GPU クライアント（`megu-client-gpu`）の明確な役割分担、初回起動、Mod、管理コマンドを確認できます。

## 実測パフォーマンス

`WARM=1 bash tools/load_compare.sh 20 0` — 同一シード・同一描画距離・同一移動スクリプトで **移動するプレイヤー** に 20 秒間負荷をかける実測テスト（Linux x86_64）。バニラ 1.21.1 `server.jar` 対 `megu` の比較です。

| 指標 | **Megu** | バニラ 1.21.1 | 差分 |
| :--- | ---: | ---: | ---: |
| CPU 時間 | **3.42 秒** | 5.39 秒 | **−37%** |
| チャンク生成コスト | **20.60 ms/チャンク** | 32.87 ms/チャンク | **−37%** |
| メモリ（VmRSS） | **331 MiB** | 1,122 MiB | **−70%** |
| スレッド数 | **16** | 66 | **−76%** |

<sub>アイドル時の `megu` は約 122 MiB です。`WARM=1 bash tools/load_compare.sh 20 0` で再現できます。正直な留意点: Megu はバニラより未実装機能が多い Alpha 版のため、差の一部は「同じことを安く」ではなく「やる処理が少ない」ことに由来します。全シードでの 100% ワールド生成互換と任意の Java Mixin 実行は開発進行中です。</sub>

## Megu の特長

| 専用サーバー（`megu`） | ネイティブ GPU クライアント（`megu-client-gpu`） | Mod・ネイティブ SDK |
| :--- | :--- | :--- |
| バニラ 1.21.1・既定ポート **25565** | 純 `wgpu` — Vulkan / DX12 / Metal | `.mmod` ネイティブ形式 + Modrinth / CurseForge JAR 向け `--compat-jar` |
| 非同期チャンク配信 + 事前圧縮チャンクフレームキャッシュ | OpenGL ゼロ・JVM ゼロの単一ネイティブバイナリ | JVM 不要の段階的 NeoForge / Fabric 互換（`Create 6.0.10`、35+ 混合 Modpack E2E） |
| 純 Rust 製 TTY 自動判定 TUI / `--cli`、`systemd` `sd_notify` | グリーディメッシュ化された半透明雲、64 灯ダイナミックライトキャッシュ | JVM なしの事前検査 `megu-mod-check`（ブラウザ版: [megu.megumin.tech/#checker](https://megu.megumin.tech/#checker)） |
| 強化された認証・DDoS 対策・unwrap 排除の入口処理 | 初回起動時のハードウェア自動調整 + 目に優しい光学処理 | 型付きビルダー・パッケージング・テストハーネス [`sdk/megu-mod-sdk`](sdk/megu-mod-sdk) |
| 可逆なバニラ Anvil ワールドのインポート / エクスポート | 公式ランチャー・PrismLauncher・Modrinth App と併用可能 | データ駆動 Mod は `mods/` に置くだけ — Java 不要 |

## クイックスタート

**プレイヤー** — **[ダウンロードページ](https://megu.megumin.tech/#downloads)** または **[GitHub Releases](https://github.com/nekomario28/megu-mc/releases)** から 1 つ選ぶだけ：

| OS / ランチャー | ファイル名 / コマンド |
| :--- | :--- |
| **Windows (x86_64)** | [`megu-installer-windows-x86_64.exe`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-installer-windows-x86_64.exe) — 単体インストーラ、`gui` 引数で GUI 起動 |
| **Linux (x86_64)** | `curl -fsSL https://raw.githubusercontent.com/nekomario28/megu-mc/main/tools/install_linux.sh \| sh`（または [`megu-installer-linux-x86_64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-installer-linux-x86_64)） |
| **macOS (Apple Silicon)** | [`megu-installer-macos-arm64`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-installer-macos-arm64) |
| **Modrinth App / Prism** | [`megu-1.21.1.mrpack`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-1.21.1.mrpack)（ドラッグ＆ドロップ）または [`megu-prism-1.21.1.zip`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-prism-1.21.1.zip) |

インストーラは `megu-client-gpu` を `<minecraft_dir>/bin/` へ配置し、**Megu (1.21.1)** ランチャープロファイルを登録します。初回起動時、クライアントは公式ランチャー / PrismLauncher / Modrinth App の既存 1.21.1 クライアント JAR を再利用するか、Mojang CDN から SHA-1 検証付きで取得します。Java も `-Xmx` 調整も不要。必要なら **設定 → ビデオ** の RAM 上限スライダーをいつでも調整できます。

**Linux サーバーアーカイブをインストール前に検証する場合** — 以下は `v0.1.0-alpha.4` に固定した例です。別バージョンを選ぶ場合は `TAG` を明示的に変更してください。リリースには `SHA256SUMS-all.txt` が公開されるため、対象アーカイブの行だけを `SHA256SUMS.txt` に取り出し、`sha256sum -c` で検証します。

```sh
TAG=v0.1.0-alpha.4
TARGET=linux-x86_64
ARCHIVE="megu-${TAG}-${TARGET}.tar.gz"
BASE="https://github.com/nekomario28/megu-mc/releases/download/${TAG}"
curl -fLO "${BASE}/${ARCHIVE}"
curl -fLO "${BASE}/SHA256SUMS-all.txt"
grep -F "  ${ARCHIVE}" SHA256SUMS-all.txt > SHA256SUMS.txt
test "$(wc -l < SHA256SUMS.txt)" -eq 1
sha256sum -c SHA256SUMS.txt
tar -xzf "${ARCHIVE}"
"./megu-${TAG}-${TARGET}/megu-installer" server --dir ./megu-server
```

**サーバー管理者** — フルアーカイブ内の 1 コマンド：

```sh
./megu-installer server --dir ./megu-server
# ./megu-server/eula.txt を確認して eula=true に変更後、起動：
cd ./megu-server && ./megu                 # 既定のポートは 25565 番
```

<details>
<summary><strong>全 12 プラットフォームアーカイブ（v0.1.0-alpha.4）</strong> — デスクトップ版: <code>megu</code>、<code>megu-client-gpu</code>、<code>megu-installer</code>。サーバー専用・muslクライアント版は構成が異なります</summary>

| ターゲット | フルアーカイブ | 同梱バイナリ |
| :--- | :--- | :--- |
| **`windows-x86_64`** | **[`megu-v0.1.0-alpha.4-windows-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-windows-x86_64.tar.gz)** | `megu.exe`、`megu-client-gpu.exe`、`megu-installer.exe` |
| **`windows-arm64`** | **[`megu-v0.1.0-alpha.4-windows-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-windows-arm64.tar.gz)** | `megu.exe`、`megu-client-gpu.exe`、`megu-installer.exe` |
| **`linux-x86_64`** | **[`megu-v0.1.0-alpha.4-linux-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-x86_64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-installer` |
| **`linux-arm64`** | **[`megu-v0.1.0-alpha.4-linux-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-arm64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-installer` |
| **`macos-arm64`** | **[`megu-v0.1.0-alpha.4-macos-arm64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-macos-arm64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-installer` |
| **`macos-x86_64`** | **[`megu-v0.1.0-alpha.4-macos-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-macos-x86_64.tar.gz)** | `megu`、`megu-client-gpu`、`megu-installer` |
| **`linux-x86_64-musl`** | **[`megu-v0.1.0-alpha.4-linux-x86_64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-x86_64-musl.tar.gz)** | サーバー専用（静的リンク）: `megu`、`megu-installer` |
| **`linux-arm64-musl`** | **[`megu-v0.1.0-alpha.4-linux-arm64-musl.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-arm64-musl.tar.gz)** | サーバー専用（静的リンク）: `megu`、`megu-installer` |
| **`freebsd-x86_64`** | **[`megu-v0.1.0-alpha.4-freebsd-x86_64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-freebsd-x86_64.tar.gz)** | サーバー専用: `megu`、`megu-installer` |
| **`linux-riscv64`** | **[`megu-v0.1.0-alpha.4-linux-riscv64.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-riscv64.tar.gz)** | サーバー専用: `megu`、`megu-installer` |
| **`linux-x86_64-musl-client`** | **[`megu-v0.1.0-alpha.4-linux-x86_64-musl-client.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-x86_64-musl-client.tar.gz)** | GPUクライアント専用（動的musl）: `megu-client-gpu` |
| **`linux-arm64-musl-client`** | **[`megu-v0.1.0-alpha.4-linux-arm64-musl-client.tar.gz`](https://github.com/nekomario28/megu-mc/releases/download/v0.1.0-alpha.4/megu-v0.1.0-alpha.4-linux-arm64-musl-client.tar.gz)** | GPUクライアント専用（動的musl）: `megu-client-gpu` |

> **どのバイナリを使えばいい？** `megu-client-gpu` がプレイ用のネイティブ GPU ゲームクライアント（`megu-client` はヘッドレス / CLI クライアント兼ステータス確認用）、`megu` が専用サーバーです。`megu-client-gpu` は初回起動時に Vanilla 1.21.1 アセットを自身で取得するため、`megu-installer` はバイナリ配置とランチャープロファイル登録の補助ツールであり、必須ではありません。

</details>

## 検証済み機能のハイライト（次期 alpha.4）

- **サーバー:** Vanilla 1.21.1、既定ポート **25565**。GameRule 51/51、エンチャント 42/42、効果 39/39、バニラの燃料と死亡メッセージに対応。純 Rust 製の TTY 自動判定 TUI / プレーン CLI（`--cli`）、`systemd` `sd_notify`、Hub の `/server`・`/hub`・`/world`、Survival Moderator の `/megu mod`、認証・DDoS 対策・入口での unwrap 排除、`.mmod` / `.jar` 識別。
- **クライアント:** 純 `wgpu` の目に優しい描画（`comfort_exposure`、`indirect_light_bounce`、Snell の法則による水の屈折、朝夕の大気光学、半透明 3 層雲）。エンティティ・Mob・ブロックエンティティのパーツ別照明、Mob のカスケード影と接触影、バニラ・レッドストーン・Mod の光源に対応するハードコード不要の動的照明、3D/2D エンチャントグリント。初回音量はマスター 50% / 音楽 45%、`Loud Sound Limiter` は dB 表示。Mod Menu は Installed / `[+ Browse]` とプロセス内 Soft Reload に対応。E2EE フレンドと LAN は `127.0.0.1:0` の隔離ホストを使用。OS 言語・クリップボード・日本語 IME・4 行の看板編集に対応。
- **明示的な拡張（厳密な Vanilla 互換のため既定ではオフ）:** 3D 方向性影光源のゲームプレイ（`directional_shadow_light`、`/megu shadowlight on|off`、クライアント設定のローカルトグル）、チャンクローダー不要の O(1) 非ロード中追いつき処理（`lazy_catchup`、`/megu lazycatchup on|off`、クライアント設定のローカルトグル）、自由形状のネザーポータル（`MEGU_CUSTOM_PORTAL_SHAPES=1`）。

全バニラのワールド生成・AI や任意の Java Mixin 実行まで保証するものではありません。運用上の制限は [運用ガイド](docs/operator-guide.md) を参照してください。

## 必要なもの

- **Java は一切不要。** サーバーもクライアントも純 Rust ネイティブバイナリ — JVM のインストールも `-Xmx` 調整も不要です。
- **クライアント:** Vulkan / DirectX 12 / Metal 対応 GPU があれば動作し、初回起動時にハードウェアへ自動調整します。`megu-client-gpu` は公式 1.21.1 アセットを自力で取得 — 既存ランチャーの JAR を再利用するか、Mojang CDN から SHA-1 検証付きで取得します。**クリーンルーム:** 本リポジトリに Mojang のコード・アセットは一切同梱されておらず、必要なデータは実行時に Mojang から取得します。
- **サーバー:** glibc 版と静的 `*-musl` 版のどちらでも動作。既存バニラワールドは `--import-anvil` または `MEGU_ANVIL_WORLD_DIR` で取り込めます。

## 対応プラットフォーム

| プラットフォーム | `megu-client-gpu` | `megu` サーバー |
| :--- | :---: | :---: |
| Windows x86_64 / arm64 | ✓ | ✓ |
| macOS arm64 / x86_64 | ✓ | ✓ |
| Linux x86_64 / arm64（glibc） | ✓ | ✓ |
| Linux x86_64 / arm64（musl・静的リンク） | — | ✓ |

## アーキテクチャ

ルートサーバーに加え 13 の独立クレート — ワークスペースは使わず、各マニフェストがそれぞれの真実のソースです。全体図: **[クレート構成](docs/architecture-crates.md)**。

| クレート | 担当領域 |
| :--- | :--- |
| `megu`（ルート） | 専用サーバー — プロトコル 767 ゲームプレイ・tick エンジン・ワールド生成・セーブ |
| `megu-core` | 正規バージョン定数、`ResourceLocation`、Java 完全一致 RNG、バニラ数学関数 |
| `megu-proto` | プロトコル IR・パケットテーブル（767/777）、コーデック、`VCR1` 近接ボイスリレー |
| `megu-state-store` | 64 シャードランタイムストア、`STV1` スナップショット、`1.21.1 ↔ 26.3` DataFixer、Anvil インスペクタ |
| `megu-server-files` | `server.properties`/eula/ops の永続化、`megu-init`/`megu-admin`、クレーム＆パーティ |
| `mod-adapter-core`・`create-adapter`・`mod-offload` | JVM 不要の Mod 分類、Mixin→Event リフティング、Create 最適化、工場/戦闘カーネル |
| `megu-kernel`・`megu-client-mesh`・`megu-client-ui` | プロトコル 767 カーネル、ボクセルメッシャー + `VFX1` エンジン、クライアント UI |
| `megu-client`・`megu-client-gpu` | ヘッドレス/CLI クライアント、`winit` + `wgpu` ウィンドウ GPU クライアント |
| `megu-mod-sdk` | 型付きビルダー、`.mmod` パッケージング、インメモリ Mod テストハーネス |

## Mod・シェーダー作者向け

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
- サーバー設定・外部公開時の注意点: **[運用ガイド](docs/operator-guide.md)** · **[SECURITY_NOTES.md](SECURITY_NOTES.md)** · Docker: `docker build -f docker/Dockerfile .`
- ポート開放なしのフレンド招待・`FRL3` エンドツーエンド暗号化・信頼ストア: **[フレンドプレイと E2EE](docs/friend-play-e2ee.md)**

## コミュニティ・コントリビュート

質問やアイデアは **[GitHub Discussions](https://github.com/nekomario28/megu-mc/discussions)** へ。GitHub アカウント不要の不具合報告は **[Web 報告フォーム](https://megu.megumin.tech/#report)** からどうぞ。

**[参加ガイド (CONTRIBUTING.md)](CONTRIBUTING.md)** と **[行動規範 (CODE_OF_CONDUCT.md)](CODE_OF_CONDUCT.md)** を参照してください。すべてのコミットには DCO 署名（`git commit -s`）が必要です。脆弱性の非公開報告は **[SECURITY.md](SECURITY.md)** を参照してください。

## 開発支援（Support Us）

Megu は `LGPL-3.0-or-later` の完全無料 OSS です。継続的な CI 維持・実機 GPU 検証・互換検証はコミュニティの支援によって支えられています：

- **個人・法人スポンサー**: **[GitHub Sponsors (`github.com/sponsors/nekomario28`)](https://github.com/sponsors/nekomario28)**
- **ホスティング事業者・法人提携窓口**: **[partners@megumin.tech](mailto:partners@megumin.tech)**

## ライセンス

**[LGPL-3.0-or-later](LICENSE)** で配布されます（付随 GPLv3 全文は [docs/COPYING.GPL](docs/COPYING.GPL)、詳細は [LICENSING.md](LICENSING.md)・[NOTICE](NOTICE)・[THIRD_PARTY.md](THIRD_PARTY.md)）。実行時に動的ロードされる外部 Mod JAR にコピーレフトは波及しません。

*Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft. ([TRADEMARK.md](TRADEMARK.md))*
