# LICENSING.md — ライセンスの選択に関する技術的整理 / Engineering notes on the licence choice

> **法的助言でも法的結論でもありません。** リポジトリの中身・依存・上流の表示から読み取れる事実と、そこから言える技術的なリスクの整理です。最終判断はオーナー (と必要なら弁護士・Pumpkin のメンテナー) が行ってください。
> **Status: Apache-2.0 は「提案」です。§5 の項目が解消するまで、Apache-2.0 / MIT を確定・公開しないでください。** 解消できない項目があれば GPL-3.0 になる可能性があります (§5)。
> 作成: 2026-10-26 (Asia/Tokyo)。対象: `chore/generate-vanilla-data-locally` (#22) の上。

## 1. 現状の棚卸し / What the repo said before this change

| 場所 | 内容 | 不整合 |
|---|---|---|
| `LICENSE` | MIT, `Copyright (c) 2026 nekomario28` | — |
| `Cargo.toml` | `license` なし | LICENSE (MIT) と食い違い (メタデータが空) |
| `deny.toml` | 許可リストは寛容ライセンスのみ。`[licenses.private] ignore = true` で root crate を除外 | root の license 未宣言を隠していた |
| `NOTICE` | 「own code is MIT」+ minecraft-data (MIT) の帰属 + Mojang 生成データ | MIT 前提の文面 |
| `THIRD_PARTY.md` | 「コード本体は MIT」+ 78 crate の表 | 同上 |
| `README.md` | 「コードはMIT」 | 同上 |
| `evidence/foundation/oss-survey/README.md` | 「mc-spine is MIT」(調査時の前提) | 履歴的記述。注記を追加 |
| SPDX ヘッダ | ソースに無い | 不要 (LICENSE + Cargo.toml の `license` で足りる。ツールが要求するなら後で追加) |
| 著作権者 | git の author は全 487 コミットが同一 (`nekomario28` / `Yuu`、同じ noreply アドレス)。他の人の貢献は履歴上なし | ただし AI コーディング支援ツールを使った形跡あり (§5 の 8) |

この変更で上記を Apache-2.0 に揃えた (LICENSE = 公式全文、`Cargo.toml` の `license = "Apache-2.0"`、NOTICE、README、THIRD_PARTY.md、deny.toml)。著作権表示は既存の LICENSE に合わせて `Copyright 2026 nekomario28` (NOTICE)。本名・組織名にしたい場合は NOTICE の1行を変えるだけ。

## 2. 互換性の分析 / Compatibility analysis

### (a) Rust 依存 (78 crate, `cargo metadata --all-features --locked`)
宣言ライセンスは全て寛容型: MIT (71), Apache-2.0 (66), ISC (5), Unlicense (4, すべて `OR MIT` つき), Zlib (2), BSD-3-Clause (1: subtle), 0BSD (1), Unicode-3.0 (1: unicode-ident, `AND`), LLVM-exception (1), CDLA-Permissive-2.0 (1: webpki-roots, CA データ)。**GPL / LGPL / AGPL / MPL / EPL は 0 件、ライセンス無しも 0 件。** よって依存木は Apache-2.0 でも MIT でも妨げない。`cargo deny check licenses bans sources` は変更前後とも OK (cargo-deny 0.20.2)。許可リストは変更なし (実在するライセンスだけ) で、コメントに「コピーレフトを足さない」を明記。
注意: (1) 配布するバイナリには MIT/ISC/BSD/Apache の著作権表示・NOTICE の同梱が必要 (`deny.toml` は検査しない)。リリースを作る前に `cargo about` 等でライセンス一覧を生成する運用を決める。(2) `ring` は `Apache-2.0 AND ISC` と宣言され、LICENSE が複数に分かれている (ISC の新規コード、BoringSSL 由来の Apache-2.0 / ISC、once_cell 由来の MIT/Apache)。同梱表記は上流の LICENSE 一式を確認する。(3) `webpki-roots` は Mozilla CA の**データ**で CDLA-Permissive-2.0。

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

使われ方 (事実): (1) jar・class・上流ソースは**一つもコミットされていない** (`git ls-files` に `*.jar`/`*.class` なし。`.cache/` `.build/` は `.gitignore`)。(2) `compat/**` の Java は mc-spine 自身のコード (約 400 行) で、`net.fabricmc.*` / `net.neoforged.*` を import してコンパイルし、実行時に JNI (`java-compat` feature の `src/java_compat.rs`) 経由で同じ JVM に読み込む。(3) `compat/*/java/net/minecraft/**` は名前だけを合わせた空の型 (1〜15 行、Minecraft のコードは含まない)。
意味 (技術的な見立て): **mc-spine が LGPL の部品を再配布しなければ、LGPL の義務 (ソース提供・差し替え可能性・LGPL 全文の同梱) は mc-spine には発生しない**。取得・組み立ては利用者の環境で起き、LGPL 側の義務は「その jar を配布する人」に付く。Apache-2.0 と MIT のどちらを選んでも、この点は同じ。Fabric (Apache-2.0) と Mixin (MIT) はどちらを選んでも問題ない。注意: (i) 将来、jar を同梱したリリース zip / Docker イメージを配布するなら、その配布物には LGPL-2.1 の義務 (全文・ソースの入手方法・利用者が jar を差し替えられること) と各 NOTICE が必要になる。(ii) Apache-2.0 は GPL-2.0-only とは互換でない (FSF の見解)。LGPL-2.1 のライブラリを「使う」側の作品は、LGPL の §6 の条件 (改変とリバースエンジニアリングを許す) を満たせば別の条件でよいとされる。Java では「リンク」の解釈に議論があるので、同梱配布をするときは弁護士の確認が望ましい。

### (c) minecraft-data (PrismarineJS, MIT)
`assets/blocks_1211.json` は上流の `data/pc/1.21.1/blocks.json` とバイト一致 (確認済み)。README に MIT と書かれているが LICENSE ファイルと著作権者名が無く、「一部は wiki.vg / minecraft.gamepedia.com 由来」との注記がある。NOTICE に帰属を記載済み (維持)。wiki 由来部分の扱い (wiki の CC BY-SA の可能性) は上流に確認が必要。取得スクリプト化すればコミットから外せる (A' に近づく)。

### (d) Mojang / Minecraft
Mojang 由来のもの (worldgen JSON・ブロック状態・biome tree・`config_packets.bin`・javap・マッピング) はコミットされておらず、利用者の jar から生成する (#22)。`--accept-eula` は利用者の EULA 同意を意味する。公式マッピング (`server.txt`) は `bytecode_probe.sh` の入力として利用者が用意する。Minecraft EULA / Usage Guidelines の現行文面と、「Mojang のデータ由来のコードを Apache-2.0 で出すこと」「`net.minecraft.*` と同名の空の型」(名前のみ・コードなし) の扱いは**この変更では確認していません** (確認項目: §5)。
**履歴には旧データ・javap・マッピングが残っている** (§5)。

### (e) 出所が未解決の項目 → §5。

### (f) 選択肢の比較

| | Apache-2.0 のみ | MIT OR Apache-2.0 | MIT | GPL-3.0 / AGPL-3.0 |
|---|---|---|---|---|
| mod 界隈での採用しやすさ | 良い (Fabric Loader 自身が Apache-2.0)。mod は別 jar として読み込まれ、mc-spine のライセンスは伝播しない | 良い。Rust の慣習で、利用者が選べる | 最も単純・広く通じる | 弱い: GPL でない / 互換でない mod 作者・利用者は組み込みにくい。AGPL は「改変版をネットワーク提供するなら公開」が効く (サーバー向きだが採用障壁は高い) |
| 特許条項 | **明示の特許許諾 + 特許訴訟時の失効** | 利用者が Apache を選べば同じ | **なし** (黙示のみ) | GPL-3.0 には特許許諾あり |
| 貢献者 | §5 で inbound=outbound が条文化。CONTRIBUTING + DCO を足せば足りる | 両方に同意する形になり、文面が増える | CLA/DCO を別に決める | コピーレフトで貢献は還元される。ただし Pumpkin (GPL-3.0) のコードを合法的に取り込めるのは GPL の時だけ |
| 依存 crate との互換 | すべて寛容型なので問題なし (MIT-only crate も取り込める、表示は必要) | 同左 | 同左 | 寛容型 crate は GPL-3.0 に取り込める (一方向) |
| NeoForge (LGPL-2.1) / Fabric (Apache-2.0) / Mixin (MIT) との関係 | 上記 (b)。同梱しなければ問題を生まない。Apache-2.0 は GPL-2.0-only と非互換 | MIT 側を選べば GPL-2.0 とも互換 | GPL-2.0 とも互換で最も柔軟 | 同梱配布なら LGPL-2.1 の条件との整理が必要 |
| 注意点 | NOTICE を同梱する必要。MIT より少し重い | LICENSE を2つ置く。`MIT OR Apache-2.0` を各所に書く | 特許が不明確。NOTICE なし | 公開済みの旧版を GPL に変えても、すでに MIT で受け取った人の権利は残る (再ライセンスは不可逆に近い) |

**推奨 (技術面)**: Apache-2.0 のみ。理由: 特許許諾、貢献条件の明確さ、Fabric Loader と同じ、依存はすべて寛容型。MIT の単純さを重視するなら `MIT OR Apache-2.0` が次点。GPL/AGPL は「Pumpkin のコードを取り込みたい」または「閉じた派生を防ぎたい」場合のみ。**ただし §5 が解消するまで確定しない。**

## 3. JVM 側の取り扱い方: 3 つの形 / Option A, A', B

- **A: 全部 mc-spine に置く、JVM 側は実行時取得のみ**。
- **A': mc-spine は自作のブリッジ/shim だけを持ち、NeoForge / Fabric / Mixin / Mojang の中身は一切再配布しない (利用者が用意・取得)**。
- **B: JVM 側のブリッジ/shim/adapter を別リポジトリに分ける**。

**現状は A' にすでに合っている** (確認した事実): jar / class / 上流ソースのコミットなし。`.cache/` `.build/` は `.gitignore` 済み (今回 `*.jar` `*.class` も追加)。`compat/**` の Java は自作で、NeoForge / Fabric のコードを写していない (import のみ)。`compat/*/java/net/minecraft/**` は名前だけの空の型で Mojang のコードは含まない。Mojang の jar・マッピング・生成データは `gen_vanilla_data.sh` で利用者が作る。
A' として残る作業: (1) 上の「再配布しない」を CONTRIBUTING / NOTICE / README に明記 (今回実施)。(2) `build.py` が Maven から自動取得するのは**利用者が build を実行したとき**なので許容範囲 (SHA-256 固定)。(3) Seed Protect jar は「テスト入力として取得するだけ・再配布しない」(ライセンス表示が jar と Modrinth で食い違う)。(4) **リリース成果物 (zip / Docker / CI artifact) に jar を入れない**。入れる場合は LGPL の義務が出るので B か同梱表記を再検討。(5) 任意: CI に「`*.jar`, `net/neoforged`, `net/fabricmc` がコミットされていない」検査を足す。

| | A | A' | B |
|---|---|---|---|
| ライセンス上の必要性 | 不要 | **不要** (再配布しない限り) | **不要** (予防策)。同梱配布をするなら意味が出る |
| 動かすもの | 変更なし | 変更なし (文書・.gitignore・NOTICE のみ) | 下記 |
| 工数 | 0 | 小 (今回の PR) | 中: 2〜4 日程度の見積り (cfg 境界は既にあるが、ブリッジは `main.rs` の世界の ABI に依存) |
| リスク | 同梱を始めると LGPL 義務 | 同梱を始めない限りなし | リポジトリが2つになり、バージョンずれと CI が増える |

**B の場合に移るもの** (実装はこの PR ではしない): `compat/fabric-probe/**` と `compat/neoforge-events/**` (34 ファイル。Java 約 400 行、`build.py` `verify.py` `server_verify.py` `seedprotect*` `dependencies.json` fixtures)、`src/java_compat.rs` (380 行)、`src/landing.rs` (247) と `src/landing_types.rs` (21) のうち Loader/Trample 部分、`main.rs` の呼び出し 2 か所 (`mod java_compat;`、`java_compat::run`)、Cargo の feature (`java-compat` `fabric-probe` `neoforge-events`) と `jni` 依存、`specs/mod-compatibility/**`。境界案: mc-spine 側に小さな trait (例: 農地の踏みつけ判定を問い合わせる `LandingHook`) と世界アクセスの C ABI を残し、別 crate (`mc-spine-jvm-bridge`、cdylib か workspace member) が実装する。別リポジトリのライセンスは自由に選べる (LGPL-2.1 互換にすれば将来の同梱が楽、Apache-2.0 / MIT でも可)。**B が必要になるのは、NeoForge / Fabric の jar や派生コードを同梱して配布すると決めたとき**。

## 4. 実施した変更 (この PR)
`LICENSE` (Apache-2.0 公式全文、sha256 `cfc7749b…c523d30`)、`NOTICE` (Apache 形式 + 既存の帰属を維持 + JVM 部品の記載)、`Cargo.toml` `license = "Apache-2.0"`、`deny.toml` (root を検査対象に、許可リストの意図を明記)、`README.md` / `THIRD_PARTY.md` の記述、`CONTRIBUTING.md`、`.gitignore` の整理。**Cargo.toml を変えたので、多数の `receipt.json` に記録された Cargo.toml の sha256 (`dada0264…`) は現在の値と一致しなくなります** (`license` 行のみの追加でビルド結果は変わらない。receipt を検証するテストはない)。

## 5. 確定・公開の前に解消が必要な項目 / Blockers
**Apache-2.0 / MIT は、次が解消するまで確定・公開しないでください。**
1. **Pumpkin (GPL-3.0) とのコード一致**: #25 (THIRD_PARTY.md §10) で残りの一致箇所を書き直し済み。再検査 (`tools/provenance_scan.py`) と、書き直しの独立確認が残る。
2. **テスト値表**: #25 で `AQ_NB` ほかの表を削除し、vanilla から生成する `vanilla_vectors_1211.tsv` に置き換え済み (THIRD_PARTY.md §10)。他に Pumpkin と一致する数値表が残っていないかの確認が残る。
3. **履歴**: 旧 Pumpkin 同一データ 3 ファイル、Pumpkin 一致コード、javap・マッピング、旧 `datapacks/vanilla/data/**` が git 履歴に残っている。公開するなら履歴を引き継がない新規リポジトリ (案 C) か、履歴の書き換え (別途承認が必要)。過去に public だった形跡 (issue #11) あり。
4. 出所不明: `datapacks/{modtest,rectest,spinetest,tagtest,testpack}`、`*.storyboard.png`、`evidence/foundation/audit-snapshots`。
5. `assets/blocks_1211.json` (MIT の表示のみ、wiki 由来の注記)。取得スクリプト化が望ましい。
6. Mojang の EULA / Usage Guidelines の確認 (§2d)、`--accept-eula` の扱い。
7. Rust 依存の表記方法 (§2a)。
8. 著作権者: git 上は単一の author だが、AI コーディング支援を使った形跡がある。AI 支援で生成された部分の著作権の扱いは法域によって未確定。オーナーが「自分が権利を持つ」と言える範囲を確認する。

**これらが解消できない場合に GPL になり得るもの**: 1、2、3 (Pumpkin の GPL-3.0 コードの複製・派生とみなされ、書き直し・許諾・削除ができない場合は、配布するなら GPL-3.0 の条件に従う、または Pumpkin のメンテナーの許諾が必要)。4〜8 は GPL にはつながらないが、Apache-2.0 / MIT を宣言する権利があるかに関わる。
