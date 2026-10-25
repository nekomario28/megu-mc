# THIRD_PARTY.md — 第三者由来物の出所一覧 / Third-party provenance inventory

> **これは事実の一覧です。法的な判断・結論ではありません。** 各ファイルを再配布してよいか、どのライセンスが適用されるかは、このドキュメントでは判断していません。
> **This is an inventory of facts only. It is not a legal opinion** and does not assert the license or redistribution status of any file. (Revision note: the original version of this document changed nothing; the follow-up PR that this revision belongs to removes the three Pumpkin-identical data files and replaces them with generated data, see §3 and §5.)

- 対象リビジョン / Revision: `main` @ `ef6e14a` (+ #16 + この PR / this PR)
- リポジトリのコード本体は Apache-2.0 ([LICENSE](LICENSE)。提案段階で、公開前の確認事項は [LICENSING.md](LICENSING.md) を参照。以前は MIT でした)。以下の表は、**リポジトリ内にコミットされているがオリジナルのコードではないもの**(または出所が文書化されていないもの)の一覧です。
- 作成日 / Written: 2026-10-25 (Asia/Tokyo)

**出所の列の意味 / Origin column**

| 値 | 意味 |
|---|---|
| `documented` | リポジトリ内の文書・スクリプトが出所を示している |
| `undocumented` | リポジトリ内に出所を示す文書が見つからない |
| `unknown - owner to confirm` | 状況証拠はあるが、確認できていない。オーナーの確認が必要 |

## 1. コミット済みの非オリジナル資産 / Committed non-original files

> **この PR 以降**: Mojang 由来のデータ (`datapacks/vanilla/`、`assets/block_states_1211.json`、`tests/data/biome_probe_1211.tsv`、`config_packets.bin`) と Mojang クラスの javap / マッピング抜粋、出所不明の `resourcepack.zip` は**コミットされていません**。下の該当行は削除・生成物化の記録です (§9)。

| パス / Path | 内容・規模 / What & size | README の記述 | 出所 | 根拠・観察 (facts) |
|---|---|---|---|---|
| `datapacks/vanilla/` (旧: `data/minecraft/**` 1083 files + `biome_climate_tree.json` + `pack.mcmeta`) | **コミットから削除・生成物に変更 (§9)**。旧データは出所不明で、worldgen 部分の多くが 26.x 形式だった | Mojang 由来・MIT 対象外 | 削除済み。以後は手元の 1.21.1 jar から `gen_vanilla_data.sh` が生成 (783 JSON + pack.mcmeta, `.gitignore` 済み) | 履歴には旧データが残っています (この PR は履歴を書き換えません)。 |
| `datapacks/{modtest,rectest,spinetest,tagtest,testpack}/` | テスト用の小さな datapack (計 約25 files) | 記述なし | `unknown - owner to confirm` | テスト用途と思われるが、作成者の記録はない (`rectest`/`testpack` は `data/minecraft` 配下に1ファイルずつ持つ)。 |
| `assets/blocks_1211.json` | ブロック物性 (1060 entries, 765,435 bytes, sha256 `7d1d95c4…c654`, `id/name/displayName/hardness/resistance/stackSize/diggable/material/...`) | Mojang 由来 | `documented` (下記) | **PrismarineJS/minecraft-data の `data/pc/1.21.1/blocks.json` とバイト単位で同一** (2026-10-25 に GitHub API で取得して `cmp` で確認。最終変更 commit `d56346a4e32d`, 2024-10-26)。minecraft-data は MIT (同リポジトリの README「License: MIT」。リポジトリ直下に LICENSE ファイルは無く、著作権者名の記載は見つからなかったため、ここでは作成していない)。README は「一部のデータは wiki.vg / minecraft.gamepedia.com から抽出」とも記載。帰属表示は [NOTICE](NOTICE) と README を参照。 |
| `assets/block_states_1211.json` | **コミットから外し、生成物のみ (`.gitignore`)** (§5, §9) | Mojang 由来 (jar の事実値) | `documented` | `gen_vanilla_data.sh` が jar から生成。 |
| `tests/data/biome_probe_1211.tsv` | **コミットから外し、生成物のみ (`.gitignore`)** | 事実値 | `documented` | `gen_vanilla_data.sh` が生成し、`worldgen.rs` のテストが全点を検証する。 |
| `resourcepack.zip` (旧: 345 bytes, `pack.mcmeta` + 72 B の `stone.png`) | **削除** (§9)。サーバーは `resourcepack.zip` が無ければリソースパックを配信しない (任意機能) ので、再生成せず `.gitignore` に入れただけ | — | 削除済み | 出所不明 (Mojang のテクスチャかは未確認) だったため。 |
| `config_packets.bin` (旧: 105,095 bytes) | **コミットから外し、生成物のみ (`.gitignore`)** (§9) | Mojang 由来 | 生成手順あり: `tools/vanilla_data/capture_config_packets.py` | 手元の 1.21.1 jar から実サーバーを一時起動して Configuration フェーズを記録する (`--accept-eula` 必須)。旧ファイルとは tag パケットの registry の並びだけが異なる (同サイズ、registry data は先頭から一致)。 |
| `evidence/gameplay/**/*.javap.txt` (旧 6 files) | **削除** (§9) | — | 削除済み | Mojang クラスの逆アセンブルは再配布しない。`tools/bytecode_probe.sh <class>` が手元の jar と公式マッピングから同じ出力を作る。 |
| `evidence/gameplay/**/*.mapping.txt`, `evidence/gameplay/light-update/mappings.txt` (旧 3 files) | **削除** (§9) | — | 削除済み | Mojang 公式マッピングの抜粋は再配布しない。`tools/bytecode_probe.sh` が利用者の手元のマッピングを読む。 |
| `evidence/foundation/audit-snapshots/main.*.rs` (4 files) | 過去の自作 `src/main.rs` のスナップショット | (第三者物ではない) | `documented` | 同ディレクトリの README が由来と sha256 を記載。第三者由来ではなく、このリポジトリの自作コードの旧版です。`AUDIT_WIRE.md` / `SECURITY_NOTES.md` から参照されています (削除していません)。 |
| `*.storyboard.png` (リポジトリ直下 6 files, `evidence/gameplay/protocol-lifecycle/spine_proto.storyboard.png`) | 図 | 記述なし | `unknown - owner to confirm` | 作成方法の記録は未確認 (自作図と思われる)。 |

## 2. Rust 依存クレートのライセンス / Rust dependency licenses

`cargo metadata --format-version 1 --locked --all-features` (Cargo.lock, 2026-10-25) の `license` フィールドをそのまま転記したもの。`cargo-deny` (`deny.toml`) が CI で同じ集合を検査します。`mc-spine` 自体は `Cargo.toml` で `license = "Apache-2.0"` を宣言しています (LICENSE と一致)。この表に GPL / LGPL / AGPL / MPL のものはありません (LICENSING.md §2a)。
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

2026-10-25 に、Pumpkin-MC/Pumpkin (`LICENSE` は GPL-3.0、issue #3 が参照する commit `b0c48b2d7688bbfa09956adf530b8bc56dcc0c56`, 2026-10-11) の公開ファイルを GitHub API で取得し、sha256 を比較した事実:

| mc-spine のファイル | Pumpkin のファイル @ `b0c48b2` | sha256 | 結果 |
|---|---|---|---|
| `assets/pblocks.json` | `assets/blocks.json` | `cc748df18e6fbc3f0016c4616cc869c168d77bf5d8ec283f6ef3b5f412613e28` | 同一 (7,526,001 bytes) |
| `assets/properties.json` | `assets/properties.json` | `da8e094913cb05348092b2fcd6b32462dd81b3602d6d89b146529f0ac655ab85` | 同一 |
| `datapacks/vanilla/multi_noise_biome_tree.json` | `assets/multi_noise_biome_tree.json` | `a2e450330744d04404d68f4a14511a46f9508417d607ce93c61c7564e9c8a595` | 同一 (1,827,624 bytes) |

追加の事実 / Additional facts:

- 時系列 / Timeline (GitHub API で確認): Pumpkin の `assets/blocks.json` は commit `df88eeaa` (2026-09-18)、`9707ae45` (2026-10-14)、`8e61e0bd` (2026-10-17) で変更されています。`df88eeaa` 時点の内容は `cc748df1…` と同一で、これは mc-spine が初めて `assets/pblocks.json` を追加した commit `f9924b0` (2026-10-05, "Restore mc-spine v2.37 source and runtime assets") より前です。`assets/multi_noise_biome_tree.json` の Pumpkin 側の最終変更は `df88eeaa` (2026-09-18) です。**どちらが他方から取り込んだか、共通の上流から得たかは、このリポジトリの情報からは判断できません。**
- Pumpkin の `assets/NOTICE.md` (同 commit に存在) は、`assets/blocks.json` や `assets/properties.json` などを「© Mojang Studios / Microsoft」の抽出・派生物であり、Pumpkin の GPLv3 の対象ではない、と記述しています (Pumpkin 側の記述の引用であり、ここでの判断ではありません)。
- 今回の比較が確認したのは上記3ファイルだけです。(その後の §9 の比較も参照。)
- 再現方法 / Reproduce: `gh api -H 'Accept: application/vnd.github.raw' 'repos/Pumpkin-MC/Pumpkin/contents/assets/blocks.json?ref=b0c48b2d7688bbfa09956adf530b8bc56dcc0c56' | sha256sum` と `sha256sum assets/pblocks.json`。
- README.md の記述 (「`assets/` は Mojang 由来」「GPL/AGPL の実装コードは取り込んでいない」) と、issue #3 の方針 (GPL のソース/アセットを取り込まない) は **コードについて** の記述です。上記のデータファイルの出所は、現在の文書だけでは確認できません。

## 4. オーナーへの質問 / Questions for the owner

1. (削除済み) `assets/pblocks.json`, `assets/properties.json`, `datapacks/vanilla/multi_noise_biome_tree.json` は、どこから入手しましたか? 履歴には残っているので、来歴の確認は引き続き必要です。 Pumpkin のリポジトリからですか、Mojang のリリースから自分で生成したものですか、第三者の配布物ですか? (§3 のとおり Pumpkin のファイルとバイト単位で同一です。)
2. (調査済み) `assets/blocks_1211.json` は PrismarineJS/minecraft-data (MIT) の `data/pc/1.21.1/blocks.json` とバイト単位で同一でした (§1)。入手元がそこで正しいか確認してください。
3. (対応済み・§9) 旧 `datapacks/vanilla/data/**` (1083 files) は削除し、1.21.1 jar から生成する形にしました。履歴に残る旧データの出所は引き続き不明です。
4. (対応済み・§9) `config_packets.bin` は削除し、実サーバーのキャプチャを再現するスクリプトにしました。
5. (対応済み・§9) javap / マッピング抜粋は削除しました (`tools/bytecode_probe.sh` で再生成できます)。履歴には残っています。
6. (対応済み・§9) `resourcepack.zip` は削除しました (任意機能)。履歴には残っています。
7. `datapacks/{modtest,rectest,spinetest,tagtest,testpack}` と `*.storyboard.png` は自作ですか?
8. このリポジトリは過去に公開されていた形跡があります (issue #11)。公開されていた期間に上記のファイルは含まれていましたか? 履歴にも残っています (この PR は履歴を書き換えません)。
9. 外部からの貢献やコードの取り込み (`src/generation_math.rs` のテスト値 "pumpkin xoroshiro128.rs test vectors" など) について、出所とライセンスを記録しますか?
10. 依存クレートのうち `webpki-roots` は `CDLA-Permissive-2.0`、`unicode-ident` は `(MIT OR Apache-2.0) AND Unicode-3.0` を宣言しています。配布時の表記方法 (ライセンス全文の同梱など) はどうしますか? (`deny.toml` は許可リストに載せているだけで、表記義務の遵守を確認するものではありません。)

回答が得られたら、この表の出所列を `documented` に更新し、README の記述 (README.md「ライセンスと第三者データ」) を更新してください。

## 5. 生成データと再生成ツール / Generated data and how to regenerate it

方針 / Decision (この PR で変更 / changed in this PR): 生成物は**コミットしない** (`.gitignore`)。Mojang 由来のデータをリポジトリに置かず、利用者が手元の jar から1コマンドで生成する。ビルド・テストの前に1回 `gen_vanilla_data.sh` が必要 (README「セットアップ」)。(以前の版はこの節で「コミットする」としていた。)
Decision (changed): generated files are **not committed**; a fresh clone runs the generator once before `cargo test`.

```
tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/1.21.1-server.jar [--mappings DIR | --fetch-fabric-mappings]
```

- 入力 jar: Minecraft 1.21.1 server (bundler) jar, sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` (スクリプトが検査)。jar 自体はリポジトリに含めない・ダウンロードしない。
- 手順: (1) 公式 data generator (`--reports --server`) の `blocks.json` と `biome_parameters/*.json` を**参照値**にする。(2) `tools/vanilla_data/Extract.java` が jar の 1.21.1 ブートストラップを実行し、状態ごとの luminance / opacity / flags / full_sides と biome RTree をリフレクションで取り出す。(3) `build_vanilla_data.py` が参照値と突き合わせて (状態 id・名前・leaf 集合・枝の境界) から 3 つの出力を書く。(この PR 以降、同じスクリプトが `datapacks/vanilla/data/**`・`pack.mcmeta`・(`--accept-eula` で) `config_packets.bin` も出力する。§9)
- 難読名の解決: この jar は難読化されており、Mojang のマッピングは使っていない。代わりに Fabric の `intermediary-1.21.1-v2` と Yarn `1.21.1+build.3-v2` (どちらも CC0-1.0, maven.fabricmc.net) を使い、**実行時に**クラス・メンバー名を引く。sha256 を固定: intermediary `e8bd6a4c39e235ee2dc73063558610775c72a2572fa0c6b32915b149074012a6`、Yarn `4ec570889585cb958b7d3c9dd94ce4064b16ab797dffec152bc8486c59a4010b`。これらは**ツールの入力**であり、リポジトリには含めない。
- 検証 (cargo test): `block_registry` / `registry_tests` が全 26,684 状態を独立に読み直して照合。`worldgen` が (a) 7,593 + 5 leaves が 1.21.1 のみの biome であること (pale_garden / dappled_forest / sulfur_caves が無いこと)、(b) ゲーム自身の検索結果 6,000 点のうち等距離の同点でない 5,415 点すべてが一致することを検査する (同点 585 点は、ゲーム内でも検索の履歴依存で答えが決まるため集計のみ)。
- 旧データとの差: 旧 pblocks 由来の状態値と新ファイルは全 26,684 状態で一致 (差 0)。biome は旧 overworld tree の 133 leaf (`dappled_forest` 92 / `pale_garden` 40 / `sulfur_caves` 1) が 1.21.1 には存在せず、対応する位置は `plains` (92) と `dark_forest` (40) になる。
- 含まれないもの: 衝突・選択形状 (`collision_shapes` / `outline_shapes`)、摩擦・速度係数、`blast_resistance`、`instrument`、`map_color`、`piston_behavior` (旧 pblocks にあったが、Rust 側の消費者は無かった)。必要になったら `Extract.java` を拡張する。

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
| `tree.rs` forking 幹 (前 2 → 後 2 箇所), fancy 幹 (前 2 → 後 1 箇所) | 4→3 | **未書き直し**: vanilla 手順 (向き→傾き→枝) の乱数消費順序そのもの。差分テストが forking を通らず、挙動同一性を検証できないため触っていない |
| `cached_lookup.rs` Default 実装, `main.rs` `args().skip(1).collect()`, `world_rng.rs` テストの反復 | 3→3 | **自明・定型**: 標準的な書き方のため、そのまま (歪めない) |

注意 / Notes:
- `Xor::next_bound` の閾値計算は 64 ビット剰余で、vanilla の 32 ビット符号なし剰余と非 2 冪の bound で異なり得る既存の挙動です。**挙動を変えないため触っていません** (再抽選が起きる稀な経路のみ)。
- `BT::search` は子を距離順に並べますが、vanilla は配列順に走査します (既存の挙動、未変更)。
- この検査はコードのみです。テスト値の表 (`AQ_NB`, `biome_vectors` など Pumpkin のテストと数値が一致するもの) と、Pumpkin とバイト一致する `data/minecraft/tags/block` の 12 ファイルは対象外でした (**§9 で当該ファイルを削除し、jar からの生成に置き換えた**)。
- 履歴には書き直し前のコードが残っています (この PR は履歴を書き換えません)。

## 9. Mojang 由来データのコミット廃止 / Removing committed Mojang-derived data

この PR (#20 の上に積んだ変更) で、Mojang 由来または出所不明だったデータと資料をコミットから外した。**履歴には残っている** (この PR は履歴を書き換えない。公開前に履歴の扱いを決める必要がある、README の残課題を参照)。

| 対象 | 変更 |
|---|---|
| `datapacks/vanilla/data/minecraft/**` (1083 files) | 削除。`tools/vanilla_data/gen_vanilla_data.sh` が jar の data generator 出力から `worldgen/{biome,configured_carver,configured_feature,density_function,noise,noise_settings,placed_feature}` と `tags/block` (783 JSON + pack.mcmeta) をコピーして生成する。**ローダーを 1.21.1 形式に移植**した (旧データは 26.x 形式): `configured_feature` ディレクトリ、`argument`/`argument1`/`argument2`、`y_clamped_gradient`、`weird_scaled_sampler`、`shifted_noise`、`firstOctave`/`amplitudes`、`noise_router` と `surface_rule` のインライン定義、`{Name, Properties}` 形式の block state、`carvers: {"air": [...]}`、および 1.21.1 に無い preliminary surface / aquifer exclusion の合成。 |
| `datapacks/vanilla/data/minecraft/tags/block/*.json` のうち Pumpkin と同一だった 12 ファイル | 削除し、jar から生成した 1.21.1 のタグに置き換えた。12 個のうち 7 個 (`chains`, `concrete`, `concrete_powders`, `default_immune_to`, `glazed_terracotta`, `nether_portal_frame`, `supports_chorus_flower`) は 1.21.1 に存在しないので生成されない。5 個 (`enchantment_power_provider`, `trail_ruins_replaceable`, `shulker_boxes`, `tall_flowers`, `deepslate_ore_replaceables`) は jar 由来の内容になった。 |
| `config_packets.bin` | コミットから外し、`tools/vanilla_data/capture_config_packets.py` で生成 (実 1.21.1 サーバーを一時起動して Configuration フェーズを記録)。旧ファイルと同サイズ (105,095 bytes) で、registry data は先頭から一致、tag パケットの registry の並びだけが異なった。 |
| `resourcepack.zip` | 削除 (サーバーは無ければ配信しない任意機能)。`.gitignore` に追加。 |
| `evidence/gameplay/**` の javap 6 ファイル + マッピング抜粋 3 ファイル | 削除。`tools/bytecode_probe.sh` が再生成する。`receipt.json` の sha256 固定 (javap / mappings / `config_packets.bin`) は外し、`generated_inputs_note` を足した。 |
| `assets/block_states_1211.json`, `tests/data/biome_probe_1211.tsv`, `datapacks/vanilla/{biome_climate_tree.json,pack.mcmeta}` | コミットから外し、`.gitignore` へ (#19 では「コミットする」方針だった。§5 を更新)。 |
| `tools/vanilla_tags_sync.py` | 削除 (リポジトリのタグを jar と同期する道具で、データ自体を jar から生成するようになったため不要)。 |

**Pumpkin との照合 (再確認、`b0c48b2` の全ファイルの git blob sha と比較)**: 旧 HEAD で一致したのは `tags/block` の 13 ファイルのみ (他のパスはなし)。この PR の HEAD にコミットされている全ファイルで一致は 0。生成された 784 ファイルのうち 6 個 (`tags/block/{trail_ruins_replaceable,tall_flowers,incorrect_for_diamond_tool,incorrect_for_netherite_tool,enchantment_power_provider,concrete_powder}.json`, 18–652 bytes) は、Pumpkin の短い JSON とバイト一致する。これは jar が出力する短いリストが同じ内容になっただけで (例: `enchantment_power_provider` は `["minecraft:bookshelf"]` で、Pumpkin の `c:bookshelves` と同一)、コミットされず、利用者の手元で生成される。

**セットアップ**: `tools/vanilla_data/gen_vanilla_data.sh --jar server.jar --fetch-fabric-mappings --accept-eula` (README 参照)。`--accept-eula` を付けると Minecraft の EULA に同意したことになる (サーバーを一時起動するため)。付けなければ `config_packets.bin` 以外が生成される。

