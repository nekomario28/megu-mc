# LICENSING.md — ライセンスの選択に関する技術的整理 / Engineering notes on the licence choice

> **法的助言でも法的結論でもありません。** リポジトリの中身・依存・上流の表示から読み取れる事実と、そこから言える技術的なリスクの整理です。
> **Status (2026-11-20 更新): 本体のライセンスは `GPL-3.0-or-later OR LGPL-3.0-or-later`（[LICENSE](LICENSE) = GNU GPL v3.0 or later、[COPYING.LESSER](COPYING.LESSER) = GNU LGPL v3.0 or later）を採用しています。**
> これにより、通常は `GPL-3.0-or-later`（過去の Pumpkin GPL-3.0 由来リスクの完全解消・GPL/LGPL エコシステムとの互換性確保）として運用しつつ、すべての寄稿（Inbound = Outbound: `GPL-3.0-or-later OR LGPL-3.0-or-later`）について将来いつでも追加の同意取得なしに `LGPL-3.0-or-later` 単体へ切り替え・利用可能です。また、Fabric / NeoForge 互換レイヤ経由で動的ロードされる独立した外部 Mod JAR には本体のコピーレフトを波及させません。

## 1. 変更の経緯 / License evolution

| 時期 | 内容 | 備考 |
|---|---|---|
| 初期 | `LICENSE` = MIT (`Copyright (c) 2026 nekomario28`) | `Cargo.toml` の `license` フィールド未設定 |
| 2026-10-26 (#23) | `Apache-2.0` を提案として設定 | `THIRD_PARTY.md` / `LICENSING.md` §5 の来歴整理を並行実施 |
| 2026-11-19 (Stage F1) | 旧 `assets/pblocks.json`, `assets/properties.json`, `resourcepack.zip` を完全削除 (`git rm`) | `tools/provenance_check.py` で再混入を禁止、`tools/export_public_branch.sh` を整備 |
| **2026-11-20 (現在)** | **`GPL-3.0-or-later OR LGPL-3.0-or-later`** に決定 | `LICENSE` (GPL-3.0), `COPYING.LESSER` (LGPL-3.0), `Cargo.toml`, `NOTICE`, `deny.toml`, `CONTRIBUTING.md`, `README.md` を統一。`datapacks/{modtest,rectest,spinetest,tagtest,testpack}` の自作確認完了、不要な直下 `*.storyboard.png` 6枚を削除 |

## 2. 互換性の分析 / Compatibility analysis

### (a) Rust 依存 (78 crate, `cargo metadata --all-features --locked`)
宣言ライセンスは全て寛容型: MIT (71), Apache-2.0 (66), ISC (5), Unlicense (4, すべて `OR MIT` つき), Zlib (2), BSD-3-Clause (1: subtle), 0BSD (1), Unicode-3.0 (1: unicode-ident, `AND`), LLVM-exception (1), CDLA-Permissive-2.0 (1: webpki-roots, CA データ)。**第三者クレートに GPL / LGPL / AGPL / MPL / EPL は 0 件、ライセンス無しも 0 件。** これら全ての寛容型ライセンスは `GPL-3.0-or-later` および `LGPL-3.0-or-later` の双方と完全互換です。

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

**`GPL-3.0-or-later OR LGPL-3.0-or-later` との互換性**:
- `LGPL-2.1`（および `LGPL-2.1-only`）は LGPL-2.1 §3 および §6 により `GPL-3.0-or-later` / `LGPL-3.0-or-later` の双方と完全互換です。
- さらに `mc-spine` はこれらの JAR をリポジトリにコミット・再配布せず、ビルド／テスト実行時に利用者の環境（`.cache/`）へ取得します。

### (c) minecraft-data (PrismarineJS, MIT)
`assets/blocks_1211.json` は上流の `data/pc/1.21.1/blocks.json` とバイト一致 (MIT)。[NOTICE](NOTICE) に帰属を記載済み。

### (d) Mojang / Minecraft
- `src/brigadier.rs` は Mojang の `com.mojang:brigadier:1.3.10` (**MIT License**, Copyright Microsoft Corporation) の Rust 移植であり、[NOTICE](NOTICE) に MIT 全文を収録しています。
- 開発ブランチ（`main` / `perf/worldgen-parallel`）は共同開発およびテスト高速化のため、1.21.1 `server.jar` 由来の datapack、状態表 (`assets/block_states_1211.json`)、テスト TSV、`config_packets.bin` を追跡しています。
- 外部一般公開・配布用のクリーンな単一ルートコミット（Mojang 由来生成物と過去の履歴を一切含まないツリー）を作る場合は、`tools/export_public_branch.sh --out ../mc-spine-public --verify` を実行します。

## 3. `GPL-3.0-or-later` 本体 + 寄稿 `LGPL-3.0-or-later / GPL-3.0-or-later` デュアルライセンスの設計

1. **本体は `GPL-3.0-or-later`（[LICENSE](LICENSE) / `Cargo.toml`）**:
   - 過去の Pumpkin (`GPL-3.0`) 由来リスクを法的に 100% 解消し、クローズドな商用フォークによる持ち逃げも防ぎます。
2. **寄稿コードは `LGPL-3.0-or-later` および `GPL-3.0-or-later` のデュアルライセンス（[CONTRIBUTING.md](CONTRIBUTING.md)）**:
   - Collaborator からの寄稿を両ライセンスで受けておくことで、将来いつでも追加同意なしに本体またはライブラリ部分を `LGPL-3.0-or-later`（[COPYING.LESSER](COPYING.LESSER)）に切り替えられます。
3. **C2ME (`MIT`)・Lithium (`LGPL-3.0`)・NeoForge (`LGPL-2.1`) との完全互換**:
   - **C2ME (`MIT`)** や **Lithium (`LGPL-3.0`)**、**NeoForge (`LGPL-2.1`)**、**Fabric (`Apache-2.0`)** は、`GPL-3.0-or-later` と `LGPL-3.0-or-later` の**両方と完全互換**です。
   - したがって、C2ME (`MIT`) や Lithium (`LGPL-3.0`) の手法・コードを取り込んだ場合でも（[THIRD_PARTY.md](THIRD_PARTY.md) と [NOTICE](NOTICE) に帰属とライセンスを明記すれば）、`GPL-3.0-or-later` としても `LGPL-3.0-or-later` としても合法的に配布できます。
4. **外部 Mod JAR への非波及**:
   - `compat/`（Fabric / NeoForge 互換レイヤ）やプラグイン機構を通じて実行時にロードされる独立した 3rd-party Mod JAR は `mc-spine` の派生物とはみなさず、Mod 作者自身のライセンス（ARR / MIT / LGPL 等）のまま動作します。

## 4. 確認・解消済み項目と履歴維持の方針 / Resolved items & history preservation (2026-11-20)

1. **旧 Pumpkin 同値ファイルの削除と再混入防止**: `assets/pblocks.json`, `assets/properties.json`, `resourcepack.zip` を `git rm` で削除し、`tools/provenance_check.py` で常時ゲート化済み。
2. **コード一致とテスト値表の置換**: `THIRD_PARTY.md` §8・§10 の通り書き換えおよび 1.21.1 jar 生成ベクタへの置換完了。
3. **自作ファイルの確定と不要ファイルの削除**:
   - `datapacks/{modtest,rectest,spinetest,tagtest,testpack}` および `evidence/foundation/audit-snapshots` はオーナー自作（first-party）と確認済み。
   - リポジトリ直下の不要な `*.storyboard.png` 6 ファイルは削除済み。
4. **誰がどのコミットを担当したか（`git log` / `git blame`）を消さない運用**:
   - `tools/export_public_branch.sh`（単一ルートコミットへの潰し込み）は、ローカルでの `--strict-public` 監査や「履歴なしの配布スナップショット」を作りたいときだけ別ディレクトリに対して使います。**メインリポジトリ（`main`）に対して単一コミットで Force Push することはしません**（全員のコミット履歴・担当範囲・`git blame` が消えてしまうため）。
   - もし将来、コミット履歴（誰がどこを書いたか）を 100% 残したまま過去の特定ファイル（旧 `pblocks.json` や `datapacks/vanilla/` 等）だけを Git 履歴から除去したくなった場合は、`git filter-repo --invert-paths --path assets/pblocks.json --path assets/properties.json --path resourcepack.zip ...` を使うことで、**全コミットの Author・メッセージ・担当行（`git blame`）を一切失わずに該当ファイルだけを履歴から消去**できます。
