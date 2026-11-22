# LICENSING.md — ライセンスの選択に関する技術的整理 / Engineering notes on the licence choice

> **法的助言でも法的結論でもありません。** リポジトリの中身・依存・上流の表示から読み取れる事実と、そこから言える技術的なリスクの整理です。
> **Status (2026-11-22 更新): 本体の配布ライセンスは `LGPL-3.0-or-later` に統一されました（[COPYING.LESSER](COPYING.LESSER) = GNU LGPL v3.0 or later、全 16 の Cargo.toml マニフェストの `license = "LGPL-3.0-or-later"`）。LGPLv3 が要求する GPLv3 付随文として [LICENSE](LICENSE)（GNU GPL v3.0）を同梱します。**
> これにより、`MIT` / `Apache-2.0` / `BSD` / `LGPL-2.1` / `LGPL-3.0` のコード（C2ME、Lithium、NeoForge、Fabric など）と完全互換であり、LGPLv3 §2 により下流利用者は GPLv3 での利用・再配布も可能です。また、Fabric / NeoForge 互換レイヤ経由で動的ロードされる独立した外部 Mod JAR には本体のコピーレフトを波及させません。

## 1. 変更の経緯 / License evolution

| 時期 | 内容 | 備考 |
|---|---|---|
| 初期 | `LICENSE` = MIT (`Copyright (c) 2026 nekomario28`) | `Cargo.toml` の `license` フィールド未設定 |
| 2026-10-26 (#23) | `Apache-2.0` を提案として設定 | `THIRD_PARTY.md` / `LICENSING.md` §5 の来歴整理を並行実施 |
| 2026-11-19 (Stage F1) | 旧 `assets/pblocks.json`, `assets/properties.json`, `resourcepack.zip` を完全削除 (`git rm`) | `tools/provenance_check.py` で再混入を禁止、`tools/export_public_branch.sh` を整備 |
| 2026-11-20 | 本体 `GPL-3.0-or-later` ＋ 寄稿 `LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later` | `LICENSE` (GPL-3.0), `COPYING.LESSER` (LGPL-3.0), `COPYING.AGPL` (AGPL-3.0), `Cargo.toml`, `NOTICE`, `deny.toml`, `CONTRIBUTING.md`, `README.md` を統一。さらに `git filter-repo --invert-paths` により過去全履歴から旧 `pblocks.json`, `properties.json`, `resourcepack.zip`, 不要 `*.storyboard.png`, 誤コミット `.jar`/`.class`/`.pyc` を完全抹消済み |
| **2026-11-22 (現在)** | **本体・寄稿ともに `LGPL-3.0-or-later` 単一ライセンスに統一** | 全 16 `Cargo.toml` の `license` を `LGPL-3.0-or-later` に統一（`tools/offload_ffi_probe`・`tools/storage_bench` は新規追加、ルート・`tools/palette_bench` は `GPL-3.0-or-later` から、各クレートはトリプル表記から変更）。`tools/provenance_check.py` の `LICENSE_MANIFESTS` を全 16 マニフェストに拡張し、`README.md` / `LICENSING.md` / `CONTRIBUTING.md` / `NOTICE` を単一ライセンス表記に更新。`LICENSE` (GPLv3 付随文) と `COPYING.LESSER` (LGPLv3) を同梱 |

## 2. 互換性の分析 / Compatibility analysis

### (a) Rust 依存 (78 crate, `cargo metadata --all-features --locked`)
宣言ライセンスは全て寛容型: MIT (71), Apache-2.0 (66), ISC (5), Unlicense (4, すべて `OR MIT` つき), Zlib (2), BSD-3-Clause (1: subtle), 0BSD (1), Unicode-3.0 (1: unicode-ident, `AND`), LLVM-exception (1), CDLA-Permissive-2.0 (1: webpki-roots, CA データ)。**第三者クレートに GPL / LGPL / AGPL / MPL / EPL は 0 件、ライセンス無しも 0 件。** これら全ての寛容型ライセンスは `LGPL-3.0-or-later`（および LGPLv3 §2 による GPLv3 利用）と完全互換です。

### (b) JVM 側の部品 (NeoForge / Fabric / Mixin ほか)
実物を `compat/*/dependencies.json` の URL から取得し、SHA-256 が一致することを確認したうえで、jar 内のメタデータと上流の POM/LICENSE を読んだ結果:

| 部品 | 宣言ライセンス | 根拠 | mc-spine での扱い |
|---|---|---|---|
| Fabric Loader 0.19.5 | **Apache-2.0** | `fabric.mod.json`, `LICENSE_fabric-loader` (jar 内)。jar は MixinExtras 0.5.5 (MIT) を内蔵 | 実行時に取得 (`.cache/`) |
| Mixin (FabricMC sponge-mixin 0.17.4) | **MIT** | jar 内 `LICENSE.txt` | 同上 |
| ASM 9.10.1 | BSD-3-Clause | POM | 同上 |
| **NeoForge 21.1.172** | **LGPL-2.1** | jar 内 `neoforge.mods.toml` に `license="LGPL v2.1"`、POM「LGPL 2.1」、リポジトリの LICENSE.txt も LGPL-2.1 | 同上 (universal jar と sources jar) |
| NeoForged event bus 8.0.2 | **LGPL-2.1-only** | POM | 同上 |
| FancyModLoader 4.0.39 | LGPL 2.1 | POM (GitHub の自動判定は NOASSERTION) | 同上 |
| MergeTool 2.0.0 (distmarker) | LGPL 2.1 | POM / リポジトリ | 同上 |
| log4j-api 2.23.1 / JetBrains annotations 24.1.0 / typetools 0.6.3 | Apache-2.0 | POM | 同上 |
| Seed Protect 1.0.0 (Itamio, テスト用 mod) | **不一致**: jar は `LicenseRef-All-Rights-Reserved`、Modrinth ページは MIT | jar 内 `neoforge.mods.toml` / Modrinth API | テスト入力として取得するだけ。**再配布しない** |

**`LGPL-3.0-or-later` との互換性**:
- `LGPL-2.1`（および `LGPL-2.1-only`）は LGPL-2.1 §3 および §6（動的リンク利用）により `LGPL-3.0-or-later` と互換です。
- さらに `mc-spine` はこれらの JAR をリポジトリにコミット・再配布せず、ビルド／テスト実行時に利用者の環境（`.cache/`）へ取得します。

### (c) minecraft-data (PrismarineJS, MIT)
`assets/blocks_1211.json` は上流の `data/pc/1.21.1/blocks.json` とバイト一致 (MIT)。[NOTICE](NOTICE) に帰属を記載済み。

### (d) Mojang / Minecraft
- `src/brigadier.rs` は Mojang の `com.mojang:brigadier:1.3.10` (**MIT License**, Copyright Microsoft Corporation) の Rust 移植であり、[NOTICE](NOTICE) に MIT 全文を収録しています。
- 開発ブランチ（`main` / `perf/worldgen-parallel`）は共同開発およびテスト高速化のため、1.21.1 `server.jar` 由来の datapack、状態表 (`assets/block_states_1211.json`)、テスト TSV、`config_packets.bin` を追跡しています。
- 外部一般公開・配布用のクリーンな単一ルートコミット（Mojang 由来生成物を一切含まないツリー）を作る場合は、`tools/export_public_branch.sh --out ../mc-spine-public --verify` を実行します。

## 3. 単一 `LGPL-3.0-or-later` ライセンスの設計

1. **本体・寄稿ともに `LGPL-3.0-or-later` に統一（[COPYING.LESSER](COPYING.LESSER) / [CONTRIBUTING.md](CONTRIBUTING.md)）**:
   - 全ワークスペースの `Cargo.toml` は `license = "LGPL-3.0-or-later"` を宣言し、`tools/provenance_check.py` が全 16 マニフェストの `license` 宣言を常時検査します。
   - LGPLv3 が要求する GPLv3 付随文として [LICENSE](LICENSE) を同梱します。
   - LGPLv3 §2 は、利用者が本ソフトウェアを GPLv3 の条件で利用・頒布することを認めており、追加の許諾なしに下流での GPLv3 利用が可能です。`or later` により将来の LGPL バージョンへの移行も可能です。
2. **C2ME (`MIT`)・Lithium (`LGPL-3.0`)・NeoForge (`LGPL-2.1`)・Fabric (`Apache-2.0`) との完全互換と隔離原則**:
   - **C2ME (`MIT`)** や **Lithium (`LGPL-3.0`)**、**NeoForge (`LGPL-2.1`)**、**Fabric (`Apache-2.0`)** は、`LGPL-3.0-or-later` と**完全互換**です。
   - 一方、外部の第三者が書いた `GPL-3.0-only` や `AGPL-3.0-only` のコードをファーストパーティのソースファイルに直接コピーすると、そのファイルは `LGPL-3.0-or-later` 単独では頒布できなくなります。そのため、ファーストパーティのファイルには `MIT` / `Apache-2.0` / `BSD` / `LGPL` 互換のコードのみを入れるか、万一外部 GPL/AGPL 専用コードを組み合わせる場合は専用モジュール／オプショナルクレートに分離します。
3. **外部 Mod JAR への非波及**:
   - `compat/`（Fabric / NeoForge 互換レイヤ）やプラグイン機構を通じて実行時にロードされる独立した 3rd-party Mod JAR は `mc-spine` の派生物とはみなさず、Mod 作者自身のライセンス（ARR / MIT / LGPL 等）のまま動作します。動的ロードであるため、外部 Mod JAR に mc-spine のコピーレフトは波及しません。

## 4. 確認・解消済み項目と Git 履歴クリーンアップ済みの記録 (2026-11-20)

1. **旧不純ファイル・バイナリの全 Git 履歴からの完全抹消（`git filter-repo --invert-paths` 完了）**:
   - `assets/pblocks.json`, `assets/properties.json`, `resourcepack.zip`, ルート直下の不要 `*.storyboard.png` 6 枚、および過去に誤コミットされていた `tools/carve-agent` の `.jar`/`.class`・`__pycache__/*.pyc` を、`git filter-repo --invert-paths` により **全 1,080 コミットの過去履歴から完全に抹消**しました（`tools/provenance_check.py` で再混入も常時ブロック）。
   - 単一コミットへの Squash ではなくパス指定の履歴フィルタを用いたため、**誰がどのコミット・どの行を担当したか（`git log` / `git blame`）は 100% そのまま保持**されています。
2. **コード一致とテスト値表の置換**: `THIRD_PARTY.md` §8・§10 の通り書き換えおよび 1.21.1 jar 生成ベクタへの置換完了。
3. **自作ファイルの確定**:
   - `datapacks/{modtest,rectest,spinetest,tagtest,testpack}` および `evidence/foundation/audit-snapshots` はオーナー自作（first-party）と確認済み。

## 5. クリーンルーム実装と先行技術（Prior Art）の整理 / Clean-room implementation & prior art

### (a) 最適化サブシステムの来歴 / Provenance of the optimization subsystems

本リポジトリの最適化サブシステムはすべて、標準的なコンピュータグラフィックス／システム系アルゴリズムに基づく **100% クリーンルームのオリジナル Rust 実装**です:

| サブシステム | アルゴリズム | 実装箇所 |
|---|---|---|
| セクション可視性 | 6×6 視界グラフ到達性＋方向単調 BFS の**グラフオクルージョンカリング**、6 平面フラスタムカリング | `client-port/src/visibility.rs` / `culler.rs` / `cull.rs` |
| 面振り分け | **方向別フェースバケッティング**（7方向バケットのバックフェースカリング） | `client-port/src/mesh.rs` |
| 描画バッチ | **マルチドローレンダリージョン**（8×4×8 セクション共有頂点/インデックスアリーナ） | `client-port/src/region.rs` |
| 遠景 | **階層的 LOD**（粗カラムサマリ） | `client-port/src/lod.rs` / `client-app/src/lod.rs` |
| 生成並列化 | **依存 DAG 並列スケジューリング**（`populate_parallel`、checkerboard feature バッチ） | `src/feature.rs` / `src/worldgen.rs` |
| 照明 | **インクリメンタルライティング**（`IncrementalLighting` / `LiveLightEngine`） | `src/lighting.rs` / `src/main.rs` |
| チャンク格納 | **パレット化ストレージ**（0-bit uniform / 4–8-bit indirect / 15-bit direct） | `src/paletted.rs` |

これらのアルゴリズム概念はグラフィックス／システム分野の標準的な手法であり、特定の第三者 Mod のソースを読まずに、vanilla 1.21.1 の観測可能な契約と独自設計から実装しています。

### (b) 非互換ソースの排除 / Exclusion of incompatible sources

**PolyForm Shield**（例: Sodium 0.6+）、**All Rights Reserved / ARR**（例: OptiFine）、その他いかなる第三者 Mod からも、**ソースコード・シェーダー・アセットを一切コピー・移植していません**。PolyForm Shield 1.0.0 は source-available かつ非 OSI で GPL/LGPL/AGPL 系と互換ではなく、ARR はそもそも利用許諾を与えないため、これらの実装を読んで挙動を複製することも禁止しています。挙動の一致は vanilla の観測可能な意味論と wire/parity テストでのみ確立します。

### (c) 互換境界 / Compatibility boundary

| 区分 | ライセンス例 | 扱い |
|---|---|---|
| **互換（参照・帰属つき利用可）** | `MIT` / `Apache-2.0` / `BSD` / `ISC` / `Zlib` / `0BSD` / `MPL-2.0` / `LGPL-2.1` / `LGPL-3.0-or-later` | すべて `LGPL-3.0-or-later` と互換（LGPLv3 §2 により下流は GPLv3 でも利用可）。帰属は [THIRD_PARTY.md](THIRD_PARTY.md) / [NOTICE](NOTICE) に記載 |
| **概念参照のみ（コード不可）** | 第三者の `GPL-3.0-only` / `AGPL-3.0-only` | ファーストパーティファイルへ直接コピーしない（そのファイルが `LGPL-3.0-or-later` 単独で頒布できなくなるため）。必要なら専用モジュール／オプショナルクレートへ分離 |
| **常時除外（参照自体を禁止）** | `PolyForm Shield`（Sodium 0.6+ 等）、`ARR`（OptiFine 等） | ソースを読まない・コピーしない・移植しない |

なお、歴史的経緯で `specs/client-fork/sodium-culling-architecture.md` というファイル名が残っていますが、これは既存リンク維持のためだけの名前であり、記述されているパイプラインは上記 (a) のクリーンルーム設計です。
