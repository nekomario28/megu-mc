# THIRD_PARTY.md — 第三者由来物の出所一覧 / Third-party provenance inventory

> **これは事実の一覧です。法的な判断・結論ではありません。** 各ファイルを再配布してよいか、どのライセンスが適用されるかは、このドキュメントでは判断していません。
> **This is an inventory of facts only. It is not a legal opinion** and does not assert the license or redistribution status of any file. See §1 for development versus export contents and LICENSING.md §5 for release blockers.

- 対象リビジョン / Revision: `perf/worldgen-parallel` / `main` (2026-11-20)
- リポジトリのコード本体は `GPL-3.0-or-later OR LGPL-3.0-or-later` ([LICENSE](LICENSE), [COPYING.LESSER](COPYING.LESSER), [LICENSING.md](LICENSING.md)) です。以下の表は、**リポジトリ内にコミットされているがオリジナルのコードではないもの**(または過去に出所確認を行った資産)の一覧です。
- 作成日 / Written: 2026-10-25 (Asia/Tokyo), 更新: 2026-11-20

**出所の列の意味 / Origin column**

| 値 | 意味 |
|---|---|
| `documented` | リポジトリ内の文書・スクリプトが出所を示している（またはオーナーが自作と確認済み） |
| `undocumented` | リポジトリ内に出所を示す文書が見つからない |
| `unknown - owner to confirm` | 状況証拠はあるが、確認できていない。オーナーの確認が必要 |

## 1. コミット済みの非オリジナル資産・確認済み資産 / Committed assets & provenance inventory

> 開発ブランチはテスト用の生成データを一部追跡します。以下は開発ツリーの一覧です。
> `tools/export_public_branch.sh --out ../megu-public --verify` は §5 の生成物を除外して ignore し、開発履歴を継がない別リポジトリを作ります。
> `tools/provenance_check.py --strict-public` は D5 の対象パスが未追跡かつ ignore 済みかを検査します。

| パス / Path | 内容・規模 / What & size | README の記述 | 出所 | 根拠・観察 (facts) |
|---|---|---|---|---|
| `datapacks/vanilla/` | worldgen JSON・tag・構造テンプレート・biome tree | Mojang 由来 | `documented` | 開発用に追跡。`tools/vanilla_data/gen_vanilla_data.sh --verify` で目録を照合する (§5)。 |
| `datapacks/{modtest,rectest,spinetest,tagtest,testpack}/` | テスト用の小さな datapack (計 約25 files) | 自作テストフィクスチャ | `documented` (first-party) | オーナー (`nekomario28`) により自作（first-party）の最小テスト用 datapack であることを確認済み (2026-11-20)。 |
| `assets/blocks_1211.json` | ブロック物性 (1060 entries, 765,435 bytes, sha256 `7d1d95c4…c654`, `id/name/displayName/hardness/resistance/stackSize/diggable/material/...`) | Mojang 由来 | `documented` (下記) | **PrismarineJS/minecraft-data の `data/pc/1.21.1/blocks.json` とバイト単位で同一** (2026-10-25 に GitHub API で取得して `cmp` で確認。最終変更 commit `d56346a4e32d`, 2024-10-26)。minecraft-data は MIT (同リポジトリの README「License: MIT」。リポジトリ直下に LICENSE ファイルは無く、著作権者名の記載は見つからなかったため、ここでは作成していない)。README は「一部のデータは wiki.vg / minecraft.gamepedia.com から抽出」とも記載。帰属表示は [NOTICE](NOTICE) と README を参照。 |
| `assets/block_states_1211.json` | 状態ごとの luminance / light_block / flags / full_sides と衝突形状 (dedup した `shapes` + 状態ごとの `collision_shapes`) | Mojang 由来 (jar の事実値) | `documented` | `tools/vanilla_data/gen_vanilla_data.sh` が 1.21.1 jar から生成し、`jar_sha1` を自分のフィールドに持つ (照合は `tools/provenance_check.py`)。**現行ブランチでは追跡**しており、`git ls-files` で確認できる。公開候補では除外。 |
| `tests/data/biome_probe_1211.tsv`, `tests/data/vanilla_vectors_1211.tsv` | biome 検索結果・照合値 | 事実値 | `documented` | 生成対象だが開発 HEAD では未追跡かつ ignore されていない。 |
| `tests/data/biome_probe_ties_1211.tsv` | 開発用に追跡。leaf の range 端から作った同距離 tie の多い biome 検索結果 2,500 点 (seed 固定) | 事実値 | `documented` | 同じツールで再生成。tie をゲームと同じ子の走査順で解くことを `worldgen.rs` のテストが全点で検証する。 |
| `tests/data/random_next_int_1211.tsv` | 開発用に追跡。ゲームの乱数クラスの `nextInt(bound)` 256 回分のハッシュ 1,116 行 (数値のみ) | 事実値 | `documented` | 同じツールで再生成。`generation_math.rs` のテストが全行を検証する。 |
| `resourcepack.zip` (旧: 345 bytes, `pack.mcmeta` + 72 B の `stone.png`) | **索引から削除 (`git rm`)**。サーバーは `resourcepack.zip` が無ければリソースパックを配信しない (任意機能) ので、再生成しない。ローカルで使う場合だけ利用者が作業ディレクトリに置く | — | 削除済み | 出所不明 (Mojang のテクスチャかは未確認) だったため。`tools/provenance_check.py` が「追跡または `src/`・`tests/` から参照されたら失敗」する。`src/main.rs` の任意機能の読み取り 2 か所だけを宣言付きで許している (§1 追記)。 |
| `config_packets.bin` (旧: 105,095 bytes) | 開発用に追跡 (§9) | Mojang 由来 | 生成手順あり: `tools/vanilla_data/capture_config_packets.py` | 手元の 1.21.1 jar から実サーバーを一時起動して Configuration フェーズを記録する (`--accept-eula` 必須)。旧ファイルとは tag パケットの registry の並びだけが異なる (同サイズ、registry data は先頭から一致)。 |
| `evidence/gameplay/**/*.javap.txt` (旧 6 files) | **削除** (§9) | — | 削除済み | Mojang クラスの逆アセンブルは再配布しない。`tools/bytecode_probe.sh <class>` が手元の jar と公式マッピングから同じ出力を作る。 |
| `evidence/gameplay/**/*.mapping.txt`, `evidence/gameplay/light-update/mappings.txt` (旧 3 files) | **削除** (§9) | — | 削除済み | Mojang 公式マッピングの抜粋は再配布しない。`tools/bytecode_probe.sh` が利用者の手元のマッピングを読む。 |
| `evidence/foundation/audit-snapshots/main.*.rs` (4 files) | 過去の自作 `src/main.rs` のスナップショット | (第三者物ではない) | `documented` (first-party) | 同ディレクトリの README が由来と sha256 を記載。第三者由来ではなく、このリポジトリの自作コードの旧版です。`AUDIT_WIRE.md` / `SECURITY_NOTES.md` から参照されています (削除していません)。 |
| `src/brigadier.rs` (Rust 移植) | Mojang の Brigadier 1.3.10 (MIT) のコア (StringReader / コマンド木 / dispatcher / context / suggestion / 基本引数型) を Rust に翻訳したもの。**Java のソースは同梱していない** | 記述なし (issue #51 slice 1 で追加) | `documented` (下記 §11) | 上流 `https://github.com/Mojang/brigadier` の commit `b5419b186e4882290a188d11d6e661d4f56bd8fd` (2024-08-07) を参照。MIT の著作権表示は上流 LICENSE のまま「Copyright (c) Microsoft Corporation」。NOTICE に MIT 全文と帰属を記載。参照クローンはリポジトリ外に置き、リポジトリには含めない。 |
| `*.storyboard.png` (リポジトリ直下 6 files) | 旧図 (`auth`, `deps`, `deps_target`, `escape`, `machine`, `schema`) | — | 削除済み (`git rm`) | 不要につきリポジトリ直下の 6 枚を削除 (2026-11-20)。`evidence/gameplay/protocol-lifecycle/spine_proto.storyboard.png` は自作のプロトコル状態遷移図 (`documented`, first-party)。 |

## 2. Rust 依存クレートのライセンス / Rust dependency licenses

`cargo metadata --format-version 1 --locked --all-features` (Cargo.lock, 2026-10-25) の `license` フィールドをそのまま転記したもの。`cargo-deny` (`deny.toml`) が CI で同じ集合を検査します。`megu` 自体は `Cargo.toml` で `license = "GPL-3.0-or-later OR LGPL-3.0-or-later"` を宣言しています (`LICENSE` / `COPYING.LESSER` と一致)。依存クレート 78 個に GPL / LGPL / AGPL / MPL のものはありません (LICENSING.md §2a)。
Copied verbatim from the `license` field of each crate's published metadata; this is what the crates declare, not an independent verification of their license texts. 78 crates (optional JNI adapters included).

| crate | version | license (as declared) | 種別 |
|---|---|---|---|
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | transitive |
| base64 | 0.23.1 | MIT OR Apache-2.0 | transitive |
| bytes | 1.12.1 | MIT | transitive |
| cc | 1.5.1 | MIT OR Apache-2.0 | transitive |
| cesu8 | 1.1.0 | Apache-2.0/MIT | transitive |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 | transitive |
| combine | 4.6.8 | MIT | transitive |
| crc32fast | 1.5.2 | MIT OR Apache-2.0 | transitive |
| errno | 0.3.14 | MIT OR Apache-2.0 | transitive |
| find-msvc-tools | 0.1.14 | MIT OR Apache-2.0 | transitive |
| flate2 | 1.1.10 | MIT OR Apache-2.0 | transitive |
| getrandom | 0.2.17 | MIT OR Apache-2.0 | transitive |
| glob | 0.3.4 | MIT OR Apache-2.0 | transitive |
| http | 1.5.0 | MIT OR Apache-2.0 | transitive |
| httparse | 1.10.1 | MIT OR Apache-2.0 | transitive |
| itoa | 1.0.18 | MIT OR Apache-2.0 | transitive |
| java-locator | 0.1.9 | MIT/Apache-2.0 | transitive |
| jni | 0.21.1 | MIT/Apache-2.0 | direct (optional) |
| jni-sys | 0.3.1 | MIT OR Apache-2.0 | transitive |
| jni-sys | 0.4.1 | MIT OR Apache-2.0 | transitive |
| jni-sys-macros | 0.4.1 | MIT OR Apache-2.0 | transitive |
| libc | 0.2.189 | MIT OR Apache-2.0 | transitive |
| libloading | 0.7.4 | ISC | transitive |
| log | 0.4.34 | MIT OR Apache-2.0 | transitive |
| memchr | 2.8.3 | Unlicense OR MIT | transitive |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 | transitive |
| once_cell | 1.21.4 | MIT OR Apache-2.0 | transitive |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 | transitive |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | transitive |
| quote | 1.0.47 | MIT OR Apache-2.0 | transitive |
| ring | 0.17.14 | Apache-2.0 AND ISC | transitive |
| rustls | 0.23.46 | Apache-2.0 OR ISC OR MIT | transitive |
| rustls-pki-types | 1.15.1 | MIT OR Apache-2.0 | transitive |
| rustls-webpki | 0.103.16 | ISC | transitive |
| same-file | 1.0.6 | Unlicense/MIT | transitive |
| shlex | 2.0.1 | MIT OR Apache-2.0 | transitive |
| signal-hook | 0.4.4 | MIT OR Apache-2.0 | direct |
| signal-hook-registry | 1.4.8 | MIT OR Apache-2.0 | transitive |
| simd-adler32 | 0.3.10 | MIT | transitive |
| subtle | 2.6.1 | BSD-3-Clause | transitive |
| syn | 2.0.119 | MIT OR Apache-2.0 | transitive |
| thiserror | 1.0.69 | MIT OR Apache-2.0 | transitive |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 | transitive |
| unicode-ident | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | transitive |
| untrusted | 0.9.0 | ISC | transitive |
| ureq | 3.4.2 | MIT OR Apache-2.0 | direct |
| ureq-proto | 0.6.4 | MIT OR Apache-2.0 | transitive |
| utf8-zero | 0.8.1 | MIT OR Apache-2.0 | transitive |
| walkdir | 2.5.0 | Unlicense/MIT | transitive |
| wasi | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | transitive |
| webpki-roots | 1.0.11 | CDLA-Permissive-2.0 | transitive |
| winapi | 0.3.9 | MIT/Apache-2.0 | transitive |
| winapi-i686-pc-windows-gnu | 0.4.0 | MIT/Apache-2.0 | transitive |
| winapi-util | 0.1.11 | Unlicense OR MIT | transitive |
| winapi-x86_64-pc-windows-gnu | 0.4.0 | MIT/Apache-2.0 | transitive |
| windows-link | 0.2.1 | MIT OR Apache-2.0 | transitive |
| windows-sys | 0.45.0 | MIT OR Apache-2.0 | transitive |
| windows-sys | 0.52.0 | MIT OR Apache-2.0 | transitive |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | transitive |
| windows-targets | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_aarch64_gnullvm | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows_aarch64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_aarch64_msvc | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows_aarch64_msvc | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_i686_gnu | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows_i686_gnu | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_i686_gnullvm | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_i686_msvc | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows_i686_msvc | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_x86_64_gnu | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_x86_64_gnullvm | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows_x86_64_gnullvm | 0.52.6 | MIT OR Apache-2.0 | transitive |
| windows_x86_64_msvc | 0.42.2 | MIT OR Apache-2.0 | transitive |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 | transitive |
| zeroize | 1.9.0 | Apache-2.0 OR MIT | transitive |
| zlib-rs | 0.6.8 | Zlib | transitive |

## 3. Pumpkin (GPL-3.0) との byte-for-byte 比較 / Byte-identity with Pumpkin files

> **状況 / Status (この PR 以降 / since this PR)**: 下の3ファイルは作業ツリーから**削除済み**で、1.21.1 jar から生成した別形式・別内容のデータに置き換えた (§5)。**過去の commit (履歴) には残っています**。この PR は履歴を書き換えません。下の表は削除前の比較の記録です。
> The three files below were **removed from the working tree** and replaced by data generated from the 1.21.1 jar (§5). They remain in git history; this PR does not rewrite history. The table is the record of the pre-removal comparison.
>
> **追記 (2026-11-19, Stage F1 / P0-3 D3–D4)**: この削除を実際に完了させた。`git rm assets/pblocks.json assets/properties.json resourcepack.zip` を実行し、消費者を差し替えた。`BReg::load` は `assets/pblocks.json` / `assets/properties.json` を読まず、`assets/block_states_1211.json` 一本になった (状態 id の対応付けと `PB_ALIAS` の `chain`→`iron_chain` 変換も不要になり削除)。再混入は `tools/provenance_check.py` が fail-closed で止める。
> **削除前の差分測定 (1.21.1 jar の出力 vs 旧 pblocks.json、25,534 状態を block 名と状態位置で対応付け)**: luminance / opacity の不一致 **0**、`isReplaceable()` (jar の flags bit 2) と旧 `state_flags` bit 4 の不一致 **0**、衝突 AABB の不一致 **1** のみ — `minecraft:pitcher_crop` の `half=upper, age=0` で、1.21.1 は 1 個の box (`[0.3125,-0.0625,0.3125]..[0.6875,0.1875,0.6875]`) を返し、旧 pblocks は空だった。旧ファイルには 1.21.1 に無い `note_block` の状態 (1,350 vs 1,150) と `pale_oak` 系があるため、block 単位で位置対応できない状態は比較から除外した (25,534 / 26,684)。
> **Measurement before removal (1.21.1 jar output vs the old pblocks.json, 25,534 states paired by block name and state position)**: 0 luminance/opacity differences, 0 differences between `isReplaceable()` and the old `state_flags` bit 4, and exactly 1 collision-AABB difference - `pitcher_crop` at `half=upper, age=0`, where 1.21.1 returns one box and the old asset had none. States that cannot be paired positionally (the old file's extra `note_block` states, `pale_oak`) were excluded from the comparison, not from the new export.

2026-10-25 に、Pumpkin-MC/Pumpkin (`LICENSE` は GPL-3.0、issue #3 が参照する commit `b0c48b2d7688bbfa09956adf530b8bc56dcc0c56`, 2026-10-11) の公開ファイルを GitHub API で取得し、sha256 を比較した事実:

| megu のファイル | Pumpkin のファイル @ `b0c48b2` | sha256 | 結果 |
|---|---|---|---|
| `assets/pblocks.json` | `assets/blocks.json` | `cc748df18e6fbc3f0016c4616cc869c168d77bf5d8ec283f6ef3b5f412613e28` | 同一 (7,526,001 bytes) |
| `assets/properties.json` | `assets/properties.json` | `da8e094913cb05348092b2fcd6b32462dd81b3602d6d89b146529f0ac655ab85` | 同一 |
| `datapacks/vanilla/multi_noise_biome_tree.json` | `assets/multi_noise_biome_tree.json` | `a2e450330744d04404d68f4a14511a46f9508417d607ce93c61c7564e9c8a595` | 同一 (1,827,624 bytes) |

追加の事実 / Additional facts:

- 時系列 / Timeline (GitHub API で確認): Pumpkin の `assets/blocks.json` は commit `df88eeaa` (2026-09-18)、`9707ae45` (2026-10-14)、`8e61e0bd` (2026-10-17) で変更されています。`df88eeaa` 時点の内容は `cc748df1…` と同一で、これは megu が初めて `assets/pblocks.json` を追加した commit `f9924b0` (2026-10-05, "Restore megu v2.37 source and runtime assets") より前です。`assets/multi_noise_biome_tree.json` の Pumpkin 側の最終変更は `df88eeaa` (2026-09-18) です。**どちらが他方から取り込んだか、共通の上流から得たかは、このリポジトリの情報からは判断できません。**
- Pumpkin の `assets/NOTICE.md` (同 commit に存在) は、`assets/blocks.json` や `assets/properties.json` などを「© Mojang Studios / Microsoft」の抽出・派生物であり、Pumpkin の GPLv3 の対象ではない、と記述しています (Pumpkin 側の記述の引用であり、ここでの判断ではありません)。
- 今回の比較が確認したのは上記3ファイルだけです。(その後の §9 の比較も参照。)
- 再現方法 / Reproduce: `gh api -H 'Accept: application/vnd.github.raw' 'repos/Pumpkin-MC/Pumpkin/contents/assets/blocks.json?ref=b0c48b2d7688bbfa09956adf530b8bc56dcc0c56' | sha256sum` と `sha256sum assets/pblocks.json`。
- README.md の記述 (「`assets/` は Mojang 由来」「GPL/AGPL の実装コードは取り込んでいない」) と、issue #3 の方針 (GPL のソース/アセットを取り込まない) は **コードについて** の記述です。上記のデータファイルの出所は、現在の文書だけでは確認できません。

## 11. Brigadier 1.3.10 の移植 / Port of Brigadier 1.3.10

issue #51 (Brigadier 互換のコマンドディスパッチャ) の slice 1 で、`src/brigadier.rs` を Mojang の
Brigadier (MIT) から Rust に翻訳した。**上流の Java ソース・jar はこのリポジトリに含まれない**
(参照クローンはリポジトリ外のローカルデータに置き、本リポジトリには含めない)。
Pumpkin (GPL-3.0) のコードは読んでいない・参照していない。

固定した参照 / Pinned reference (2026-11-17 に確認):

| 項目 | 値 |
|---|---|
| 上流 | `https://github.com/Mojang/brigadier` |
| 版 | 1.21.1 が同梱する `com.mojang:brigadier:1.3.10` |
| 上流のタグ | `1.3.10` は**存在しない** (公開タグは `1.0.15` / `1.0.17` / `1.0.18` のみ、`gradle.properties` は `majorMinor: 1.3`) |
| 固定した commit | `b5419b186e4882290a188d11d6e661d4f56bd8fd` "Add ArgumentType.parse() overload that can accept source context" (2024-08-07) |
| 版の一致の根拠 | 1.21.1 の server.jar (`META-INF/libraries.list`, sha1 `c8ee4136e474ac7723ca2b432ec8d1a2bc88ef7d1ec57c314ba9e33cdc83dd75`) から取り出した brigadier-1.3.10.jar の `javap com.mojang.brigadier.context.SuggestionContext` は `parent` と `startPos` のみで、2024-12 の suggestion-context 変更 (commit `24b4072`, `9ba4f13`) を含まない。`CommandDispatcher` の公開シグネチャも固定した commit と一致 |
| LICENSE | `LICENSE` (MIT, 「Copyright (c) Microsoft Corporation. All rights reserved.」) を改変せず保持。sha256 `d25deed12d803f6c973597348627b1939e2d19bbc28fda83c93ea0cde44702da` |

取得と確認の手順 / Reproduce: `git clone https://github.com/Mojang/brigadier` して
`git checkout b5419b186e4882290a188d11d6e661d4f56bd8fd`。版の一致は 1.21.1 の server.jar から
`META-INF/libraries/com/mojang/brigadier/1.3.10/brigadier-1.3.10.jar` を取り出し、
`javap -p -classpath <展開先> com.mojang.brigadier.context.SuggestionContext` で確認した。
移植部分と意図的な差異は `src/brigadier.rs` のモジュールドキュメントに列挙している。

## 4. オーナーへの質問 / Questions for the owner

1. (削除済み) `assets/pblocks.json`, `assets/properties.json`, `datapacks/vanilla/multi_noise_biome_tree.json` は、どこから入手しましたか? 履歴には残っているので、来歴の確認は引き続き必要です。 Pumpkin のリポジトリからですか、Mojang のリリースから自分で生成したものですか、第三者の配布物ですか? (§3 のとおり Pumpkin のファイルとバイト単位で同一です。)
2. (調査済み) `assets/blocks_1211.json` は PrismarineJS/minecraft-data (MIT) の `data/pc/1.21.1/blocks.json` とバイト単位で同一でした (§1)。入手元がそこで正しいか確認してください。
3. (対応済み・§9) 旧 `datapacks/vanilla/data/**` (1083 files) は削除し、1.21.1 jar から生成する形にしました。履歴に残る旧データの出所は引き続き不明です。
4. (対応済み・§9) `config_packets.bin` は実サーバーから再取得でき、開発ブランチでは追跡しています。
5. (対応済み・§9) javap / マッピング抜粋は削除しました (`tools/bytecode_probe.sh` で再生成できます)。履歴には残っています。
6. (対応済み・§9) `resourcepack.zip` は削除しました (任意機能)。履歴には残っています。
7. (確認・対応済み, 2026-11-20) `datapacks/{modtest,rectest,spinetest,tagtest,testpack}` はオーナー自作（first-party）と確認済み。リポジトリ直下の `*.storyboard.png` 6 ファイルは不要につき削除済み。
8. このリポジトリは過去に公開されていた形跡があります (issue #11)。公開されていた期間に上記のファイルは含まれていましたか? 履歴にも残っています (この PR は履歴を書き換えません)。
9. 外部からの貢献やコードの取り込みについて、出所とライセンスを記録しますか? (以前ここに挙げていた `src/generation_math.rs` のテスト値は §10 で jar から生成した値に置き換え済み。)
10. 依存クレートのうち `webpki-roots` は `CDLA-Permissive-2.0`、`unicode-ident` は `(MIT OR Apache-2.0) AND Unicode-3.0` を宣言しています。配布時の表記方法 (ライセンス全文の同梱など) はどうしますか? (`deny.toml` は許可リストに載せているだけで、表記義務の遵守を確認するものではありません。)

回答が得られたら、この表の出所列を `documented` に更新し、README の記述 (README.md「ライセンスと第三者データ」) を更新してください。

## 5. 生成データと再生成ツール / Generated data and how to regenerate it

開発ブランチでの追跡状況は §1 の通り。公開候補はこれらの生成物を含まないので、ビルド・実行前に利用者が手元で生成します（手順: [開発ガイド](docs/development.md)）。

```
tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/1.21.1-server.jar [--mappings DIR | --fetch-fabric-mappings]
```

- 入力 jar: Minecraft 1.21.1 server (bundler) jar, sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` (スクリプトが検査)。jar 自体はリポジトリに含めない・ダウンロードしない。
- 手順: (1) 公式 data generator (`--reports --server`) の `blocks.json` と `biome_parameters/*.json` を**参照値**にする。(2) `tools/vanilla_data/Extract.java` が jar の 1.21.1 ブートストラップを実行し、状態ごとの luminance / opacity / flags / full_sides、`getCollisionShape` の AABB 列、biome RTree をリフレクションで取り出す。(3) `build_vanilla_data.py` が参照値と突き合わせて (状態 id・名前・leaf 集合・枝の境界) から `assets/block_states_1211.json` と `tests/data/*.tsv` を書く。(4) 同じスクリプトが `datapacks/vanilla/data/minecraft/**` (data generator の `worldgen/{biome,configured_carver,configured_feature,density_function,flat_level_generator_preset,multi_noise_biome_source_parameter_list,noise,noise_settings,placed_feature,processor_list,structure,structure_set,template_pool,world_preset}`・`tags/block`・`dimension_type`)、jar のメンバーである 1,180 個の構造テンプレートと `tags/worldgen/{biome,structure}`、`multi_noise_biome_tree.json` (`tools/biome_tree_from_jar.py`)、`pack.mcmeta` を**ステージングディレクトリに生成し、`tools/vanilla_data/verify_pack.py` の目録・ハッシュ検査を通してから**差し替える。(5) `--accept-eula` で `config_packets.bin`。
- 生成器の検査 / Generator checks: `verify_pack.py` は data generator 由来 1,084 JSON と jar メンバー 1,263 個の**件数・JSON 妥当性・`members_sha256` 全件一致・予期しないディレクトリ/ファイルが無いこと**を検査し、1 つでも外れたら exit 1 でツリーを差し替えない。jar 無しでも走るので CI で使える (`gen_vanilla_data.sh --verify`)。
- 難読名の解決: この jar は難読化されており、Mojang のマッピングは使っていない。代わりに Fabric の `intermediary-1.21.1-v2` と Yarn `1.21.1+build.3-v2` (どちらも CC0-1.0, maven.fabricmc.net) を使い、**実行時に**クラス・メンバー名を引く。sha256 を固定: intermediary `e8bd6a4c39e235ee2dc73063558610775c72a2572fa0c6b32915b149074012a6`、Yarn `4ec570889585cb958b7d3c9dd94ce4064b16ab797dffec152bc8486c59a4010b`。これらは**ツールの入力**であり、リポジトリには含めない。
- 検証 (cargo test): `block_registry` / `registry_tests` が全 26,684 状態を独立に読み直して照合。`worldgen` が (a) 7,593 + 5 leaves が 1.21.1 のみの biome であること (pale_garden / dappled_forest / sulfur_caves が無いこと)、(b) ゲーム自身の検索結果 6,000 点のうち等距離の同点でない 5,415 点すべてが一致することを検査する (同点 585 点は、ゲーム内でも検索の履歴依存で答えが決まるため集計のみ)。
- 旧データとの差: 対応可能な状態での比較範囲・差は §3 を参照。biome は旧 overworld tree の 133 leaf (`dappled_forest` 92 / `pale_garden` 40 / `sulfur_caves` 1) が 1.21.1 には存在せず、対応する位置は `plains` (92) と `dark_forest` (40) になる。衝突形状は 25,534 の対応可能な状態で 1 件だけ差があり、その 1 件は旧ファイルが誤っていた (§3 追記)。
- 含まれるもの / Included: 衝突形状 (`shapes` + `collision_shapes`、1.21.1 の 26,684 状態で `getCollisionShape(...).isEmpty()` と全件一致)。
- 含まれないもの: 選択形状 (`outline_shapes`、描画専用で生成側に消費者が無い)、摩擦・速度係数、`blast_resistance`、`instrument`、`map_color`、`piston_behavior` (旧 pblocks にあったが、Rust 側の消費者は無かった)。必要になったら `Extract.java` を拡張する。

## 6. バージョン不整合の調査結果 / Version-alignment findings

- (**この PR で解消** §9: 旧データを削除し、ローダーを 1.21.1 形式に移植した。以下は削除前の観察の記録。) `datapacks/vanilla/data/minecraft/**` の worldgen 部分は、1.21.1 jar の `data/minecraft/**` と意味的には一致しない。207 ファイルが内容差、517 ファイルがリポジトリ側のみ、約420 JSON のみ一致 (placed_feature 233, tags/block 180, carver 4, density_function 2)。例: `base_amplitude` / `base_octave` noise、`axis` つきの `gradient` density function、`material_rule` / `aquifers` つきの noise_settings、`attributes` つきの biome、`worldgen/feature` ディレクトリ。Rust 側のローダー (`worldgen.rs` の `dp_files`) もこの形式を読むので、**1.21.1 形式に揃えるにはローダーの移植が必要で、この PR では行っていない**。
- 既知の食い違いを示すコメントは `worldgen.rs` (約5358, 5405 行付近) と `tools/vanilla_tags_sync.py` / `tools/vanilla_nofeat_pack.py` にある。
- `config_packets.bin` には 26.x の識別子は見つからなかった (908 個の識別子)。(§9: 1.21.1 jar から再取得でき、旧ファイルと registry data は一致した。)

## 7. この文書の更新方法 / Maintaining this file

- Rust 依存の表は `cargo metadata --format-version 1 --locked --all-features` から再生成できます。
- 新しい第三者データ・逆アセンブル・マッピングをコミットするときは、§1 に行を足してください。Mojang 由来のものはコミットせず、生成スクリプトに足してください (§9)。

## 8. Pumpkin とのコード一致: 書き直しの結果 / Code-match rewrite results

検査 / Scan: `scan5.py` (コメントと文字列を除いたトークン列を 12 トークン単位で Pumpkin `b0c48b2` の全 `.rs` と比較し、連続 24 トークン以上かつ識別子 6 個以上の一致を報告)。**一致が見つかるかどうかの目安であり、複製や権利についての判断ではありません。** A 12-token shingle scan against Pumpkin `b0c48b2`; runs of ≥24 tokens with ≥6 identifiers are reported. It is a screening tool, not a legal or copying verdict.

方法 / Method: 該当箇所を、公開されているアルゴリズムの説明と vanilla の挙動 (既存の `javap`/仕様メモ、Gustavson の simplex noise 論文、Blackman/Vigna の xoroshiro、Stafford の Mix13、Lemire の範囲付き乱数) だけから書き直した。書き直し中に Pumpkin のソースは開いていない。浮動小数点の演算順序と乱数の消費順序は元のコードと同一に保った。確認: 既存テスト全て変更なし、加えて一時的な差分テスト (xoroshiro / simplex / Perlin / 地形列 / 3 seed × 49 チャンクの feature 配置のハッシュ) が書き直し前後で完全一致 (167 行)。この一時テストはコミットしていない。

結果 / Result: 同じ検査の再実行で 49 件 → **6 件**。

| 範囲 | 検査件数 前→後 | 扱い |
|---|---|---|
| `generation_math.rs` Xoroshiro / Stafford Mix13 / Lemire / Perlin 勾配 (5 件) | 5→0 | 書き直し (定数は公開アルゴリズムで固定) |
| `generation_math.rs` simplex 2D/3D | 9→0 | 書き直し (角寄与・ハッシュ連鎖・軸順序を別構造に。`GRAD` 表は公開の勾配表) |
| `worldgen.rs` spline (Hermite) | 1→0 | 書き直し (係数を端点ごとに分解。数式自体は三次 Hermite) |
| `worldgen.rs` aquifer 圧力項 | 1→0 | 書き直し (分子/分母の組へ。係数 1.5/2.5/3/10 は vanilla の値) |
| `feature.rs` ore / vein | 6→0 | 書き直し (球のチェーンを struct 化、重なり除去を分離) |
| `feature.rs` lake / underwater magma | 3→0 | 書き直し |
| `feature.rs` monster room | 4→0 | 書き直し |
| `feature.rs` 2713 (テスト補助) | 1→0 | 書き直し |
| `tree.rs` dark oak 幹 / foliage `gen_square` / decorators (shuffle ×3, alter ground) | 6→0 | 書き直し (共通ヘルパー化) |
| `generation_providers.rs`, `landing.rs`, `main.rs` (座標・小さな反復) | 6→0 | 書き直し (自明な箇所) |
| `tree.rs` forking 幹 (前 2 → 後 2 箇所), fancy 幹 (前 2 → 後 1 箇所) | 4→3 | この時点では未書き直し (§10 で書き直し) |
| `cached_lookup.rs` Default 実装, `main.rs` `args().skip(1).collect()`, `world_rng.rs` テストの反復 | 3→3 | この時点では未書き直し (§10 で書き直し) |

注意 / Notes:
- `Xor::next_bound` の閾値計算は 64 ビット剰余で、vanilla の 32 ビット符号なし剰余と非 2 冪の bound で異なり得る既存の挙動です。**挙動を変えないため触っていません** (再抽選が起きる稀な経路のみ)。
- `BT::search` は子を距離順に並べますが、vanilla は配列順に走査します (既存の挙動、未変更)。
- この検査はコードのみです。テスト値の表 (`AQ_NB`, `biome_vectors` など Pumpkin のテストと数値が一致するもの) と、Pumpkin とバイト一致する `data/minecraft/tags/block` の 12 ファイルは対象外でした (**タグは §9 で削除して jar からの生成に、テスト値は §10 で jar から生成した照合値に置き換えた**)。
- 履歴には書き直し前のコードが残っています (この PR は履歴を書き換えません)。

## 9. Mojang 由来データのコミット廃止 / Removing committed Mojang-derived data

以下は #20 の上で行った変更の履歴であり、現行の追跡状況 (§1) ではない。当時、Mojang 由来または出所不明だったデータと資料をコミットから外した。**履歴には残っている** (この PR は履歴を書き換えない。公開前に履歴の扱いを決める必要がある、README の残課題を参照)。

| 対象 | 変更 |
|---|---|
| `datapacks/vanilla/data/minecraft/**` (1083 files) | 削除。`tools/vanilla_data/gen_vanilla_data.sh` が jar の data generator 出力から `worldgen/{biome,configured_carver,configured_feature,density_function,noise,noise_settings,placed_feature}` と `tags/block` (783 JSON + pack.mcmeta) をコピーして生成する。**ローダーを 1.21.1 形式に移植**した (旧データは 26.x 形式): `configured_feature` ディレクトリ、`argument`/`argument1`/`argument2`、`y_clamped_gradient`、`weird_scaled_sampler`、`shifted_noise`、`firstOctave`/`amplitudes`、`noise_router` と `surface_rule` のインライン定義、`{Name, Properties}` 形式の block state、`carvers: {"air": [...]}`、および 1.21.1 に無い preliminary surface / aquifer exclusion の合成。 |
| `datapacks/vanilla/data/minecraft/tags/block/*.json` のうち Pumpkin と同一だった 12 ファイル | 削除し、jar から生成した 1.21.1 のタグに置き換えた。12 個のうち 7 個 (`chains`, `concrete`, `concrete_powders`, `default_immune_to`, `glazed_terracotta`, `nether_portal_frame`, `supports_chorus_flower`) は 1.21.1 に存在しないので生成されない。5 個 (`enchantment_power_provider`, `trail_ruins_replaceable`, `shulker_boxes`, `tall_flowers`, `deepslate_ore_replaceables`) は jar 由来の内容になった。 |
| `config_packets.bin` | コミットから外し、`tools/vanilla_data/capture_config_packets.py` で生成 (実 1.21.1 サーバーを一時起動して Configuration フェーズを記録)。旧ファイルと同サイズ (105,095 bytes) で、registry data は先頭から一致、tag パケットの registry の並びだけが異なった。 |
| `resourcepack.zip` | 削除 (サーバーは無ければ配信しない任意機能)。`.gitignore` に追加。Stage F1 で `git rm` により索引からも削除し、`tools/provenance_check.py` が再混入を止める。 |
| `evidence/gameplay/**` の javap 6 ファイル + マッピング抜粋 3 ファイル | 削除。`tools/bytecode_probe.sh` が再生成する。`receipt.json` の sha256 固定 (javap / mappings / `config_packets.bin`) は外し、`generated_inputs_note` を足した。 |
| `assets/block_states_1211.json`, `tests/data/biome_probe_1211.tsv`, `datapacks/vanilla/{biome_climate_tree.json,pack.mcmeta}` | コミットから外し、`.gitignore` へ (#19 では「コミットする」方針だった。§5 を更新)。現行ブランチは `assets/block_states_1211.json` を**追跡**しており (公開候補では除外)、`biome_climate_tree.json` は生成対象から外した (消費者は `multi_noise_biome_tree.json` だけ)。 |
| `tools/vanilla_tags_sync.py` | 削除 (リポジトリのタグを jar と同期する道具で、データ自体を jar から生成するようになったため不要)。 |

**Pumpkin との照合 (再確認、`b0c48b2` の全ファイルの git blob sha と比較)**: 旧 HEAD で一致したのは `tags/block` の 13 ファイルのみ (他のパスはなし)。この PR の HEAD にコミットされている全ファイルで一致は 0。生成された 784 ファイルのうち 6 個 (`tags/block/{trail_ruins_replaceable,tall_flowers,incorrect_for_diamond_tool,incorrect_for_netherite_tool,enchantment_power_provider,concrete_powder}.json`, 18–652 bytes) は、Pumpkin の短い JSON とバイト一致する。これは jar が出力する短いリストが同じ内容になっただけで (例: `enchantment_power_provider` は `["minecraft:bookshelf"]` で、Pumpkin の `c:bookshelves` と同一)、コミットされず、利用者の手元で生成される。

**セットアップ**: `tools/vanilla_data/gen_vanilla_data.sh --jar server.jar --fetch-fabric-mappings --accept-eula` (README 参照)。`--accept-eula` を付けると Minecraft の EULA に同意したことになる (サーバーを一時起動するため)。付けなければ `config_packets.bin` 以外が生成される。

## 10. 識別子の改名・テスト値の置き換え・文言の整理 / Renames, generated test vectors, wording cleanup

この PR (#22 の上に積んだ変更) の内容。**挙動は変えていない**。履歴には旧コードが残っている (この PR は履歴を書き換えない)。事実の記録であり、ライセンスや複製についての判断ではない。

| 項目 | 変更 |
|---|---|
| 識別子の改名 (src) | Pumpkin の識別子と一致していたものを、vanilla の名前、なければこのリポジトリ独自の名前に改名した。`hash_block_pos`→`mth_get_seed`、`java_hash`→`string_hash_code`、`build_octaves`→`octave_table`、`legacy_fbm`→`legacy_octave_perlins`、`resolve_rule`→`load_named_rule`、aquifer の `aq_*` 関数と定数、`fetch_skin`→`load_skin_properties`、`flush_block_updates`→`apply_queued_block_updates`、`reschedule`→`requeue`、`CarveRun`→`CarveSpan`、など。以前の一致範囲内のローカル変数名とコメントの言い回しも置き換えた。 |
| tree / cached_lookup / world_rng / main | `tree.rs` の forking 幹 (`lay` クロージャで行ごとに配置、乱数の消費順序は不変) と fancy 幹のローカル名、`cached_lookup.rs` の Default、`world_rng.rs` のテストの反復、`main.rs` の引数処理を書き直した。`tree.rs` の型名・feature 名は vanilla のデータパックの `tree` configured feature の名前に従う (そのまま)。 |
| 削除 | `normal_new` (未使用)、`noise_id` の旧スキーマ分岐 (1.21.1 のデータには存在しない形)、`main.rs` の `WGDIFF` デバッグ出力 (他実装から取った参照値を含んでいた)。 |
| テスト値 | `AQ_NB` ほか 7 個の aquifer 表 (約 780 行)、`biome_vectors` の固定値、xoroshiro のテストベクタ (他実装のテストの数値と一致していた) を削除。代わりに `tools/vanilla_data/Extract.java` の `Vectors` が、利用者の 1.21.1 jar のゲーム本体の関数 (aquifer の `getFluidLevel` ほか、NoiseRouter、MultiNoiseSampler、Xoroshiro) をランダムな問い合わせで呼び、`tests/data/vanilla_vectors_1211.tsv` (開発 HEAD では未追跡) に書き出す。テスト (`aquifer_vectors`, `biome_vectors`, `climate_functions_match_the_games_router`, `xoroshiro_parity`) がその値と照合する。 |
| 旧表との差 | 旧表の 1270 個の aquifer 値と biome の 2 ベクタは、jar から得たゲームの値と全て一致した (期待値の変更なし)。新しいランダム問い合わせでの差は f32 (ゲーム) と f64 (このコード) の丸めのみ: biome の気候パラメータで 1 単位 (180 個中 1 個、depth 6177 と 6178)、density の最大差 4.08e-8。テストの許容誤差をそれに合わせて明記した。 |
| 生成物 | `tests/data/vanilla_vectors_1211.tsv` を追加 (公開候補で ignore、NOTICE に記載)。 |
| 残る言及 | README / THIRD_PARTY.md / NOTICE の帰属、`tick_queue.rs` の設計参照 (設計のみ、issue #3)、`specs/` と `DESIGN.md` の設計参照、`evidence/foundation/provenance/` の 2026-10-21 の測定記録。ブロック名 `pumpkin` / `carved_pumpkin` は Minecraft の名前。 |
| `evidence/time-contract/**/*.log` ほか | 一部の AI レビューのログ (Pumpkin に触れた引用を含む) は `main` にのみ存在し、このブランチの系列 (#16→#19→#20→#22) では #20 で削除済み (`evidence/time-contract/REDACTION.md` 参照)。公開リポジトリから消えるのは `main` を置き換える・書き換えるときで、この PR は `main` に触れない。 |

