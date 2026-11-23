# Megu (`megu-mc`)

> Clean-room Rust engine & native GPU client targeting **Minecraft: Java Edition 1.21.1** (Protocol 767; secondary target 26.3).
> **Official Site**: [megu.megumin.tech](https://megu.megumin.tech/) · **License**: `LGPL-3.0-or-later` · **Status**: `v0.1.0-alpha.1`

Minecraft: Java Edition 1.21.1（Protocol 767）互換を目指す、**100% クリーンルームの Rust 製サーバー＆ネイティブ GPU クライアント（純 Vulkan / DX12 / Metal・OpenGL 依存ゼロ）** です。
バニラ挙動の忠実な再現を最優先にしつつ、低 RAM・低 CPU・高速起動・安全な Mod 互換（データ駆動＋有界 JVM 委譲境界）を両立することを目指しています。

> **重要（Alpha 版の前提）**: Megu は現在 `v0.1.0-alpha.1` です。任意の複雑な Java/Mixin Mod をネイティブ実行できる完成品ではなく、未実装機能・Tier 3（有界 JVM 委譲）境界が残っています。詳細は下の「できること・境界」と [specs/ROADMAP.md](specs/ROADMAP.md) を参照してください。
>
> *Not an official Minecraft product. Not approved by or associated with Mojang or Microsoft.*（[TRADEMARK.md](TRADEMARK.md)）

---

## 主な特徴（`v0.1.0-alpha.1` 実測・検証済み）

- **1.21.1 バニラ互換（Protocol 767）**:
  - C2S 58/58・S2C 124/124・Gamerules 51/51・コマンドルート 89/89・エンティティ 130/130
  - 対公式 Vanilla 1.21.1 ライブ差分オラクル: `vanilla_live_diff.py --strict` **18/18 MATCH**、サバイバル/クリエイティブ **7/7 PASS**、ブロック形状＆レッドストーン **11/11 PASS**
- **軽量サーバー＋外部ゼロ負荷 TUI**:
  - 起動〜待ち受け **~1.0 秒**・アイドル RAM **`~62 MiB`**・4 スレッド
  - 3 倍省メモリ `LightingChunk`・`SECTION_STORE` 優先キャッシュ・オーバーレイ差分保存・Tick 末合流 WAL・SHA-256 重複排除バックアップ
  - 外部プロセス型ダッシュボード [tools/megu_tui.py](tools/megu_tui.py)（TPS/MSPT 固定スケール配色・CPU%・スレッド数・`ms/ch`・ダーティフレーム抑止でアイドル描画負荷 ~0%）
- **純 `wgpu` ネイティブ GPU クライアント（`megu-client-gpu`・OpenGL 依存 0）**:
  - Linux **Vulkan** / Windows **DX12 & Vulkan** / macOS **Metal**（GL/GLES/EGL/WGL クレート 0）
  - GPU 主導 `multi_draw_indexed_indirect`＋インエンコーダ GPU Hi-Z 深度ピラミッド＋定常フレーム 0 B 差分転送
  - 手元 `1.21.1.jar` からのオンメモリ読込（一時展開 0 B・テクスチャ/音源/言語/スプラッシュ＋`MEGU EDITION` タイトル合成）
  - 組込レシピ＆解説ブラウザ（JEI/EMI/JER/Ponder 風：`R`/`U`/`W` ホットキー・`+` 自動配置・Mod JAR/データパック自動読込）
- **論文ベース数理最適化＆汎用差分基盤（`megu-core`）**:
  - `DiscountedUcbAutotuner`（Garivier & Moulines, ALT 2011）・`P2QuantileEstimator`（Jain & Chlamtac, CACM 1985・ゼロヒープ P95/P99）・`RooflineModel`（Williams–Waterman–Patterson, CACM 2009）・アンチワインドアップ PID・3D Morton・Mara–McGuire 解析球投影・`XorRleDiff`（`XDF1`＋CRC32）をサーバー／クライアント双方で共有
- **汎用 Mod 互換＆フェイルクローズ静的検査**:
  - Fabric / NeoForge 共存レジストリ（`DualLoaderHost`）・データ駆動レシピ/タグ/ルート/ディメンション取込
  - 非実行型バイトコード静的検査（危険なプロセス実行・ネイティブロード・`Unsafe`・JNDI・動的コードはハード隔離、直接ネットワーク/制限付き Foreign Linker はサンドボックス警告分離）
- **らくらく導入（公式ランチャー・Prism・サーバー）**:
  - Java 不要・単体ネイティブインストーラ **`megu-installer`（Windows 版 `megu-installer.exe`・~530 KiB）**
  - ダブルクリック GUI/CLI 対応 **`megu-bootstrapper.jar`（23 KiB）**・Prism 用 `.zip`・Modrinth 用 `.mrpack`

---

## 参考ベンチマーク実測（Alpha・条件明記）

### サーバー：対公式 Vanilla 1.21.1 `server.jar`（`WARM=1` 20 秒移動ビューア・seed 0・view 4 / sim 5・90 チャンク移動・PGO なし）

| サーバー | CPU (s) | `ms/chunk` | RAM (RSS) | OS スレッド | 送信パケット | 配信チャンク |
|---|---:|---:|---:|---:|---:|---:|
| Vanilla 1.21.1 (`server.jar`) | `3.470s` | `21.16` | `1,141.1 MiB` | `64` | `2,798` | `164` |
| **Megu (`v0.1.0-alpha.1`)** | **`1.680s`** | **`12.44`** | **`280.4 MiB`** | **`6`** | **`791`** | `135` |

> **正直な注記**: 両サーバーとも同じ 90 チャンク移動ドライバを使用していますが、配信チャンク総数（164 vs 135）および実装済みバニラ機能の範囲が異なるため、完全同一作業の比較ではありません。

### クライアント：実機 AMD Radeon RX 7800 XT（純 Vulkan・720p・GPU 間接描画＋Hi-Z）

| 描画距離（クライアント既定 `12`・範囲 `6..=32`） | チャンク列 / セクション | 受信＋照明（`O(N)` 増分） | メッシュ生成 | 平均 FPS | 1% Low（P99） | クライアント RSS |
|---|---:|---:|---:|---:|---:|---:|
| `view = 8` | 289 / 578 | `446.8 ms`（`1.55 ms/ch`） | `9.1 ms` | **`1,839 FPS`** | **`1,125 FPS`** | `193.6 MiB` |
| **`view = 12`（既定）** | 625 / 1,250 | `955.1 ms`（`1.53 ms/ch`） | `22.9 ms` | **`1,025 FPS`** | **`655 FPS`** | `347.5 MiB` |
| `view = 16` | 1,089 / 2,178 | `1,659.9 ms`（`1.52 ms/ch`） | `47.9 ms` | **`619 FPS`** | **`489 FPS`** | `555.7 MiB` |

---

## できること・まだできないこと（正直な境界）

| 確認済み（`v0.1.0-alpha.1`） | まだ未完了・対象外 |
|---|---|
| 1.21.1 ライブ差分 18/18・サバイバル 7/7・レッドストーン 11/11 | 全バイオーム・全構造物・全エンティティ AI の 100% 完全網羅 |
| データ駆動 Mod（レシピ・タグ・ルート・ディメンション・静的検査） | 任意の複雑 Java / Mixin Mod の無条件ネイティブ実行（Tier 3 境界あり） |
| 実機 RX 7800 XT 純 Vulkan 描画・GPU 間接描画＋Hi-Z・音響・UI | すべての OptiFine/Iris 外部シェーダーパックの完全再現 |
| Win / macOS / Linux 3 OS ビルド＆インストーラ検証 | 大規模マルチプレイヤー長期稼働・全 OS 実機 GPU 網羅検証 |

---

## クイックスタート

### プレイヤー向け（公式ランチャー / Prism Launcher）
[GitHub Releases](https://github.com/nekomario28/megu-mc/releases) または [megu.megumin.tech](https://megu.megumin.tech/) から導入ツールを取得します：

```sh
# 1. 公式ランチャーへ導入（Java 不要の単体バイナリ・Windows は megu-installer.exe）
./megu-installer client --mc-version 1.21.1 --client-bin ./megu-client-gpu --bootstrap-jar ./megu-bootstrapper.jar

# 2. Prism Launcher インスタンスを 1 発生成
./megu-installer prism --mc-version 1.21.1 --client-bin ./megu-client-gpu

# 3. あるいは 23 KiB の GUI/CLI インストーラ JAR をダブルクリック / 実行
java -jar megu-bootstrapper.jar
```

### サーバー運用者向け
```sh
# 標準 7 フォルダ・run.sh / run.bat・eula.txt (eula=false) を初期化
./megu-installer server --dir ./megu-server --server-bin ./megu

# eula.txt の内容を確認し、同意する場合のみ eula=true にして起動
cd ./megu-server
./megu --port 25565
# または外部 TUI ダッシュボード付きで起動（サーバー Tick 負荷 0）
python3 tools/megu_tui.py -- ./megu --port 25565
```

> 既定のバインド先は `127.0.0.1` です。外部公開前の設定は [SECURITY_NOTES.md](SECURITY_NOTES.md) と [docs/operator-guide.md](docs/operator-guide.md) を参照してください。初回起動時に `server.toml` が生成され、`MEGU_*` 環境変数が常に優先されます。

### 開発者・コントリビュータ向け
前提：Rust 1.98.0（[rust-toolchain.toml](rust-toolchain.toml) 固定）・Python 3・Java 21（互換テスト用）。

```sh
# 1. ツールチェーンとローカル入力の診断
python3 tools/dev_check.py

# 2. クリーン公開ツリーでは手元の 1.21.1 server.jar から検証データをローカル生成（公開リポには含まれません）
bash tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/1.21.1-server.jar --fetch-fabric-mappings --accept-eula

# 3. 層違反・負債・来歴（Zero-GL / Zero-Embed）・衛生ゲート検証
bash tools/layers_check.sh && bash tools/debt_ratchet.sh && python3 tools/provenance_check.py --strict-zero-gl && python3 tests/public_hygiene_test.py

# 4. テスト＆サーバー起動
cargo test --locked -- --test-threads=1
MEGU_PORT=25699 cargo run --locked
```

> **Note**: クリーンな公開スナップショットの生成手順は `tools/export_public_branch.sh`（および [docs/development.md](docs/development.md)）を参照してください。

---

## リリースロードマップ（SemVer 方針）

マイナーバージョンを急いで上げず、**`0.1.0` のプレリリース段階を着実に積み上げて 1.21.1 実用版を仕上げてから `0.2.0` へ進む方針** です：

| マイルストーン | 主な目標 |
|---|---|
| **`v0.1.0-alpha.1`**（現行） | 1.21.1 サーバー基盤・18/18 ライブ差分一致・純 `wgpu` GPU クライアント・`megu-installer`・TUI・汎用 Mod/レシピ/静的検査基盤の初回公開 |
| **`v0.1.0-alpha.2`** | 実 1.21.1 クライアント 30 分プレイテスト由来のエッジケース修正・Create/ATM10 系（Flywheel/Ponder 依存・汎用 Mixin リフト拡充）・同一チャンク数条件でのベンチマーク整備・Docker 検証 |
| **`v0.1.0-beta.1`** | リポジトリ構造整理（`crates/`・Workspace・`src/main.rs` モジュール分割）・サーバー/クライアント物理＆NBT 共通化・インストーラ GUI 洗練・Cloudflare Pages 運用安定化 |
| **`v0.1.0`** | Minecraft 1.21.1 サバイバル＆主要 Modpack の日常利用を安定達成した初回安定版 |
| **`v0.2.0`** | Minecraft `26.3` 完全パリティ・マルチバージョン共存・高度なシェーダーパック／Flywheel 互換の拡充 |

---

## アーキテクチャ（13 クレート構成）

ルートのサーバーバイナリ（`megu`）に加え、12 の独立クレートを厳格な層契約（`tools/layers_check.sh`）で分離しています：

| クレート | 主な役割 |
|---|---|
| `core` (`megu-core`) | 共通葉クレート（外部依存 0）：`hw_adaptive_engine`・`math_opt_engine`・`diff_engine`・`crash_report`・`zip_archive`・`romaji_ime`・`tier25_guard_batch` |
| `proto` (`megu-proto`) | Protocol 767 パケット・セッション・`VCR1` 音声リレー・フレンドリレー |
| `state-store` (`megu-state-store`) | Anvil 検査・マイグレーション（`megu-world-migrate`）・増分セーブ・CAS スナップショット・監査ログ |
| `server-files` (`megu-server-files`) | サーバー初期化（`megu-init`）・管理（`megu-admin`）・単体ネイティブインストーラ（`megu-installer`） |
| `mod-offload` | Mod 演算委譲契約（`MOT1` 戦闘モーション・`PRTY` コンパニオン保管・C-ABI FFI） |
| `mod-adapter-core` | 非実行型 JAR 静的セキュリティ検査・`DualLoaderHost`・限定 `.class` VM・`megu-mod-check` |
| `create-adapter` | Create 系キネティクス・軌道遷移・オフロードブリッジ |
| `client-kernel` (`megu-kernel`) | サーバーと共通の `lighting.rs`・物理・レジストリ・補間・QoL Looking-At |
| `client-port` (`megu-client-mesh`) | メッシュ生成・3D モデルベイク・アトラス・Hi-Z・サブタイル差分キャッシュ・外部シェーダー計画 |
| `client-ui` (`megu-client-ui`) | 純 Rust UI・設定・レシピ/Ponder ブラウザ・クラッシュ確認モーダル |
| `client-app` (`megu-client`) | クライアント CLI・セッション・オーディオミキサ＆リミッター・クラッシュレポーター |
| `client-gpu` (`megu-client-gpu`) | 純 `wgpu` 実ウィンドウ GPU クライアント（Vulkan / DX12 / Metal・0 GL） |

---

## 周辺ツール・セキュリティ・プライバシー

- **OP 運用ポリシー**: `/megu policy list|get|set|reset`（フェアプレイマップ 5 項目・MineAll/CutAll 既定 `off`）
- **Mod 静的検査**: `megu-mod-check --scan-mods-dir ./mods`（JVM 不要・危険な実行/ネイティブロード/`Unsafe`/JNDI をハード隔離）
- **ローカル Mod 自動翻訳（任意）**: `python3 tools/megu_translate.py --mods-dir ./mods --out ./megu-auto-lang-ja_jp.zip`（既定 `127.0.0.1` のみ、外部 API は `--allow-remote-llm` 明示必須）
- **クラッシュ報告**: 既定 `Ask`（同意確認なしに送信しない・ホームパス/IP/トークン自動秘匿・4 KiB 上限・`Off` で完全停止）
- **組み込み最適化と既存 Mod**: 描画カリング・増分ライティング・パレット圧縮・並列チャンク処理・LOD 等のクリーンルーム実装を内蔵しています。`megu-mod-check` は重複する軽量化 Mod を事前分類しますが、第三者 Mod の公式同梱・コード移植は一切行っていません（詳細: [ux-and-builtin-optimizations.md](specs/client-fork/ux-and-builtin-optimizations.md)）。

## ドキュメント案内

- 参加ガイド・DCO: [CONTRIBUTING.md](CONTRIBUTING.md) / [開発ガイド](docs/development.md)
- 運用・配布: [運用ガイド](docs/operator-guide.md) / [リリースチェックリスト](docs/release-checklist.md) / [配布計画](docs/RELEASE_AND_DISTRIBUTION_PLAN.md)
- Mod・シェーダー作者向け: [docs/mod-author-guide.md](docs/mod-author-guide.md)
- 仕様・ロードマップ: [specs/README.md](specs/README.md) / [specs/PLAN.md](specs/PLAN.md) / [specs/ROADMAP.md](specs/ROADMAP.md)
- 設計原則・セキュリティ: [DESIGN.md](DESIGN.md) / [SECURITY.md](SECURITY.md) / [SECURITY_NOTES.md](SECURITY_NOTES.md)
- ライセンス・出所・商標: [LICENSING.md](LICENSING.md) / [THIRD_PARTY.md](THIRD_PARTY.md) / [NOTICE](NOTICE) / [TRADEMARK.md](TRADEMARK.md)

## ライセンスとクリーンルーム方針

本プロジェクトのコードは **`LGPL-3.0-or-later`**（[COPYING.LESSER](COPYING.LESSER)・LGPLv3 付随の [LICENSE](LICENSE)・[NOTICE](NOTICE)・[LICENSING.md](LICENSING.md)）で配布されます。Fabric / NeoForge 互換レイヤ経由で実行時に読み込まれる独立した外部 Mod JAR は派生物とみなさず、コピーレフトは波及しません。

すべての最適化サブシステムは標準的なコンピュータグラフィックス／システムアルゴリズムに基づく **100% クリーンルームのオリジナル Rust 実装** であり、Mojang の逆コンパイルコード・アセット・PolyForm Shield・All Rights Reserved のソースコードやシェーダーは一切コピー・同梱していません。寄稿は `LGPL-3.0-or-later` 提供と DCO（`git commit -s`）を前提とします。
