# THIRD_PARTY.md — 第三者由来物の出所一覧 / Third-party provenance inventory

> **これは事実の一覧です。法的な判断・結論ではありません。** 各ファイルを再配布してよいか、どのライセンスが適用されるかは、このドキュメントでは判断していません。
> **This is an inventory of facts only. It is not a legal opinion** and does not assert the license or redistribution status of any file. Nothing was deleted or changed in the repository because of this document.

- 対象リビジョン / Revision: `main` @ `ef6e14a` (+ この PR / this PR)
- リポジトリのコード本体は MIT ([LICENSE](LICENSE))。以下の表は、**リポジトリ内にコミットされているがオリジナルのコードではないもの**(または出所が文書化されていないもの)の一覧です。
- 作成日 / Written: 2026-10-25 (Asia/Tokyo)

**出所の列の意味 / Origin column**

| 値 | 意味 |
|---|---|
| `documented` | リポジトリ内の文書・スクリプトが出所を示している |
| `undocumented` | リポジトリ内に出所を示す文書が見つからない |
| `unknown - owner to confirm` | 状況証拠はあるが、確認できていない。オーナーの確認が必要 |

## 1. コミット済みの非オリジナル資産 / Committed non-original files

| パス / Path | 内容・規模 / What & size | README の記述 | 出所 | 根拠・観察 (facts) |
|---|---|---|---|---|
| `datapacks/vanilla/` (`data/minecraft/**` 1083 files + `multi_noise_biome_tree.json` + `pack.mcmeta`) | worldgen の JSON (`worldgen/{placed_feature,feature,biome,noise,material_rule,density_function,...}`) と `tags/block` | Mojang 由来・MIT 対象外 | `unknown - owner to confirm` | `pack.mcmeta` の説明は "vanilla worldgen data"。取得手順の記録は見つからない。`tools/fixture_base.sh` 等は Minecraft 1.21.1 の server.jar を sha1 固定で使う。**`multi_noise_biome_tree.json` は Pumpkin (GPL-3.0) の `assets/multi_noise_biome_tree.json` @ `b0c48b2` とバイト単位で同一** (下記 §3)。 |
| `datapacks/{modtest,rectest,spinetest,tagtest,testpack}/` | テスト用の小さな datapack (計 約25 files) | 記述なし | `unknown - owner to confirm` | テスト用途と思われるが、作成者の記録はない (`rectest`/`testpack` は `data/minecraft` 配下に1ファイルずつ持つ)。 |
| `assets/blocks_1211.json` | ブロック物性 (1060 entries, 765 KB, `id/name/displayName/hardness/resistance/stackSize/diggable/material/...`) | Mojang 由来 | `undocumented` | 出所の記述なし。フィールド構成は PrismarineJS `minecraft-data` のブロックスキーマに似て見えるが、**未確認**。 |
| `assets/pblocks.json` | ブロック状態データ (top-level `block_entity_types`/`shapes`/`blocks`, 7.5 MB) | Mojang 由来 | `undocumented` / `unknown - owner to confirm` | 出所の記述なし。**sha256 `cc748df1…3e28` は Pumpkin-MC/Pumpkin の `assets/blocks.json` と同一** (§3)。`src/block_registry.rs:119-120` に "pumpkin state_flags / side_flags" のコメントがある。 |
| `assets/properties.json` | ブロックプロパティ定義 (124 entries, `hash_key/enum_name/serialized_name/type`) | Mojang 由来 | `undocumented` / `unknown - owner to confirm` | `RESTORATION.md` は「元の保存場所のローカル補助資産」と記述するのみ。**sha256 `da8e0949…ab85` は Pumpkin の `assets/properties.json` と同一** (§3)。 |
| `resourcepack.zip` | 345 bytes。中身は `pack.mcmeta` (59 B, "spine test pack") と `assets/minecraft/textures/block/stone.png` (72 B) | Mojang 由来 | `unknown - owner to confirm` | 非常に小さいテスト用パックに見える。72 B の PNG が Mojang のテクスチャかどうかは未確認。`RESTORATION.md` は「元の保存場所のローカル補助資産」と記述。 |
| `config_packets.bin` | 105,095 bytes。Configuration フェーズの送信パケット列 (先頭は `minecraft:brand` / `vanilla`、約900個の `minecraft:` 識別子を含む。registry / tag データと思われる) | Mojang 由来 | `undocumented` | 生成手順の記録は見つからない (読み込みは `src/main.rs` の `load_config_packets`)。実サーバーの出力を取り込んだものかは未確認。 |
| `evidence/gameplay/**/*.javap.txt` (7 files) | Mojang クラスの `javap -p -c` 逆アセンブル: `FallingBlockEntity` (obf `cjg`, 38.6 KB), `ClientboundLightUpdatePacket` (`adj`), `ClientboundLightUpdatePacketData` (`adk`), `ClientPacketListener.applyLightData`, `DataLayer` (`dvd`), `LevelLightEngine` (`eot`) | **README に記載なし** | `documented` (生成方法) | `tools/bytecode_probe.sh` が、固定した 1.21.1 server.jar (sha1 `59353fb4…8dc05c`) と公式マッピング `server.txt` (sha1 `03f89854…0f35f570`) から生成する方法を記録している。ファイル自体の再配布可否は §4 の質問。 |
| `evidence/gameplay/**/*.mapping.txt`, `evidence/gameplay/light-update/mappings.txt` (3 files) | Mojang 公式マッピング (難読名↔公式名) の対象クラス分の抜粋 (1.0–3.5 KB) | **README に記載なし** | `documented` (生成方法) | `bytecode_probe.sh` の記述 (公式 mappings を使用)。ファイルは抜粋で、`falling-vanilla/README.md` に「40行」と記載。再配布可否は §4。 |
| `evidence/foundation/audit-snapshots/main.*.rs` (4 files) | 過去の自作 `src/main.rs` のスナップショット | (第三者物ではない) | `documented` | 同ディレクトリの README が由来と sha256 を記載。第三者由来ではなく、このリポジトリの自作コードの旧版です。`AUDIT_WIRE.md` / `SECURITY_NOTES.md` から参照されています (削除していません)。 |
| `*.storyboard.png` (リポジトリ直下 6 files, `evidence/gameplay/protocol-lifecycle/spine_proto.storyboard.png`) | 図 | 記述なし | `unknown - owner to confirm` | 作成方法の記録は未確認 (自作図と思われる)。 |

## 2. Rust 依存クレートのライセンス / Rust dependency licenses

`cargo metadata --format-version 1 --locked --all-features` (Cargo.lock, 2026-10-25) の `license` フィールドをそのまま転記したもの。`cargo-deny` (`deny.toml`) が CI で同じ集合を検査します。`mc-spine` 自体はクレートメタデータに `license` を宣言していません (LICENSE ファイルは MIT)。
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

2026-10-25 に、Pumpkin-MC/Pumpkin (`LICENSE` は GPL-3.0、issue #3 が参照する commit `b0c48b2d7688bbfa09956adf530b8bc56dcc0c56`, 2026-10-11) の公開ファイルを GitHub API で取得し、sha256 を比較した事実:

| mc-spine のファイル | Pumpkin のファイル @ `b0c48b2` | sha256 | 結果 |
|---|---|---|---|
| `assets/pblocks.json` | `assets/blocks.json` | `cc748df18e6fbc3f0016c4616cc869c168d77bf5d8ec283f6ef3b5f412613e28` | 同一 (7,526,001 bytes) |
| `assets/properties.json` | `assets/properties.json` | `da8e094913cb05348092b2fcd6b32462dd81b3602d6d89b146529f0ac655ab85` | 同一 |
| `datapacks/vanilla/multi_noise_biome_tree.json` | `assets/multi_noise_biome_tree.json` | `a2e450330744d04404d68f4a14511a46f9508417d607ce93c61c7564e9c8a595` | 同一 (1,827,624 bytes) |

追加の事実 / Additional facts:

- 時系列 / Timeline (GitHub API で確認): Pumpkin の `assets/blocks.json` は commit `df88eeaa` (2026-09-18)、`9707ae45` (2026-10-14)、`8e61e0bd` (2026-10-17) で変更されています。`df88eeaa` 時点の内容は `cc748df1…` と同一で、これは mc-spine が初めて `assets/pblocks.json` を追加した commit `f9924b0` (2026-10-05, "Restore mc-spine v2.37 source and runtime assets") より前です。`assets/multi_noise_biome_tree.json` の Pumpkin 側の最終変更は `df88eeaa` (2026-09-18) です。**どちらが他方から取り込んだか、共通の上流から得たかは、このリポジトリの情報からは判断できません。**
- Pumpkin の `assets/NOTICE.md` (同 commit に存在) は、`assets/blocks.json` や `assets/properties.json` などを「© Mojang Studios / Microsoft」の抽出・派生物であり、Pumpkin の GPLv3 の対象ではない、と記述しています (Pumpkin 側の記述の引用であり、ここでの判断ではありません)。
- 今回の比較が確認したのは上記3ファイルだけです。`datapacks/vanilla/data/**`、`assets/blocks_1211.json`、`config_packets.bin` は Pumpkin との比較をしていません。
- 再現方法 / Reproduce: `gh api -H 'Accept: application/vnd.github.raw' 'repos/Pumpkin-MC/Pumpkin/contents/assets/blocks.json?ref=b0c48b2d7688bbfa09956adf530b8bc56dcc0c56' | sha256sum` と `sha256sum assets/pblocks.json`。
- README.md の記述 (「`assets/` は Mojang 由来」「GPL/AGPL の実装コードは取り込んでいない」) と、issue #3 の方針 (GPL のソース/アセットを取り込まない) は **コードについて** の記述です。上記のデータファイルの出所は、現在の文書だけでは確認できません。

## 4. オーナーへの質問 / Questions for the owner

1. `assets/pblocks.json`, `assets/properties.json`, `datapacks/vanilla/multi_noise_biome_tree.json` は、どこから入手しましたか? Pumpkin のリポジトリからですか、Mojang のリリースから自分で生成したものですか、第三者の配布物ですか? (§3 のとおり Pumpkin のファイルとバイト単位で同一です。)
2. `assets/blocks_1211.json` はどこから入手しましたか? (`minecraft-data` など第三者のデータ集ですか?) そのライセンス表記は?
3. `datapacks/vanilla/data/**` (1083 files) は、Minecraft 1.21.1 の server.jar から自分で抽出しましたか? 抽出に使った手順・jar の sha はどこかに残っていますか? 他の人の抽出物ですか?
4. `config_packets.bin` はどのように作りましたか? (実サーバーのキャプチャ? 生成スクリプト?) 手順を記録できますか?
5. `evidence/gameplay/**/*.javap.txt` (Mojang クラスの逆アセンブル) と `*.mapping.txt` / `mappings.txt` (公式マッピング抜粋) は、このリポジトリに置いたまま再配布してよいと考えていますか? リポジトリを公開する (または公開状態にしておく) 予定がある場合、残す・生成スクリプトだけにする・削除する、のどれにしますか? (`tools/bytecode_probe.sh` は同じ出力を再生成できます。)
6. `resourcepack.zip` 内の `stone.png` (72 bytes) は自作ですか、Mojang のテクスチャですか?
7. `datapacks/{modtest,rectest,spinetest,tagtest,testpack}` と `*.storyboard.png` は自作ですか?
8. このリポジトリは過去に公開されていた形跡があります (issue #11)。公開されていた期間に上記のファイルは含まれていましたか? 履歴にも残っています (この PR は履歴を書き換えません)。
9. 外部からの貢献やコードの取り込み (`src/generation_math.rs` のテスト値 "pumpkin xoroshiro128.rs test vectors" など) について、出所とライセンスを記録しますか?
10. 依存クレートのうち `webpki-roots` は `CDLA-Permissive-2.0`、`unicode-ident` は `(MIT OR Apache-2.0) AND Unicode-3.0` を宣言しています。配布時の表記方法 (ライセンス全文の同梱など) はどうしますか? (`deny.toml` は許可リストに載せているだけで、表記義務の遵守を確認するものではありません。)

回答が得られたら、この表の出所列を `documented` に更新し、README の記述 (README.md「ライセンスと第三者データ」) を更新してください。

## 5. この文書の更新方法 / Maintaining this file

- Rust 依存の表は `cargo metadata --format-version 1 --locked --all-features` から再生成できます。
- 新しい第三者データ・逆アセンブル・マッピングをコミットするときは、§1 に行を足してください。
