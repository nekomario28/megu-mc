# Mod を Rust へ移植する / Port Your Mod to Rust

`tools/megu_port_mod.py` は Fabric / NeoForge / Forge の `.jar`、Modrinth の
`.mrpack`、または展開済み Mod ディレクトリを **JVM なし・一時展開なし** で読み取り、
`tools/megu_mod_dev.py validate` が受理するネイティブ Megu プロジェクトへ変換します。
Python 標準ライブラリのみで動作します。

`tools/megu_port_mod.py` reads a Fabric / NeoForge / Forge `.jar`, a Modrinth
`.mrpack`, or an extracted mod directory **without a JVM and without extracting
to disk**, and emits a native Megu project that `tools/megu_mod_dev.py
validate` accepts. Python standard library only.

- 関連ツール / related tools: [megu_mod_dev.py](../tools/megu_mod_dev.py) (`validate` / `pack` / `watch`)
- ランタイム規約 / runtime authority: [mod-adapter-core/src/unified_bundle.rs](../mod-adapter-core/src/unified_bundle.rs)
- バンドルスキーマ / bundle schema: [sdk/schemas/megu-mod-bundle-v1.schema.json](../sdk/schemas/megu-mod-bundle-v1.schema.json)
- Mod SDK ガイド / SDK guide: [mod-sdk-guide.md](mod-sdk-guide.md)
- SDK クレート / Rust crate: [sdk/megu-mod-sdk](../sdk/megu-mod-sdk/Cargo.toml)

## 1. クイックスタート / Quickstart

```sh
# 分類レポートのみ (読み取り専用、ファイルは生成しない)
python3 tools/megu_port_mod.py analyze mods/example.jar --json

# Megu プロジェクトへ変換 + 決定論的 .mmod を dist/ に生成
python3 tools/megu_port_mod.py port mods/example.jar --out ported/example

# Modrinth 提出用 .jar も同時生成 (megu-mod-bundle.json + 両ローダーマニフェスト同梱)
python3 tools/megu_port_mod.py port mods/example.jar --out ported/example --format both

# .class 定数プールを解析して rust-port/ クレート雛形 + PORT_REPORT.md を生成
python3 tools/megu_port_mod.py port mods/example.jar --out ported/example --rust-scaffold
```

`port` は出力プロジェクトをそのまま `validate` に通し、失敗した場合は
診断を表示して終了コード `1` を返します。`analyze` は同じ検査を
**ファイルを一切書かずに** 行います。

`port` runs the generated project through `validate` itself; a failure prints
the diagnostics and exits `1`. `analyze` performs the same inspection while
writing nothing.

## 2. 自動で移植されるもの / What is auto-ported

| 入力 / input | 出力 / output |
| --- | --- |
| `data/<ns>/recipe(s)/**`, `tags/**`, `loot_table(s)/**`, `worldgen/**`, `data_maps/**`, `dimension*/**` | `data/` にそのままコピー / copied verbatim |
| `assets/<ns>/lang/**`, `textures/**`, `models/**`, `blockstates/**`, `sounds/**` | `assets/` にそのままコピー / copied verbatim |
| `fabric.mod.json`, `META-INF/neoforge.mods.toml`, `META-INF/mods.toml`, `mcmod.info`, `quilt.mod.json` | `megu-mod.toml` の `[mod]` 表 / merged into the manifest |
| `META-INF/jarjar/*.jar`, `META-INF/jars/*.jar` (Jar-in-Jar) | 深さ 3・64 個・64MiB の上限内で再帰的に fold / folded recursively under bounded budgets |
| `.mrpack` の `overrides/` (+ `clientOverrides`/`serverOverrides`) | `data/`・`assets/` だけ fold。`files[]` の外部ダウンロードはレポートに列挙 / overrides folded; declared downloads are listed, never fetched |
| Entity 系 `.class` (`*Entity`, `*Mob`, `*Animal`, ...) | bundle `creatures[]` エントリ (movement `land`) へ近似 / approximated into the bundle |
| ネットワーク payload 系 `.class` (`*Packet`, `*Payload`, ...) | bundle `custom_channels[]` の channel id / declared as channel ids |

生成される `megu-mod-bundle.json` は厳格な `megu-mod-bundle/1` です。
ブロック・アイテムの登録はバンドルのキーに存在しないため、`data/` と
`rust-port/` のスタブで表現します (§4)。

The synthesized `megu-mod-bundle.json` is strict `megu-mod-bundle/1`. Because
the bundle has no `blocks`/`items` keys, registrations are expressed through
`data/` plus the `rust-port/` stubs (§4).

## 3. 安全境界 (fail-closed) / Safety boundary

`mod-adapter-core` の bounded reader と同じ方針です。

Same policy as the `mod-adapter-core` bounded readers:

- `..`・`.`・空セグメント・絶対パス・ドライブレター・`\`・NUL を含むメンバー名は拒否。
- メンバーごと・合計の非圧縮バイト上限、メンバー数上限、圧縮率ヒューリスティックで zip bomb を遮断。ヘッダーの申告サイズを超えた場合も中断します。
- 破損したネスト JAR はその 1 件だけ `rejected` として記録し、兄弟 JAR の内容は保持します (トップレベルアーカイブの違反は fatal)。
- ディレクトリ入力ではプロジェクト外を指す symlink を拒否します。

Member names containing `..`, `.`, empty segments, absolute paths, drive
letters, `\`, or NUL are rejected. Per-member and aggregate uncompressed
byte caps, a member-count cap, and a compression-ratio heuristic stop zip
bombs before decompression; reads also abort past the declared size. A
corrupt nested JAR is recorded in `rejected` while sibling jars still
contribute — top-level archive violations are fatal. Directory inputs
reject symlinks resolving outside the root.

## 4. Rust 雛形 `--rust-scaffold` / The Rust scaffold

`.class` の constant pool を純 Python で解析し (magic `0xCAFEBABE`、UTF-8 /
Class 参照、superclass、interface、メソッド名 — JVM も外部ツールも不要)、
各クラスを `block` / `item` / `entity` / `block_entity` / `mixin` /
`screen` / `network` / `event_listener` / `entrypoint` / `config` に分類します。
`rust-port/` には `megu-mod-sdk` 依存のクレートが生成され、検出した
block/item/entity は `ModProjectBuilder` チェーンで登録済み、残りは
Java クラス → SDK 置き換え先を示す `TODO(port)` コメントになります。
`PORT_REPORT.md` が自動移植率とクラスごとの対応表をまとめます。
`rust-port/Cargo.toml` の `megu-mod-sdk` パスは自分のチェックアウトに合わせてください。

A pure-Python constant-pool parser classifies each `.class` by its
superclass/interfaces, and `rust-port/` receives a `megu-mod-sdk` crate:
detected blocks/items/entities are already registered on
`ModProjectBuilder`, every other class is a `TODO(port)` stub naming the
SDK surface that replaces it. `PORT_REPORT.md` summarizes the auto-ported
share. Point the `megu-mod-sdk` path in `rust-port/Cargo.toml` at your
checkout.

## 5. `--format` の違い / Output formats

| `--format` | 生成物 / artifact |
| --- | --- |
| `mmod` (default) | `dist/<mod>-<version>.mmod` — ネイティブ専用・Java メタ無し |
| `compat-jar` | `dist/<mod>-<version>.jar` — `megu-mod-bundle.json` + `META-INF/neoforge.mods.toml` + `fabric.mod.json` 同梱。Modrinth にそのまま投稿可能 |
| `both` | 両方 / both of the above |

両フォーマットとも `megu_mod_dev.py` の決定論的 packer (固定 ZIP 時刻・
ソート済みエントリ) を共有し、`sha256` / `sha512` を出力します。

Both formats reuse the deterministic `megu_mod_dev.py` packer (fixed ZIP
timestamps, sorted entries) and print `sha256`/`sha512` digests.

## 6. 制限 / Limitations

- `.class` のメソッド本体・Mixin ターゲット解決は行いません — `analyze` の分類はヒューリスティックであり、実行互換を保証するものではありません。詳細な Tier 判定は `megu-mod-check --adapt-jar` を参照してください ([mod-author-guide.md](mod-author-guide.md))。
- `.mrpack` の `files[]` は外部 URL のためバンドルしません。各 `.jar` を取得して個別に `port` してください。
- バイトコードしか持たない機能 (カスタム tick 処理、複雑な Mixin、GUI) は Rust 実装が別途必要です。

The classifier does not resolve method bodies or Mixin targets — it is a
heuristic, not a compatibility proof; see `megu-mod-check --adapt-jar` for
tier classification. `.mrpack` `files[]` are external downloads — port each
`.jar` individually. Code-only features (custom tick logic, complex mixins,
GUIs) still need a hand-written Rust port.
