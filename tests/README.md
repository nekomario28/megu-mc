# tests

このディレクトリは「何を、どの前提で走らせるか」を持つ。コマンドの正規の一覧は
[`verify.sh`](verify.sh) が持つ（コミット前に通す唯一の手順で、段の順序もそこにある）。
ここは段と前提クラスを説明し、一覧を写さない。

まず何が動くかを知りたいときは `python3 tools/dev_check.py`
（ビルドもDLもせず、`OK`/`MISSING`/`NOT_CHECKED`/`ERROR` と次に打つコマンドを出す）。
道具の案内は [tools/README.md](../tools/README.md)。

## 前提クラス

| クラス | 何が要る | 無いとどうなるか |
|---|---|---|
| Python | `python3` >= 3.9（`str.removeprefix` を使う道具がある） | T0 が動かない |
| 層ゲート | `depsgx`（CI が `.github/workflows/ci.yml` の `GXKIT_REV` を固定して入れる。`MC_SPINE_DEPSGX` でも可） | `layers_gate_test.py` と `tools/layers_check.sh` は**失敗する**（skip にしない契約）。他は動く |
| Rust | 固定 toolchain（`rust-toolchain.toml` の 1.98.0）と `Cargo.lock` の crate（初回だけ取得が要る） | T1・T2 が動かない |
| JDK | JDK 21（`java`/`javac`） | jar 側（T3）の oracle/capture だけが動かない |
| 固定 jar | 1.21.1 server jar（sha1 固定、**再配布しない**。各自が用意する） | 参照を**作り直す**段だけが動かない。保存済み参照との照合は動く |
| EULA 同意 | `config_packets.bin` の再生成だけ `--accept-eula` が要る | その1ファイルの再生成だけが動かない。同意は自動で取らない |

## 段（T0–T3）

段の意味と何を確かめるかは [開発ガイド §4](../docs/development.md#4-テストの段階) が正本です。
ここは**各段の前提クラス**だけを持ち、正規のコマンドは [verify.sh](verify.sh) が持ちます。

| 段 | 前提クラス |
|---|---|
| T0 Python の契約・fixture | Python（`layers_gate_test.py` だけ depsgx）|
| T1 Rust unit／回帰 | 固定 toolchain と `Cargo.lock` の crate |
| T2 `tests/verify.sh` | T0・T1 に加えてビルド済み binary（wire）、depsgx（層）、時間（mutation と 8スレッド×3回）|
| T3 hosted CI／native oracle | 固定 jar、JDK 21、（`config_packets.bin` は）EULA 同意。ローカルの代用にならない |

`gate_scripts.py` のように**偽のバックエンド**を temp に置くテストがある。あれが固定するのは
gate の配線と exit contract であって、実ツールの正しさではない。実ツール側は
`native_cargo_gate.py`（filter が0件でも cargo は exit 0 を返すので「実行されたこと」を別に要求する）
と `tools/mutation_check.sh`（実装を壊して赤を要求する）が担う。

## native 段を新人の smoke にしない理由

- 固定 jar は各自が用意する入力で、再配布しない。EULA 同意も自動では取らない。
- 1 回が分単位で、生成物も重い。手元の確認としては割に合わない。
- 「参照と一致するか」は Python 段が**保存済み参照**で既に確かめられる。jar が要るのは
  **参照を作り直す**ときだけ。この2つ（保存済み参照の段／生成段）を混ぜると、
  「jar が無いから何も確かめられない」という誤解になる。

## 測った現在地（この候補の、追跡ファイルのみの export）

`git archive HEAD` を temp に展開した tracked-only の checkout で、`verify.sh` の Python 段
（`grep -E '^python3 tests/'` の一覧）を上から順に走らせた結果:

- `layers_gate_test.py` **以外は全て exit 0**。`java`・`javac`・`cargo`・`rustc` をすべて 127 で
  落とす `PATH` でも同じ結果だったので、この段は JDK にも Cargo にも依存しない。
- `layers_gate_test.py` は depsgx が無いため fail（「ツール不在は skip ではない」という契約どおり）。
  depsgx を入れた環境での同じ段は、このホストでは**未測定**（CI は固定 revision を入れて回している）。
- `cargo test` 全体・wire・native の各段は **NOT RUN**。ここに「clean clone で全部 green」とは
  書かない（測っていない）。追跡データで足りるかは `git ls-files` と
  `tools/dev_check.py` の `build_inputs`/`runtime_data` が答える範囲までしか主張しない。
  persistence の絞った filter（`tick_persistence::tests` と `persistence_tests::`）だけは
  統合後の checkout で実行済み（[検証記録](../evidence/review/contributor-readiness-2026-11-16.md)）。

件数・所要時間・生ログは
[contributor-readiness-2026-11-16](../evidence/review/contributor-readiness-2026-11-16.md) にあり、
このページには写しません。

`tests/data/biome_probe_1211.tsv` や `tests/data/vanilla_vectors_1211.tsv` のように、生成器の出力でも追跡されていないものがある
（`git ls-files` と `tools/provenance_check.py --strict-public` が正）。`assets/block_states_1211.json` は `762f312` で jar 生成出力に切替済みで、ビルドと T0 は tracked のままで動く。

## 走らせるときの前提

- **cwd はリポジトリ root**。`src/` は `assets/`・`datapacks/`・`config_packets.bin` を
  相対で開くので、別の場所から呼ぶと「データが無い」失敗になる。
- `verify.sh` は `PYTHONDONTWRITEBYTECODE=1` を立てる。手で回すときも同じにすると
  ツリーに `__pycache__/` が増えない。

## この preflight 自身のテスト

    python3 tests/dev_check_test.py

hermetic（temp の合成ツリー・偽の `rustup`/`javac`/`cargo`・実 JDK/Cargo/jar 不要）。
固定しているのは preflight の status/exit 契約、route の READY がその route の必須検査から
導かれること（include の欠落は toolchain があっても Rust を NOT READY にする）、
`javac -version` の解析（空・雑音・要求不明は NOT_CHECKED）、欠けた入力を赤にできること、
cwd を変えてもリポジトリを掴むこと、ツリーを書き換えないこと、`cargo` を起動しないこと、
壊れた `MC_SPINE_DEV_CHECK_TIMEOUT_MS` が exit 2 になること。
`verify.sh` への組み込みは未実施（親が判断する）。
