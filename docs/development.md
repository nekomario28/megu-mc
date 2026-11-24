# 開発ガイド

環境・データ・ビルド・テスト・ソースの歩き方をまとめます。現在の状態は
[specs/README.md](../specs/README.md)、次に進む順序は [specs/ROADMAP.md](../specs/ROADMAP.md)、
提出条件は [CONTRIBUTING.md](../CONTRIBUTING.md) が正本です。このページは「どう作業するか」だけを持ちます。

## 1. 環境

- コマンドはすべてリポジトリ直下から実行します。`cd` を前提にした絶対パスは書きません。
- Rust は [rust-toolchain.toml](../rust-toolchain.toml) の **1.98.0** に固定されています。`rustfmt` と `clippy` も必要です。
  このソースは新しい標準API（`slice::as_chunks` など）を使うため、古い `stable` や OS の古い `cargo` ではビルドできません。
- python3 は `tools/` と `tests/` のスクリプトに必要です。
- Java 21 は**データ生成とnative照合のときだけ**必要です。`cargo build` / `cargo test` 自体は Java を要求しません。
- 依存crateは `cargo fetch --locked` で取得できます。取得後は `--offline` を付けてネットワークなしでビルドできます（`cargo fetch --offline --locked` が成功することを確認済み）。

## 2. データと前提

### 2.1 3つの種類

| 種類 | 例 | 公開 `main` での状態 |
|---|---|---|
| 追跡している入力 | `datapacks/testpack/`、`tests/data/` の個別 fixture | クリーンルームの T0/静的検査と、データに依存しないクレートの unit テストはクローン直後から実行可能 |
| 手元の jar から生成する入力 | `datapacks/vanilla/`、`assets/block_states_1211.json`、`config_packets.bin`、生成器の `tests/data/*.tsv` | 追跡しない。T2/T3 の実サーバー接続・vanilla データ照合には §2.2 で用意する |
| 利用者が任意で置くもの（任意機能の入力） | `resourcepack.zip` | 無ければリソースパックを配信しない。`762f312` で追跡から削除し、`tools/provenance_check.py` で再混入を禁止している |

`datapacks/vanilla/` はworldgenのJSON、block tag、構造物templateなどを含むローカル生成物です。PrismarineJS/minecraft-data（MIT）由来の旧 `assets/blocks_1211.json` も同梱していません。帰属は [NOTICE](../NOTICE) にあります。

### 2.2 生成器

手元の公式 1.21.1 **bundler** server.jar（sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`）から生成します。jar は配布もダウンロードもしません。

```sh
# マッピングを取得して生成する（config_packets.bin 以外）
tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar --fetch-fabric-mappings

# config_packets.bin も作る場合。実サーバーを一時ディレクトリで1回起動する
tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar --fetch-fabric-mappings --accept-eula
```

- `--fetch-fabric-mappings` は Fabric の intermediary / Yarn（CC0、sha256固定）の2ファイルだけをダウンロードします。手元にある場合は `--mappings DIR` を使います。
- `--accept-eula` は **Minecraft EULA（https://aka.ms/MinecraftEULA）に同意したという意味**です。サーバーをオフライン・ループバックで一時起動するためで、付けなければ `config_packets.bin` 以外が生成されます。他人の代わりに同意したり、CIで自動的に付けたりしないでください。
- 生成器の出力先はリポジトリ内です。生成物は手元のテスト用です。**コミットしないでください**（2.4）。
- `config_packets.bin` が無い場合、サーバーは起動してもクライアントへregistryを送れません（`src/main.rs` の起動時メッセージ）。

生成器と抽出器（`tools/vanilla_data/Extract.java`、`build_vanilla_data.py`）はこのツリーにあります。ただし、
「生成器を手元で実行し、全テストが緑になること」は確認していません（NOT RUN。jar・Java 21・EULA同意が必要）。

### 2.3 テストが必要とする資産

| 確認 | 必要なもの |
|---|---|
| 通常のビルド・データ不要の unit テスト | 指定の Rust ツールチェーンと依存クレート。Mojang の生成データは不要 |
| vanilla データ照合 | §2.2 の生成データと各 oracle 専用の fixture。必要な入力は対象テストと生成器で確認する |
| `tests/verify.sh` 全体 | 上記に加え、サーバーバイナリ、loopbackのwire試験、`depsgx`（層ゲート）、時間 |
| `--features native-oracle` のテスト | 固定jarから作ったnative fixture。無いと**失敗する**（skipしない）。CIの `p1-native-oracle.yml` がjarを取得して実行する |
| ゲームlistenerの手動確認 | ローカル生成データとビルド済みバイナリ。外部公開はしない |

テストの段と前提クラスは [tests/README.md](../tests/README.md)、道具の入口は
[tools/README.md](../tools/README.md) にあります。どのコマンドを走らせるかは
[tests/verify.sh](../tests/verify.sh) が正本です。

### 2.4 ローカル生成物と公開ツリーを分ける

`main` 自体がクリーンルームの公開ツリーです。追跡の有無は `git ls-files`、生成済み datapack の検査は
`tools/vanilla_data/gen_vanilla_data.sh --verify` で確認できます（jar 不要）。

```sh
tools/export_public_branch.sh --out ../megu-public --verify
```

このコマンドは HEAD の追跡ファイルから、D5 対象の datapack・状態表・4 TSV・パケットを除いた
単一コミットの別リポジトリを作ります。出力先は開発ツリーの外の新規または空ディレクトリにしてください。
既定のブランチ名は `public-export`。`--from-worktree` はコミット前の検査専用で、索引にあるパスの作業コピーを読みます。
元の履歴は変更せず、push もしません。候補では `--strict-public` が通り、再生成前の `dev_check.py` は
生成入力の不足を報告することがあります。実サーバーや vanilla データ照合を動かすには §2.2 に従ってください。
この検査は他の fixture や画像等の出所を認定しません。[LICENSING.md](../LICENSING.md) §5 の確認が別途必要です。

貢献者へのルール:

- これらのパスを `git add -f` で追加しない。新しいMojang由来データ・jar・マッピングを持ち込まない。
- 既存の追跡データを「整理」のために削除しない。証跡であり、削除・公開の判断はオーナーの権限。
- 生成器を回して差分が出た場合はコミットせず、変更内容と生成条件をissueに書く。
- 食い違いを見つけたら、[CONTRIBUTING.md](../CONTRIBUTING.md) の「データと来歴」に従って報告する。ここでライセンスを決めない。

## 3. ビルドと起動

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
MEGU_PORT=25699 cargo run --locked
```

- `--test-threads=1` はテストが共有する生成ディレクトリ・環境変数を壊さないためです。
- 起動すると `world_overlay.bin`（保存）をカレントディレクトリに書きます。
- listenerは既定で `127.0.0.1` にbindします。公開bindは明示的な環境変数と覚悟が要ります（[SECURITY_NOTES.md](../SECURITY_NOTES.md)）。外部ネットワークに出さないでください。
- 起動モードのまま診断用の環境変数だけ渡すと、出力を待ち続けることがあります。診断は各ツールの想定モードで実行してください。

## 4. テストの段階

成功の意味を段階ごとに分けます。**T0/T1が緑でも、T2/T3やvanilla一致の代用になりません。**

最初に `python3 tools/dev_check.py` を実行してください。ビルドもダウンロードもせず、このチェックアウトで
何が動き何が欠けているか、次に打つコマンドを数秒で出します。

### T0: Pythonだけで走る確認（jar・JDK・Cargo不要）

追跡済みのfixtureだけを読み、サーバーもCargoも起動しない確認です。一覧はスクリプトが正本なので
ここに写しません。

```sh
grep -E '^python3 tests/' tests/verify.sh
```

- 前提は python3 >= 3.9 だけです。`tests/layers_gate_test.py` だけは追加で `depsgx` を要求し、
  無いと skip せず fail します（T2の層ゲートと同じ検査器）。
- この一覧を tracked-only の export（`git archive HEAD`）で走らせると、`layers_gate_test.py` 以外は
  exit 0 でした。`java`/`javac`/`cargo`/`rustc` を 127 で落とした PATH でも同じ結果でした。
  測定した件数・所要時間・生ログは
  [contributor-readiness-2026-11-16](../evidence/review/contributor-readiness-2026-11-16.md)。
- 現行 CI のデータ生成不要の高速ゲートは `python3 tools/run_fast_gates.py` でまとめて実行できます。

### T1: Rustのunit／回帰

```sh
cargo test --locked -- --test-threads=1
cargo test --locked ingress_guard -- --test-threads=1   # 例: モジュール単位の絞り込み
```

全体の `cargo test` は、このガイドの作業では未実行です（NOT RUN。ビルド予算）。
依存crateの取得（`cargo fetch --locked`）は成功しています。実行した範囲は
[検証記録](../evidence/review/contributor-readiness-2026-11-16.md) にあります（persistence の絞った filter のみ）。
実行する場合は、追跡データの有無と、失敗がデータ由来かコード由来かを切り分けてください。

### T2: ローカル全体ゲート

```sh
bash tests/verify.sh
```

pythonの契約テスト、層ゲート、debt ratchet、直列cargo test、8スレッド×3回、loopbackのwire smoke、
Java adapterの `cargo check`、mutation（ゲートが赤くなれることの確認）を順に実行します。
時間がかかり、`depsgx`（固定revision。`.github/workflows/ci.yml` の `GXKIT_REV`）が必要です。
`bash tests/verify.sh` 全体は、このガイドの作業では未実行です（NOT RUN）。**部分実行の緑を `verify.sh` の代用にしないでください。**

### T3: hosted CI と native 照合

- `.github/workflows/ci.yml` は `main` へのPRと一部pushで走ります。ローカルの代用にはなりません。
- `.github/workflows/audit.yml` は `cargo-deny`（advisory・license・bans・sources）、`gitleaks.yml` は秘密情報の検査です。
- `p1-native-oracle.yml` は固定jarとJava 21でnative照合を行います。対象ブランチは限られ、`workflow_dispatch` でも起動できます。手元で回す場合は `tools/native_cargo_gate.py` と各oracleの引数を読んでください。jarの取得・EULA同意を自動化しないでください。
- T2の層ゲートは `tools/check_layers.py` が検査器の同一性を確認します。`depsgx` が無い環境では実行できないため、`MEGU_DEPSGX` に固定ビルドを指定します。

## 5. ソースの歩き方

`src/` はフラットなファイル構成です。**層の名前を発明せず**、実在する境界とownerを示します。
依存の許される向きは [evidence/foundation/module-layers/policy.json](../evidence/foundation/module-layers/policy.json) が正本で、
`tools/layers_check.sh` が検査します。

| 責務 | 主なowner | 変更時の注意 |
|---|---|---|
| 起動・フレーム受信・tick・パケット分岐 | `src/main.rs`（大きい。`game_tick` / `main`）、`src/ingress_guard.rs`、`src/chunk_ack.rs`、`src/connection_limits.rs` | 影響範囲が広い。まず小さい関数とテストを足す |
| 地形worldgen（density function・noise・carver） | `src/worldgen.rs`、`src/generation_math.rs`、`src/generation_providers.rs`、`src/generation_leaf_queue.rs`、`src/world_rng.rs` | 生成stageと入力順が契約。数値が動かない変更を「棄却」と読まない（[DESIGN.md](../DESIGN.md#比較の約束)） |
| 生成コンテキストとfeature | `src/generation_context.rs`、`src/feature.rs`、`src/tree.rs`、`src/flowing_fluid.rs`、`src/generation_payload.rs`、`src/generated_content.rs` | `Gen` の可変borrowと上書きの境界。featureの順序・RNG消費を変えない |
| 構造物 | `src/structure_pipeline.rs`、`src/structure_placement.rs`、`src/structure_template.rs`、`src/structure_transform.rs`、`src/jigsaw_structures.rs`、`src/template_structures.rs`、`src/single_piece_structures.rs`、`src/stronghold_structures.rs`、`src/end_city_mansion.rs`、`src/fortress_monument.rs`、`src/mineshaft*.rs` | 一部は `include!` で別ファイルから合成されます（例: `structure_pipeline.rs` は `feature.rs` から）。構造物の変更はシンボルで検索してください。 |
| レジストリ・データ読み込み | `src/block_registry.rs` | `assets/*.json` と `assets/*_1211.bin` の両方に依存 |
| 実行時状態・永続化 | `src/tick_queue.rs`、`src/scheduled_ticks.rs`、`src/tick_persistence.rs`、`src/nbt.rs`、`src/chest_loot.rs`、`src/minecart.rs`、`src/spawner.rs`、`src/effects.rs` | 保存・再読込とtick順序を対で確認する |
| Java／Mod境界 | `src/java_compat.rs`、`src/landing.rs`、`compat/` | jarは取得のみ。同梱しない |

テストはownerの隣にあります（同じファイルの `#[cfg(test)] mod tests`、または `src/*_tests.rs`）。
提案段階の再設計（root reform の chunk ownership 案など）は [specs/README.md](../specs/README.md#このページの読み方)
を参照してください。現行の境界は上の表と `policy.json` です。

## 6. 一つの変更を追う（worked example）

例として「プレイヤー移動フレームの拒否条件を変える」を追います。実装済みの拒否（非有限値・world border外）を
別の条件に広げる／狭める、という小さな変更です。

1. **発見**: シンボルで探す。

   ```sh
   rg -n "valid_movement_frame|MAX_PLAY_FRAME" src
   ```

   `src/ingress_guard.rs` に定義があり、`src/main.rs` のフレーム読み取りと移動パケット分岐から呼ばれています。
2. **ownerを決める**: フレーム長と移動値の検証は `src/ingress_guard.rs`。ファイル末尾の `mod tests` に
   境界ケース（正常、非有限、world border外、短い/パディング、他パケット）があります。
3. **変更する**: 定数または検証条件を変え、**同じ場所に境界ケースを1つ足す**。`main.rs` の分岐は契約が変わらない限り触りません。
4. **速く確かめる**:

   ```sh
   cargo test --locked ingress_guard -- --test-threads=1
   ```

   サーバーも生成データも要らないunitテストです。このガイドの作業では未実行です（NOT RUN。ビルド予算）。
5. **経路を確かめる**: wireの振る舞いが変わるなら、ビルド後に
   `python3 tests/wire_smoke.py target/debug/megu`、または `bash tests/verify.sh`。
   `tests/protocol767_test.py` はCLIデコーダのknown-answer検査で、サーバーの受信経路を実行しないため**証明になりません**。
6. **提出する**: 実行したコマンドとexit、実行していない段階、変更したテスト名をPRに書く。
   「一部のテストが緑」を「全体合格」と書かない。

## 7. コミット前

```sh
git status                 # 生成データ・jar・秘密情報が混ざっていないか
git diff --check           # 空白エラー
git diff --stat
git add <path>...          # pathspecで明示。git add -A/-u は使わない
git commit -s -- <path>... # DCO。共有indexの他人の変更を巻き込まない
```

- 生成物、`*.jar`、`*.class`、マッピング、`world_overlay.bin`、`.env` をコミットしない。
- 変更したdocの相対リンクとアンカーを確認する。
- 検証していないことを「確認済み」と書かない。実行しなかったコマンドは、依存（jar、EULA、depsgx、時間）とともに残す。
