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

## 実装されている構成（Alpha）

ルートのサーバーパッケージとは別に、以下の **11 standalone crates** を管理しています。`tests/verify.sh` は各 crate の `cargo test --offline --locked` を実行します。Clippy の警告ゼロは別途リリース対象の各 crate で確認してください。

| crate | 主な役割 |
|---|---|
| `core` | クラッシュの指紋・機密情報の伏せ字化 |
| `proto` | セッション、プロトコル、招待・音声リレー |
| `state-store` | ワールド保存、Anvil 検査、監査ログ |
| `server-files` | サーバー初期化、パケットキャッシュ |
| `mod-offload` | Mod 処理の分離・委譲契約 |
| `mod-adapter-core` | Mod の静的検査、限定 `.class` VM、互換アダプター |
| `create-adapter` | Create 系の動力・線路モデル |
| `client-port` | メッシュ、3D モデルベイク、テクスチャアトラス、Hi-Z |
| `client-kernel` | クライアント側レジストリ、エンティティ補間 |
| `client-app` | クライアント CLI、描画パイプライン、フレーム制御、音量制限 |
| `client-ui` | 純 Rust の UI 状態・レイアウト・描画リスト・ソフトウェア描画 |

現時点で利用できる実装面は次の通りです。

- OP レベル 4 の `/megu policy list|get|set|reset` が、マップの **5 項目**（`minimap_mode`、`allow_cave_map`、`allow_mob_radar`、`player_radar_scope`、`allow_waypoint_teleport`）を制御します。`player_radar_scope=party_only` を選べます。MineAll/CutAll は初期値 `off` の opt-in です。
- 死亡時の元スロット復帰は opt-in のピックアップ処理とテストが存在します。通常の運用コマンドから有効化する配線は未完成です。
- `mod-adapter-core` に制限付き `.class` バイトコード検証・実行 VM があります。侵襲的な Java Mod は Tier 3 の境界で判定し、必要な場合のみ `tools/build_mini_jre.py` で手元の JDK から小型の委譲用 JRE を作れます。任意の Mod の動作は保証しません。
- クライアント側に 3D モデルベイカー、アトラス、Hi-Z、WGSL シェーダー、RenderGraph、純 Rust `client-ui` とヘッドレスのソフトウェアラスタライザがあります。FPS 上限は `10..=2000` または `Unlimited`、`FrameBudgetPacer` はフレームごとの処理量を制御します。音量の上限を制御するリミッターも実装されています。
- Linux / macOS / Windows を対象とした移植可能な構成です。実ウィンドウを持つ `winit` + `wgpu` バックエンドは Alpha 後の作業、実際の第三者 Modpack による E2E 検証は Wave 3 の対象です。各 OS での実機検証状況は [リリースチェックリスト](docs/release-checklist.md) に従って確認します。
- Cloudflare Pages Functions に `POST /api/report`（最大 4 KiB、機密文字列を伏せ字化する GitHub Issue リレー）と `GET /api/latest` があります。運用には設定と秘密情報の登録が必要です（[運用ガイド](docs/operator-guide.md)）。

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

設定ファイルは任意です。初回起動時にカレントディレクトリへ短い `server.toml` を書き出します（`MC_SPINE_CONFIG` で場所を変更可）。`[server]` の各キーは既存の `MC_SPINE_*` 環境変数に対応し、環境変数が常に優先されます。`[anticheat]` は on/off の 4 項目です: `enabled`（移動・落下・設置チェックの on/off）、`antixray`（埋まったバニラ鉱石＋`#c:ores`/`#forge:ores` 等の Mod 鉱石・チェスト・埋没 BlockEntity NBT を隠す）、`antifullbright`（密閉された地下空洞の光を送らない。クライアントのガンマ/フルブライト自体はサーバーからは止められません）、`check_ops`（`true` で OP も検査対象）。しきい値はプレイヤーの属性・エフェクト・エンチャント・Mod ブロック輸送からサーバー側で毎回自動計算し（`src/anticheat.rs`）、違反は引き戻し・取り消しのみでキックしません。クリエイティブ・スペクテイターは常に対象外で、サバイバル復帰時は短い猶予が付きます。非バニラコマンドは追加せず、既存のバニラ `/reload` で `server.toml` と Mod タグも自然に再読込されます。環境変数 `MC_SPINE_ANTICHEAT=off` でも無効化できます。

## 導入・周辺ツール（Alpha）

- **1 コマンド導入**: `python3 tools/megu_start.py --dir ./server [--modpack pack.mrpack] [--accept-eula]`（`[]` 内は任意）。ローカルサーバーの初期設定と起動を補助します。`--accept-eula` は EULA に同意した場合のみ指定してください。`--modpack` は事前検査用で、Mod の自動ダウンロード・導入はしません。
- **任意のローカル Mod 翻訳**: `python3 tools/megu_translate.py --mods-dir ./mods --out ./megu-auto-lang-ja_jp.zip`。欠けている日本語訳から、ローカルで個人利用するためのクライアント用リソースパックをオンデマンド生成します。ローカルの翻訳 API が必要で、既定の接続先は `127.0.0.1` のループバックのみです。外部 API への接続は `--allow-remote-llm` による明示的な許可が必要です。
- **JVM 不要の Mod 検査**: `mc-spine-mod-check --scan-mods-dir ./mods` で `mods/` 内の JAR を静的検査し、互換性経路を分類します。任意の Mod が動作する保証ではありません。
- **ワールド操作**: `mc-spine-world-migrate` は `inspect-anvil`、`migrate-level`、`audit-query`、`rollback`、`pregen-plan` を提供します。`pregen-plan` は事前生成の計画のみを行います。
- **クライアント CLI**: `mc-spine-client` は `--ping <host:port>`、`--join-code <code> --relay <host:port>`、`--gpu-backend classic-low-spec`、`--background-fps <0..240>` を提供します。
- **Mod 統合の検証**: `cargo check --offline --locked --features mod-suite` で `mod-suite` feature を有効にしたビルドを検査できます。

## 軽量化 Mod について（プレイヤー・運用者向け）

- 描画カリング・ライティング・パレット圧縮・並列チャンク処理・遠景 LOD など、既存の軽量化 Mod と同じ分野の Rust 実装があります。`Sodium` / `Embeddium` / `Lithium` / `Starlight` / `FerriteCore` / `ModernFix` / `EntityCulling` / `Distant Horizons` と機能・性能が同等であることは検証されていません。
- `mc-spine-mod-check` の事前検査では、組み込みの置換対象として登録された軽量化 Mod の ID をスキップ対象に分類します。それ以外のコンテンツ Mod の動作可否は個別の互換性検査・実行検証に依存します（対象一覧と仕組み: [ux-and-builtin-optimizations.md](specs/client-fork/ux-and-builtin-optimizations.md)）。
- スキップ対象の分類は事前検査の挙動です。上記 Mod 群の公式同梱・提携・コード移植は行っていません（詳細は下の「クリーンルーム実装と設計上の先行技術」を参照）。

## どこから読むか

- 参加する・並列作業の担当境界: [CONTRIBUTING.md](CONTRIBUTING.md) → [開発ガイド](docs/development.md)
- Mod・シェーダーの作者向け: [Mod・シェーダー開発者ガイド](docs/mod-author-guide.md)
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

## クリーンルーム実装と設計上の先行技術（Prior Art）

本リポジトリの最適化サブシステムはすべて、標準的なコンピュータグラフィックス／システム系アルゴリズムに基づく **100% クリーンルームのオリジナル Rust 実装**です。該当するサブシステム:

- **フラスタムカリング＋グラフオクルージョンカリング**（セクション視界グラフと方向単調 BFS — `client-port/src/cull.rs` / `visibility.rs` / `culler.rs`）
- **方向別フェースバケッティング**（7方向バケットのバックフェースカリング — `client-port/src/mesh.rs`）
- **マルチドローレンダリージョン**（8×4×8 セクション共有アリーナ — `client-port/src/region.rs`）
- **階層的 LOD**（粗カラムサマリ — `client-port/src/lod.rs`）
- **依存 DAG 並列スケジューリング**（`populate_parallel`）、**インクリメンタルライティング**（`src/lighting.rs`）、**パレット化ストレージ**（`src/paletted.rs`）

**PolyForm Shield**（例: Sodium 0.6+）や **All Rights Reserved**（例: OptiFine）、その他いかなる第三者 Mod からも、**ソースコード・シェーダー・アセットを一切コピー・移植していません**。`MIT` / `Apache-2.0` / `BSD` / `MPL-2.0` / `LGPL-3.0-or-later` 系のコードとはライセンス互換ですが、PolyForm Shield や ARR のコードは GPL/LGPL/AGPL 系と互換でないため、参照・コピー・移植の対象から常時除外しています。歴史的経緯で `specs/client-fork/sodium-culling-architecture.md` というファイル名が残っていますが、リンク維持のための名前であり、中身は上記のクリーンルーム設計です。詳細な境界は [LICENSING.md](LICENSING.md) を参照してください。
