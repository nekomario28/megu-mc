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
| Python | `python3` >= 3.9（`str.removeprefix` を使う道具がある） | fixture 段が動かない |
| 層ゲート | `depsgx`（CI が `.github/workflows/ci.yml` の `GXKIT_REV` を固定して入れる。`MC_SPINE_DEPSGX` でも可） | `layers_gate_test.py` と `tools/layers_check.sh` は**失敗する**（skip にしない契約）。他は動く |
| Rust | 固定 toolchain（`rust-toolchain.toml` の 1.98.0）と `Cargo.lock` の crate（初回だけ取得が要る） | Rust 段・wire 段が動かない |
| JDK | JDK 21（`java`/`javac`） | jar 側の oracle/capture だけが動かない |
| 固定 jar | 1.21.1 server jar（sha1 固定、**再配布しない**。各自が用意する） | 参照を**作り直す**段だけが動かない。保存済み参照との照合は動く |
| EULA 同意 | `config_packets.bin` の再生成だけ `--accept-eula` が要る | その1ファイルの再生成だけが動かない。同意は自動で取らない |

## 段（どこまでを確かめるか）

| 段 | 確かめること | 前提 | 入口 |
|---|---|---|---|
| fixture 段（Python） | tracked fixture との照合、gate の exit contract、落ちられない gate の負の制御 | Python のみ（`layers_gate_test.py` だけ depsgx） | `grep -E '^python3 tests/' verify.sh` が一覧 |
| 層・負債 | `src/` の依存方向、数えた負債が増えていないこと | depsgx（層のみ） | `layers_gate_test.py`、`tools/debt_ratchet.sh` |
| Rust | 生成・構造物・プロトコルの実装 | 固定 toolchain と crate | `cargo test --offline --locked -j 2 -- --test-threads=1` |
| wire / 実プロセス | loopback の実通信（spawn chunk 生成→play） | ビルド済み binary | `tools/cargo_executable.py` で binary を取り、`wire_smoke.py` 等へ渡す |
| native oracle / capture | 固定 jar の実クラスを実行して参照と照合する／参照を作り直す | jar、JDK 21、（`config_packets.bin` は）EULA 同意 | `tools/*_oracle.py`、`tools/sculk_capture.sh` 等 |

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

## 測った現在地（base `e7ead8e`）

tracked-only の checkout（`git archive HEAD` を temp に展開）で、`verify.sh` の Python 段を
そのまま走らせた結果:

- `layers_gate_test.py` **以外は全て pass**。`java`・`javac`・`cargo`・`rustc` をすべて 127 で
  落とす `PATH` でも同じ結果だったので、この段は JDK にも Cargo にも依存しない。
- `layers_gate_test.py` は depsgx が無いため fail（「ツール不在は skip ではない」という契約どおり）。
  depsgx を入れた環境では同じ段が通る。
- Rust・wire・native の各段は tracked-only で **NOT RUN**。ここに「clean clone で全部 green」とは
  書かない（測っていない）。追跡データで足りるかは `git ls-files` と
  `tools/dev_check.py` の `build_inputs`/`runtime_data` が答える範囲までしか主張しない。

`assets/block_states_1211.json` のように、生成器の出力でも追跡されていないものがある
（`git ls-files` が唯一の正）。無いと困るのは `tools/asset_coverage.py` と再生成だけで、
ビルドと上の fixture 段は tracked のままで動く。

## 走らせるときの前提

- **cwd はリポジトリ root**。`src/` は `assets/`・`datapacks/`・`config_packets.bin` を
  相対で開くので、別の場所から呼ぶと「データが無い」失敗になる。
- `verify.sh` は `PYTHONDONTWRITEBYTECODE=1` を立てる。手で回すときも同じにすると
  ツリーに `__pycache__/` が増えない。

## この preflight 自身のテスト

    python3 tests/dev_check_test.py

hermetic（temp の合成ツリー・偽の `rustup`/`javac`/`cargo`・実 JDK/Cargo/jar 不要）。
固定しているのは preflight の status/exit 契約、欠けた入力を赤にできること、cwd を変えても
リポジトリを掴むこと、ツリーを書き換えないこと、`cargo` を起動しないこと。
`verify.sh` への組み込みは未実施（親が判断する）。
