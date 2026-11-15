# Horizon のアーキテクチャ・ツール調査と mc-spine のビルド・テスト時間（2026-11-16）

この文書は 2 部構成である。第 1 部（4〜6 節）は Horizon の**設計と道具**を読み、
mc-spine に写像できる形にした調査記録。第 2 部（3 節）は mc-spine 自身の
`cargo test --bin mc-spine` が 623.72 s かかっていた問題に対する、**測定済みの**変更。

実行結果の機械可読な控え（コマンド、各アームの ms、gate の結果、限界）は
[architecture-survey-20261116.run.json](architecture-survey-20261116.run.json)。

正本の関係: 借りる/借りないの判断表と参照 revision の来歴は
[horizon-reuse/README.md](README.md)、gameplay の移植順序は
[specs/horizon-gameplay](../../../specs/horizon-gameplay/README.md)、
並列化の目標と不変条件は [specs/chunk-parallelism](../../../specs/chunk-parallelism/README.md)。
この文書はその 3 つを置き換えず、**上流のコードを 1 行も取り込まずに読める設計情報**と、
このホストでの実測値だけを足す。

## 0. 何を主張し、何を主張しないか

主張する:

- Horizon の差分ハーネス 4 点（`port_ledger.py` / `rea_vanilla.py` / `parity_reach.py` /
  `parity_shim.py`）が何を入力に何を出し、どこで推測を諦めているか。
- mc-spine の `Cargo.toml` に 2 つの profile 設定を足したときの、ビルド時間・
  テスト実行時間・成果物サイズの実測値（同一ホスト、同一コマンド）。
- WorldGen・tick・storage について、Horizon が**どういう primitive で解いているか**、
  および mc-spine の現行コードとの差分。

主張しない:

- Horizon のテスト suite を実行していない（26.3 の jar が手元にない）。
  以下の記述はすべて**ソースを読んだ結果**であり、その動作を実行して確かめたものではない。
- 上流 README の「281 harnesses」「100%」を検証していない。
- 並列化（P3-2）を実装していない。設計の写像を書いただけである。
- `tests/verify.sh` の end-to-end 時間を測っていない。測ったのは
  `cargo test --offline --locked --bin mc-spine -- --test-threads=1` の 1 本である。

## 1. 参照 revision とライセンスの移動

| 項目 | 値 |
|---|---|
| repository | `https://github.com/horizon-rust/horizon`（既存 README の `ludovic111/horizon` は移転後の同一 repo。`0564ab0` も同じ URL で解決する） |
| 調査した HEAD | `4626921`（2026-11-16 clone、`--depth 1`） |
| 従来の donor revision | `0564ab0bcd3235daa6001033261b79aef5f81de1` |
| ライセンス移動 | `fb1135c6020b`（2026-11-15T20:37:34Z）"docs: change Horizon's licence from MIT to LGPL-3.0-or-later (#17111)" |

確認した内容:

- `0564ab0` の `LICENSE` は `MIT License / Copyright (c) 2026 Ludovic`、`Cargo.toml` は
  `license = "MIT"`。既存 README と `third_party/licenses/horizon.txt` の記述と一致する。
- HEAD の `LICENSE` は LGPL-3.0、`[workspace.package] license = "LGPL-3.0-or-later"`。
  `LICENSES/` に `GPL-3.0.txt` と `MIT.txt` が併置されている。
- **crate 単位では一律ではない**。`crates/horizon-core/Cargo.toml` は
  `license.workspace = true`（= LGPL-3.0-or-later）で、`crates/horizon-addresses` /
  `horizon-bench` / `horizon-cosmetics` は明示的に `LGPL-3.0-or-later`。

この調査で重要な帰結: この文書が一番参考にしたい 3 つ —
`horizon-core/src/paletted.rs`、`horizon-core/src/storage.rs`、`horizon-core/src/ticks/` —
は、いずれも **LGPL 側の crate** にある。したがって「MIT 時代の revision からなら写せる」
という逃げ道は、この 3 つには使えない（`0564ab0` の `Cargo.toml` も `MIT` だが、
写すなら revision を固定してその時点のライセンスで扱う話であり、本調査は写さない）。

本調査の運用:

- `/tmp/horizon-survey` に `--depth 1 --filter=blob:none --sparse` で clone し、
  読むためだけに使った。調査後に削除する。
- mc-spine のツリーに入れた Horizon 由来のファイルは**0 件**。以下の設計記述は
  すべて散文で書いた clean-room の写像であり、コードの転写ではない。
- 引用は識別子と 1〜2 行のコメントに限る（何を読んだかの同定のため）。

## 2. 測定環境

| 項目 | 値 |
|---|---|
| host | 16 logical CPU / 30 GB RAM / swap 30 GB |
| toolchain | `rustc 1.98.0`（`rust-toolchain.toml` の pin どおり）/ `cargo 1.98.0` |
| worktree | `perf/worldgen-parallel` @ `e7ead8e` |
| build dir | worktree の外（`CARGO_TARGET_DIR`）に profile ごとに 1 つ。`target/` は使っていない |
| `TMPDIR` | `/tmp` が別レーンで満杯だったため、ディスク上に固定（`cc` の一時ファイルが tmpfs を食い、`ring` の build script が `No space left on device` で落ちた）。測定はすべて同じ `TMPDIR` で取った |
| 実行隔離 | 各測定を `systemd-run --user --scope -p MemoryMax=14G` で 1 本ずつ直列に実行 |
| 事前 gate | `resource_gate.py --class light --need-mem-gb 8` が GO のときだけ開始 |

この repo の [chunk-parallelism](../../../specs/chunk-parallelism/README.md) は
「end-to-end は同じバイナリでも 11.9-29.4 s と振れる。wall clock の A/B では 20% 級は
分離できない」と記録している。以下の数値はその規律に従い、**同一ホスト・同一コマンド・
同一 `TMPDIR`・直列**で取り、差が 20% 級に収まる比較は主張しない。今回の差は 85% で、
20% 級の分解能の問題にはならない。

## 3. mc-spine のテスト実行時間（実測）

### 3.1 変更したもの

`Cargo.toml` に 2 つだけ足した（`tests/verify.sh`、gate、テスト、`policy.json`、
`deps_target.py` は触っていない）。

```toml
[profile.dev]
debug = "line-tables-only"

[profile.test]
opt-level = 1
```

`debug-assertions` と `overflow-checks` は dev/test profile の既定のまま**有効**である。
`opt-level` は「最適化する/しない」だけを変え、panic-on-overflow や `debug_assert!` を
消していない。この suite は `PoisonError` や overflow を検出対象にしているので、
そこを黙って切る変更は入れていない（切っていれば結果行の内訳が変わるはずで、変わっていない）。

### 3.2 測定値

3 通りを同一手順で測った。`A` は `debug = "line-tables-only"` のみ、`B` が上記 2 つ。

| 項目 | 変更前 | A: line-tables のみ | B: A + `[profile.test] opt-level = 1` |
|---|---|---|---|
| cold build `cargo test --no-run` | **12.96 s** | 11.10 s | **22.99 s** |
| suite 実行 `--test-threads=1` | **623.72 s** | 561.03 s | **92.14 s** |
| テストバイナリ | 142,764,760 B (136.1 MiB) | 74,511,248 B (71.1 MiB) | 74,644,504 B (71.2 MiB) |
| target dir | 673 MB | 464 MB | 571 MB |
| 結果行 | 530 passed / 0 failed / 1 ignored | 同左 | 同左 |

- suite 時間: **623.72 s → 92.14 s（6.77 倍、−85.2%）**。
- ビルド時間: **+10.03 s**（12.96 → 22.99 s、cold）。テスト実行の短縮 531.6 s に対して
  1.9% の前払いである。
- バイナリ: **−68.1 MB（−47.8%）**。
- ignored は 1 件のままで、`#[ignore]` を追加していない。テストの集合も変わっていない
  （`530 passed` が 3 条件で一致）。

内訳（何が効いたか）:

- A（サイズだけ）: **−62.7 s（−10.0%）**。`src/test_support.rs` の `isolated_world_test` は
  `current_exe()` を `--exact <name>` で起動し直す。suite 全体で子プロセス起動は 15 回、
  子の自己申告時間の合計は約 33 s。136 MiB のバイナリを起動ごとに page-in し直す費用が
  そこに乗っていた。同じ理由でリンク時間も縮んでおり、cold build が
  12.96 → 11.10 s（−1.86 s）になっている。
- B（`opt-level = 1`）: さらに **−468.9 s**。suite は計算律速で、
  1 テストあたり平均 1.17 s（623.72 / 531）が 0.17 s になった。最適化なしの
  worldgen/surface/aquifer の内側ループが支配的だったということである。
  子プロセス起動は 15 回・合計 33 s しかないので、A で説明できる上限はここまでで、
  残りは in-process の計算そのものである。
- ビルドが 12.96 s しかないのに suite が 623.72 s だった、という非対称が
  「コンパイル時間を削る」方向の投資を無意味にしていた。削るべきは実行時間だった。

### 3.3 再測定（同じ条件で 2 回目）

上の 3 列は profile を 1 段ずつ切り分けるために別々に取った。主張の再現性を見るため、
変更前と変更後を**同じ script で連続して**もう一度測った（両アームとも同じ `TMPDIR`、
同じ `systemd-run` 隔離、間に他の測定を挟まない）:

| 項目 | 変更前 | 変更後（`line-tables-only` + `opt-level = 1`） | 比 |
|---|---|---|---|
| suite 実行 `--test-threads=1` | **622.53 s** | **92.53 s** | **6.73 倍**（−85.1%） |
| テストバイナリ | 142,764,760 B | 74,644,504 B | −47.7% |
| 結果行 | 530 passed / 0 failed / 1 ignored | 同左 | — |

2 回の測定のばらつきは、変更前が 623.72 s と 622.53 s（0.19%）、変更後が 92.14 s と
92.53 s（0.42%）。**同じ 531 テストの suite は wall clock が 1% 未満で再現する**。
[chunk-parallelism](../../../specs/chunk-parallelism/README.md) が記録している
「同じバイナリでも 11.9-29.4 s と振れる」は、spawn burst を含む**server の end-to-end**
の話であり、この suite には当てはまらない。したがって、この 6.7 倍という差は
20% 級の分解能の問題ではなく、そのまま主張できる。

（2 回目の測定中、別レーンが `/tmp` に native capture を書き続けていた。それでも
変更前が 0.19% で再現したので、上記の安定性は「ホストが空だったから」ではない。）

### 3.4 変更が壊さないことの確認

`opt-level` は最適化であって意味論の変更ではないが、**最適化は timing を変えるので、
並列で走るテストの競合が表に出る可能性がある**。この repo の suite は
`--test-threads=1` と `--test-threads=8` の両方で緑であることを要求している
（`tests/verify.sh`）ので、8 スレッドでも 1 回通した。

| 確認 | 結果 |
|---|---|
| `bash tools/layers_check.sh`（`PATH` に gxkit venv） | `layers check: PASS (current debt is named, not resolved)`、probe 4 件が期待どおり赤 |
| `bash tools/debt_ratchet.sh` | `debt ratchet: 0 over limit`（`ignored tests: 1` は不変） |
| `python3 tests/gate_scripts.py` | 15 件の self-test すべて PASS |
| `cargo test --offline --locked --bin mc-spine -- --test-threads=8` | **530 passed / 0 failed / 1 ignored、48.34 s** |
| `cargo check --offline --locked --features fabric-probe` | `Finished`（warnings のみ） |
| `cargo check --offline --locked --features neoforge-events` | `Finished`（warnings のみ） |

`tests/verify.sh` 自体は実行していない（suite 4 回 + mutation check で、この変更の
範囲を超える）。ただし `verify.sh` が走らせるのは
`cargo test -- --test-threads=1` と `--test-threads=8`（3 回）の**同じ bin の unit
tests** なので、そこで測られる時間は上の表の値に従う。CI（`.github/workflows/ci.yml`
の `cargo test --locked -- --test-threads=1`）も同じ profile を使う。
`p1-native-oracle.yml` は `--features native-oracle`（Java fixture 必須）で、この profile
でも同じ短縮が見込まれるが、**手元に jar が無いので測っていない**。

### 3.5 副次的に分かったこと

- テストバイナリのファイル名ハッシュが profile ごとに変わる（`mc_spine-d4e3d0377ac7e8f8`
  → `mc_spine-381af6bc07711f9b` → `mc_spine-5263732b84b8ac33`）。パスを決め打ちする
  道具は無く、`tools/cargo_executable.py` は cargo の JSON から解決している。この点は
  変更で壊れない。
- `tools/cargo_executable.py` は `target.get("name") == "mc-spine"` かつ
  `not event["profile"]["test"]` で**非テスト**の bin を選ぶ。`[profile.test]` を足しても
  この区別は保たれる（`cargo build` は dev profile のまま）。
- `debug = "line-tables-only"` は backtrace の file:line を残す。完全な debug info を
  捨てるのは、この suite が「backtrace から変数を読む」種類のテストを持たないため。
  その前提が崩れる変更（例: 失敗時にローカル変数をダンプする gate）を入れるなら、
  この設定を見直す対象になる。

## 4. 差分ハーネスと method ledger（依頼 a）

### 4.1 上流にある 4 つの道具

`tools/class_index.py`（327 行）が土台である。純 Python の classfile parser で、
クラスごとに super/interfaces/fields/methods を取り、メソッドごとに**バイトコードの
ハッシュ**を作る。このとき定数プール参照は「何を指すか」に、分岐先は命令 index に
正規化するので、定数プールの並び替えや `ldc`→`ldc_w` のような再コンパイル差では
ハッシュが変わらない。`lambda$name$N` は `lambda$name` に畳む（無関係な変更で
lambda の採番がずれても「変わった」にしない）。

**(1) `tools/rea_vanilla.py`（114 行）— bytecode への問い合わせ口**

REA（headless JADX）に対する薄い CLI で、`classes` / `class` / `method` / `callers` /
`refs` の 5 コマンドだけを持つ。要点は 3 つ:

- 答えは**出荷バイトコード**から取る。decompiled source ではない。lambda・bridge・
  overload の folding で source からは消える呼び出しが、ここでは残る。
- `--overload-index` で overload を名指しできる。同名メソッドの取り違えを
  「たぶんこれ」で済ませない。
- JADX の jar を sha256 で pin し、jar を `.apk` 名のコピーとして渡す
  （REA は JADX のターゲットを `.apk` 名で受けるため、「見つからなかった」を
  「無い」と読み違えないように）。答えが `normalized_result` の形で返らなければ、
  message と issue を stderr に出して終了コード 1 で落ちる（空の結果を成功にしない）。

lambda を意識した call graph の実体は `port_ledger.py` 側にある（下記）。

**(2) `tools/port_ledger.py`（3133 行）— 双方向の method ledger**

vanilla の各メソッドと、それを移植した Rust の関数を**両方向に**対応づけ、
line span つきで保持する。移植の同定は 5 つの独立した信号の和:

1. doc コメントがメソッドを cite している（`Class.method` / `Class::method` / `Class#method`）
2. クラス名の file / impl の中に同名 fn がある
3. call graph の一致（移植側の callee と vanilla 側の callee が重なる）
4. クラスの home file と共有する state・call
5. 偶然は共有しにくいもの: 珍しい数値（`0.458`, `72000`）と game id
   （`SoundEvents.ENTITY_SHULKER_BULLET_HIT` ↔ `"entity.shulker_bullet.hit"`）

どれでも決まらないものは verdict map（`tools/port_map/*.tsv`）が答える:
`fn` / 折り込まれた先 / `missing` / 移植不要の理由。

lambda-aware な部分（`parse_class_bytes` 以降）:

- `lambda$X$N` を宣言メソッド `X` に畳む（`class_index.lambda_owner`）。
- bridge（`access & 0x40`）を捨てる。synthetic（`access & 0x1000`）と enum の
  `values`/`valueOf`/`$values` も捨てる。
- `invokedynamic`(opcode 186) の bootstrap 引数から `h<N>:owner.method(` を拾って
  **呼び出しとして足す**。`Foo::bar` というメソッド参照だけの経路でも、
  「bar が呼ばれている」ことが graph に出る。
- 3 命令以下の非 lambda 本体を accessor として印を付け、単体の移植対象から外す。

その上で 2 つの call graph を突き合わせ、8 種類の**gap** を出す:

| gap | 意味 |
|---|---|
| `seam` | 同じ vanilla メソッドが 2 本の経路（共有コード / server-client 側 / network 側）で移植され、片方だけ callee の移植に届く |
| `callee` | 移植済みの vanilla メソッドの callee に、どの Rust からも届かないものがある |
| `client` | client の copy だけが起こす効果（`addParticle` / `playLocalSound` / `itemUsed` / `isClientSide()`）の移植に、server-client 側から届くものがない |
| `class` | package の大半は移植済みなのに、クラス全体を移植したものが無い |
| `float` | vanilla が double に widening する float リテラル（`0.3F`）を、double の `0.3` と書いている |
| `drift` | vanilla が使うもの（sound・particle・item・block・effect・game event・珍しい数値）を移植が一度も名指ししていない |
| `stale` | cite 先のクラスは在るがメソッドが無い（rename 候補）。pin した版に無いクラスは報告しない |
| `unported` | 移植の無い vanilla メソッドを、vanilla 側の被呼出し数で順位付け |

**(3) `tools/parity_reach.py`（865 行）— dep-info の到達可能性による絞り込み**

Cargo の dep-info は「probe がリンクする crate の全 source」を並べる。そのため
`horizon-client` のどこかを 1 行変えると、`horizon-client` をリンクする全 harness が
選択されてしまう。この道具は harness の依存 file 集合を、**entry point から到達できる
item を持つ file** に絞る。graph の node は item（fn/struct/enum/union/trait/type/
const/static/impl/macro）で、規則は docstring に列挙されている:

- entry（走った probe module、build した binary）の中は全部到達。module tree が
  置かない Rust file（`include!` された source）も全部到達。
- module は、到達した code を持つか、到達した code から path で名指しされていて
  親が open なら open。library の root は、到達した code が crate 名
  （`horizon_world`）を名指しした時点で open。
- free item は、自分の module 以下で名前が出たとき／module が open で path・import で
  名指しされたとき／glob import されているとき。
- inherent impl のメソッド・定数は型が到達した後で 1 つずつ名前で。trait impl は
  trait と型が到達した時点で。macro は invoke されたとき。`pub use` は import する
  もの全部に名指しされたのと同じ。
- `#[cfg(test)]` / `#[test]` の下は除く（probe は test build ではない）。
- **名前は綴りで照合するので、意味していない名前で file を残しうる。graph は
  over-approximate するだけ**、と明記されている。

**(4) `tools/parity_shim.py`（229 行）— oracle 出力の content-addressed cache**

`tools/parity.py` が `java` / `javac` への symlink を PATH の先頭に置き、この script が
自分自身を「本物の java の前の薄い層」として挿す。run の同一性は
「command line + stdin が file ならその中身 + 本物の program の実体」で決める。
成功した run について、引数が名指す path（classpath / sourcepath を含む）の下で
**作られた/変わった file** を output（size と mtime）、それ以外を input（content hash）
として記録し、stdout / stderr も保存する。次の同じ command は、

- input の content が全部同じ
- 隣に file が増えていない
- output が置いたまま

のときだけ記録を再生する。**pipe から stdin を読む run と失敗した run は絶対に
cache しない**。`PARITY_JAVA_CACHE=0` で無効化。hash は sqlite に
(size, mtime, sha256) で持ち、並列 harness で共有する。file を増やす wrapper が
自分を PATH に戻して無限に自分を起動する事故には `MAX_DEPTH = 4` の depth で落ちる。

この道具のいちばん良い点は性能ではなく、**「oracle を再実行しなかった」ことを
隠さない**ことである: 記録に残るのは「command と input の hash」であり、
`tools/parity.py --fresh` は `PARITY_JAVA_CACHE=0` で必ず本物を走らせる。

### 4.2 mc-spine に足すなら（clean-room 設計）

現状: mc-spine の `tools/` には 57 本の `*oracle*.py`、85 本の Java probe、
58 本が `vanilla_java` を経由する。Java の実行は
`tools/vanilla_java.py::run_probe`（javac + java）**1 箇所**に閉じている。
一方、dep-info を読む道具も、oracle 出力を再利用する仕組みも無い。
`cargo test --bin mc-spine` は 531 テストを毎回全部走らせる。

#### (A) dep-info による test/oracle 選択 `tools/test_reach.py` + `tools/test_select.py`

mc-spine は 1 crate・1 bin（66 ファイル）なので、Horizon の「crate 単位の選択」
（`tools/ci/test_scope.py`: 変更 file → その crate + それに依存する crate + その file を
実行時に読む crate）は、そのままでは効かない。全 crate が 1 つだからである。
効くのは **item 単位**の絞り込みで、Horizon の `parity_reach.py` と同じ規則を使う。

設計:

1. 根（roots）: `src/*.rs` の `#[test] fn NAME`。module path から harness の test 名
   （`feature::generation_contract_tests::sculk_cursor_...`）を作る。
2. node: item（`fn` / `struct` / `enum` / `trait` / `impl` の各メソッド / `const` /
   `static` / `mod` / `use` / `macro_rules!`）。edge: 本体の token 列に現れる名前。
   名前は綴りで照合し、**over-approximate する側に倒す**（Horizon と同じ判断）。
3. `git diff` の hunk を、item の span に写して「変更された item」を作る。
4. 各 test について、到達する item の集合と 3 の交差があれば選択。
5. **fail-open** を明示的に実装する。次は必ず全件:
   - Rust 以外の変更（`datapacks/`、`assets/`、`tests/data/`、`config_packets.bin`）
   - `Cargo.toml` / `Cargo.lock` / `rust-toolchain.toml`（**今回の profile 変更がまさにこれ**。
     profile を変えると何が走るかが変わるので、選択で縮めてはいけない）
   - graph が読めなかった file（`include!`、`#[path]`、解決できない `mod`）
   - 選択結果が空、または既知の「基準テスト」集合を含まない
6. 出力: 選んだ test 名と、その理由（どの変更 item から到達したか）、
   および「全 531 件中 N 件」を必ず印字する。

この repo の規律に合わせた境界:

- 選択は**追加の速いレーン**であり、`tests/verify.sh` の全件実行を置き換えない。
  P4-1 の合格条件は「同一 SHA の全 gate 完走」であって「変更に関係する gate 完走」
  ではない。選択が緑でも、それは P4-1 の緑ではない。
- 選択器自体に負の対照を付ける: `src/feature.rs` の 1 item を名指しで変えたとき、
  その item を使う test が選択に**入る**こと、無関係な test が**入らない**ことを
  1 本ずつ assert する。Horizon の `parity_reach.py` が「over-approximate するだけ」と
  書いているのは、この assert が無いと選択器が静かに取りこぼすからである。

#### (B) oracle 出力の cache `tools/oracle_cache.py`

Horizon は `java`/`javac` を PATH で横取りして cache する。mc-spine は
**Java 実行の owner が既に 1 つある**（`run_probe`）ので、PATH を触る必要はない。
2 つ目の owner を作らずに 1 箇所へ入れる:

- key = sha256( jar の SHA-1（`JAR_SHA1` で固定済み）/ probe の `.java` bytes /
  `extra_sources` の bytes / main class / args / classpath の content digest /
  `javac -version` と `java -version` の文字列 / cache format version )
- value = `{stdout, stderr, status}`。置き場は repo 既存の scratch 規約に合わせて
  `.local/oracle-cache/<key>.json`（`port_ledger.py` の `.local/port-ledger/` と同じ）。
- **cache しない**: 非 0 exit、空 stdout、`capture=False` の run、
  probe 自身の parse に失敗する stdout。
- **hit を隠さない**: `oracle-cache: hit <key>` を stderr に出す。evidence に記録する
  run（`record` 経路）と `tests/verify.sh` は `MC_SPINE_ORACLE_CACHE=0` で必ず実行する。
  この repo は「入力・結果の欠落、比較ゼロ、別版 jar は失敗」を契約にしているので、
  再生した値が新しい観測として evidence に入る経路を作ってはいけない。

見積りの根拠: Java の起動は 1 回あたり `javac` 60 s / `java` 120-300 s の timeout で
守られている（`run_probe`）。Rust 側だけを変えたときに毎回これを払っている。
Horizon の shim が狙うのも同じ点で、docstring に
"On a Rust-only change a harness then pays only for its Rust side." とある。

## 5. WorldGen の並列性とロック渋滞（依頼 b）

### 5.1 Horizon の構造

`crates/horizon-world/src/worldgen/generator.rs`（+ `worldgen/` の 90 エントリ、サブディレクトリを含む）で、
**可変状態を 3 層に分けている**。

1. **不変入力**: `GeneratorData` を `Arc` で共有。registry、biome source、structure
   resource、decoration、tags。
2. **per-worker 状態**: `generator.worker()` が `GeneratorWorker { pipeline, state:
   WorkerState }` を返す。`WorkerState` は sampler context（terrain / biome / carver）
   と、**worker 専用の cache** を持つ:
   `carver_biomes`、`structure_biomes`、`heights`、`surface_rule`（worker ごとに
   1 回コンパイル）、`timings`。コメントは
   "Seed/dimension-bound lookups avoid the shared mutex for overlapping carver
   neighborhoods. Bounded independently of the shared cache."
   —— 隣接 chunk が重なる lookup を、共有 mutex から**構造的に**外している。
3. **共有 cache**: `Arc<OnceLock<T>>` の slot を `Mutex<Vec<..>>` で配る。
   `rings_for(seed, generator)` は `Arc<OnceLock<Rings>>` を返し、重い ring 探索は
   `OnceLock::get_or_init` の中で走る。mutex を持つのは slot を出す瞬間だけで、
   後着の worker は **OnceLock で待つ**（cache mutex ではない）。既に出来ていれば
   mutex にも触らない。上限は定数で、`RING_CACHE_LIMIT = 8`、
   `DECORATION_CACHE_LIMIT = 4`。

**lock convoy を消している本体は `ChunkWork`**:

```
ChunkWork(Box<[Mutex<HashMap<(i32,i32), Weak<Mutex<()>>>>]>)   // 64 shards
work.at(key) -> Arc<Mutex<()>>
```

- chunk ごとに 1 本の work lock。A の前提（structure start / biome chunk）を作って
  いる worker が B の worker を止めない。
- 呼び出し側は `try_lock`（待てない読み手は `None` を返して諦める）か `lock` を選ぶ。
  ロックを取った後に cache を**二重に確認**し、計算し、cache に入れて離す。
  cache mutex を持つのは get/insert の瞬間だけ。
- `Weak` なので、誰も持っていないロックは map から消える（`strong_count > 0` で剪定）。
  map は chunk 数に比例して増えない。
- docstring の不変条件がそのまま設計契約になっている:
  "Only one work lock may be held at a time; finish shared prerequisites before
  acquiring it."、および structure 側の
  "Ring searches enlist other workers and wait for them: never hold a chunk-work
  lock while helping, even when this chunk has no starts."
  —— **別の chunk を助ける前に、自分の chunk の work lock を離す**。
  これを破ると cycle するので、規則として書いてある。

**exactly-once を assert する test がある**（`generator.rs:4953`）:
`Barrier::new(6)` で 6 スレッドを同じ chunk に同時に入れ、biome chunk と
structure start が `Arc::ptr_eq` であること（= 1 回だけ計算され、同じ実体が配られた）
を確かめる。cache の LRU 追い出し後に再計算されることも同じ test で見ている。
**「OnceLock 経路が実際に走ったこと」を証明する test** が同居しているのが要点で、
速度の主張だけを置いていない。

**メモリは API になっている**: `cache_census()` が
`CacheCensus { biome_chunks, biome_bytes, carver_entries, start_chunks, start_lists }`
を返し、`ProtoChunk::heap_bytes()` は 6 区分（packed blocks / section palettes /
heightmaps / その他 terrain / biomes / 構造体の箱）で返す。P3-2 の「peak RSS」を
外側の RSS 読みではなく**内訳**で語れる形にしてある。

### 5.2 mc-spine の現状との差分

mc-spine は既に段 1 を終えている。`src/worldgen.rs:694` の

```rust
pub struct Gen {
    pub(crate) data: std::sync::Arc<GenData>,   // 不変入力
    memo: HashMap<..>, aq_lv: .., aq_near: ..,  // scratch
    col_c, base_c, beard_c, clim_c, bioq, deep_c, feat_c, wg_after_carve, chunk_biomes,
    ...                                          // chunk ごとの cache
}
```

は、Horizon の 1 層目と 2 層目を 1 つの struct に同居させた形である。
一方、共有の入口は `static GEN: Mutex<Option<Gen>>` 1 本で、`GenLock`
（`src/worldgen.rs:2794`）が待ち時間と保持時間を分けて計測している
（spec の実測: 3 スレッドでロック待ち合計 1,994 ms、保持 3,478 ms、wall 1,484 ms）。

差分を 4 点に整理する:

| 観点 | Horizon | mc-spine |
|---|---|---|
| 重い計算中の lock | 持たない（`OnceLock::get_or_init` / per-chunk work lock） | `GEN` を chunk の terrain+populate+encode の間ずっと保持 |
| 後着の待ち方 | その値の `OnceLock` / その chunk の work lock | `GEN` 1 本 |
| 待ちたくない読み手 | `try_lock` → `None` で諦める経路がある | 無い（`GEN` を待つ） |
| 何を計算したかの証明 | `Arc::ptr_eq` の exactly-once test | `gen_lock_contention` は計測のみ |
| メモリ | `cache_census()` で内訳 | 外側の RSS と `*_ord` の長さ |

cache の数は mc-spine の方が多い（chunk ごとの cache が 10 本以上あり、それぞれ
`*_ord` の追い出し順を持つ）。したがって Horizon の形をそのまま写すのではなく、
**「cache ごとに 1 枚の `OnceLock` を配る」**という一点だけを借りるのが正しい。

### 5.3 P3-2 への写像（実装順）

1. **cache slot 化**: `col_c: HashMap<(i32,i32), Arc<ChunkCols>>` を
   `HashMap<(i32,i32), Arc<OnceLock<Arc<ChunkCols>>>>` にする。`GEN` を持つのは
   slot を出す瞬間だけにし、`ChunkCols` の計算は lock の外で行う。
   `col_c` から始める（populate の地形読みがここに集まるため、効果が一番大きい）。
2. **`Gen` の 3 分割**: `Arc<GenData>`（済）/ `Arc<GenShared>`（上記 cache）/
   `GenWorker`（`memo`, `aq_lv`, `aq_lvo`, `aq_near`, `aq_neo`, `deep_c`, `bioq`）。
   spec が「worker ローカルにするか読み取り専用スナップショットにするか」と
   保留している集合とほぼ同じで、Horizon の `WorkerState` がその答えの実例である。
3. **chunk ごとの work lock**: `ChunkWork` 相当（64 shard、
   `Weak<Mutex<()>>`、`at(key)`、`try_lock` 経路）を入れ、beardifier 解決と
   3x3 populate の「同じ chunk を 2 人で作らない」を担保する。
   不変条件を doc に書く: **work lock は同時に 1 本だけ。別 chunk を助ける前に離す。**
4. **exactly-once の test**: 6 スレッド + `Barrier` で 1 chunk を作り、
   `Arc::ptr_eq` を assert する。`gen_lock_contention` は「どれだけ待ったか」を
   測るが、「2 回計算していない」は測っていない。Horizon の test はその後者である。
5. **cache census**: `Gen` に「cache ごとの件数と推定バイト」を返す関数を足し、
   P3-2 の peak RSS を内訳で報告する。Horizon の `CacheCensus` は 5 項目だが、
   mc-spine は cache が多いので「件数 + 追い出し順の長さ + Arc の強参照数」でもよい。

## 6. Storage・Tick・server thread（依頼 c）

### 6.1 `PalettedContainer`（`crates/horizon-core/src/paletted.rs`、1639 行）

- `Kind::Blocks`（4096 entries、palette は 4 bit まで linear、5-8 bit は hash-map）と
  `Kind::Biomes`（64 entries、3 bit まで linear）。`global_bits` は registry サイズの
  `ceil_log2`。
- **常駐表現と保存表現を分けている**。常駐側は palette が縮まない
  （"Palettes never shrink while resident"）。縮むのは `pack` が canonical 形を
  書くときだけ。成長は `onResize` に従い、新しい configuration を
  「distinct な値を index 順に足し直し、入らなかった値を最後に足す」ことで作る。
- **clean-room の教訓**: wire/保存形式が常駐表現を縛る設計にしない。
  mc-spine 側で「保存形式に合わせて毎回詰め直す」実装があるなら、
  「resident は緩く、`pack` で正規化」の方が hot path から詰め直しを消せる。
  （mc-spine のどの型がこれに当たるかは本調査では特定していない。特定していない
  以上、「置き換えるべき」とは書かない。）

### 6.2 `storage.rs`（416 行）— multiply+shift と、その**適用しない**判断

- 64 bit word に palette index を詰め、**値は word を跨がない**（`SimpleBitStorage` の
  意味論）。
- `locate(bits, i)` は `n / per_word` を除算ではなく乗算 + shift で出す。
  `MAGIC[bits].0 = ceil(2^20 / (64/bits))` は `n * (64/bits) < 2^20` で厳密。
- **その上で、arm64 では使わないと決めている**。コメントに実測が残っている:
  "measured on an Apple M-series CPU, 65k reads took 1.5 ms with the division and
  2.5 ms with the table (bits 4-16), so only the x86 targets take the table."
  `const MAGIC_BEATS_DIVISION: bool = cfg!(any(target_arch = "x86", target_arch = "x86_64"));`
- `bits` が 2 の冪のときは shift だけで済ませる分岐もある。
- `Slot`（read して write する 1 組で word/shift を 1 回だけ計算）と
  `WordRun`（縦方向の fill が、entry ごとに locate し直さず word を歩く）は、
  「同じ位置を 2 回計算しない」ための型である。

**clean-room の教訓**: 「除算を乗算 + shift に置き換える」は**移植性のない**
最適化である。x86 では 20-26 cycle の除算が消えるが、arm64 の divider は table
lookup と同程度で、table 参照の方が遅い。**target で分岐し、両方で測る**。
mc-spine の `src/scheduled_ticks.rs` が `fastutil_mix64` を使っているのは
「hash 関数の一致が iteration 順を決めるから」であって速度のためではない。
この 2 つは理由が違うので、同じ「最適化」として混ぜない。

### 6.3 `LevelTicks` / `LevelChunkTicks`（`horizon-core/src/ticks/`、1063 行 + `level/tick.rs`）

3 つの部品から成る:

1. `heap.rs` — **pinned JDK `PriorityQueue` の array heap を再現**する。docstring:
   "Array heap with the pinned JDK PriorityQueue's equal-key and iterator-removal
   behavior. Equal comparator keys are observable in saved and loaded ticks."
   `up`/`down` を JDK と同じ手順で書き、同値キーの取り出し順を一致させる。
2. `index.rs` — `DeadlineIndex`。**pinned fastutil `Long2LongOpenHashMap` の走査順を
   再現**する。`mix` は fastutil の mix、rehash は**旧 table を末尾から先頭へ**走り、
   remove は backward-shift。docstring は
   "Deadline index whose traversal matches the pinned fastutil long map. Heap
   insertion order decides exact ties between chunk heads after save loading."
   visit は「vanilla の iterator 順」で、callback が deadline を更新して true を返すと
   削除、という契約。
3. `generation.rs` — `ProtoChunkTicks` と `WorldGenTickContainers`。worldgen 中に
   予約された tick を、level の queue に触らずに chunk ごとに集める。

そして **block と fluid は別の `LevelTicks`** である。`Ticks { blocks: LevelTicks,
fluids: LevelTicks, .. }` で、1 tick の順序は `level/mod.rs` の doc に明記されている:

> 1. `Behaviours::tick_scheduled` runs due block ticks then fluid ticks
>    (`blockTicks.tick(time, 65536, ...)`, then `fluidTicks`).

実装（`level/tick.rs:1761`）も、`blocks` を `begin_tick` → `next_tick` で**最後まで
drain** し → `finish_tick` → そのあと `fluids` を同じ手順で drain する。

### 6.4 mc-spine の tick owner との比較（P1-3）

mc-spine は同じ結論に**独立に**到達している。`src/scheduled_ticks.rs` は

- `heap_push` / `heap_pop` に「Java PriorityQueue の同値要素の扱いは、2 つの container が
  同じ priority/sub-order を持つとき観測可能」とコメントし、JDK と同じ sift を書いている。
- `NextChunks` に「native の next-deadline map は fastutil の Long2LongOpenHashMap。
  降順 table の iteration が tied chunk の挿入順を与える」とコメントし、
  `fastutil_mix64`、`grow` の降順 rehash、remove の backward-shift を再現している。

**データ構造も、queue の分割も、順序も収束している**。実際に読んで確認した一致:

| 観点 | vanilla / Horizon | mc-spine |
|---|---|---|
| container | `blockTicks` / `fluidTicks` の 2 本 | `block_ticks()`（`static Mutex<ScheduledTicks>` + `BLOCK_TICK_ORDER`）と `GeneratedFluidOwner.queue`（+ `next_order`）の 2 本 |
| `sub_order` の採番 | LevelTicks ごとの独立カウンタ | 同上（別カウンタ） |
| 1 tick の順序 | block を全部 drain → fluid | `game_tick` が `scheduled_blocks` → `generated_fluids` の順に `run` する |
| 1 tick の上限 | `tick(time, 65536, ..)` | `collect(now, 65_536, ..)` |
| heap | pinned JDK `PriorityQueue` | `heap_push` / `heap_pop`（同じ sift） |
| deadline index | pinned fastutil `Long2LongOpenHashMap` | `NextChunks`（同じ mix・降順 rehash・backward-shift） |
| proto tick の展開 | `LevelChunkTicks.unpack` が chunk ごとに `-N..-1` | `unpack_generated` が chunk ごとの pending 件数から `-*remaining` |
| 保存形式 | chunk ごとの `ScheduledTick` list | `TickState { blocks, fluids, block_order, fluid_order, .. }` と**分けて**保存 |

したがって P1-3 の不足は「データ構造」でも「block/fluid の混在」でもない。
読んで見つかった残りの差は 2 点で、どちらも**仮説**である（fixture で確かめる対象で
あって、誤りだと断定したものではない）:

1. **fluid の owner が vanilla では 1 本、mc-spine では 2 本に分かれている。**
   `GeneratedFluidOwner` は「生成から引き継いだ source fluid の予約」だけを持ち、
   それ以外の水・溶岩の伝播は `water_tick` / `lava_tick` が
   `WATER_QUEUE` / `LAVA_QUEUE`（`src/tick_queue.rs`、delay-index の ring buffer）で
   回している。`water_tick` 自身のコメントが "vanilla-ish" と書いている。
   vanilla の `LevelTicks<Fluid>` に対応する契約（priority・sub_order・重複排除・
   上限・cross-chunk eligibility）を、この 2 つのどちらが持つのかはコードから
   一意に読めない。**同じ概念を 2 つの owner が持っている**状態なので、
   P1-3 の spec には「fluid の scheduled tick はどちらの owner が答えるか」を
   1 行書く必要がある。Horizon にはこの分割が無い（`Ticks.fluids` 1 本）。
2. **eligibility を別の集合で答えている。** vanilla の `LevelTicks` は chunk の
   container が在るか + `isPositionTickingWithEntitiesLoaded` で eligibility を決める。
   Horizon は `is_ticking_chunk(chunk)` を closure で受け取る。mc-spine は
   `scheduled_tick_chunks()` が `region_guard().hot` に forceloaded の 3x3 を足した
   `HashSet` を作り、それを `collect` に渡す。同じ問いを別の集合で答えているので、
   `hot` の定義が vanilla の "ticking chunk" と一致するかを 1 点で確かめる価値がある。

### 6.5 `Behaviours` は世界を持たない（server-thread の分担）

`crates/horizon-world/src/level/` は、世界の状態を**所有しない**。`LevelAccess` trait
（`block_state` / `set_raw` / `is_loaded` / `random` / `game_time` / `block_light` /
`sky_light` / `emit` / `is_ticking_chunk`）越しに読み書きし、副作用（音・粒子・
item drop・落下 block・TNT・木の成長）は `LevelEvent` として返す。heightmap・light・
block entity・render mesh・dirty flag の更新は**呼び手の責任**である、と doc に書いてある。

1 tick の順序も doc に列挙されている: scheduled tick → 各 ticking chunk の random tick
（`randomTickSpeed`、`getBlockRandomPos`）→ block event（piston）→（entity の後で）
block entity（moving piston）。`horizon_threads::work::set_chunk` / `set_block` で
「このスレッドが今どの chunk/block を触っているか」を **thread-local** に記録し、
sampler が lock なしで読む（`horizon-world/src/level/tick.rs:1780`）。

**clean-room の教訓**: 「規則の owner は 1 つ、状態の owner は呼び手」に分けると、
並列化のたびに世界全体のロックを設計し直さなくて済む。mc-spine は
`static` ベースの所有（`src/*.rs` に `static` 101 個、うち `Mutex` 52 個）で、
Horizon の `crates/horizon-world/src`（357 ファイル）は `static` 20 個、
うち `Mutex` は 6 個（`level/data/cache.rs` の parse 済み source、`json_folder.rs` の
folder handle と in-flight map、`item_defaults.rs` の同じ 2 つ、
`block_state_properties.rs` の intern 表）— いずれも**資源の singleton cache** で、
`worldgen/` 配下に `Mutex` static は 1 つも無い（あるのは `OnceLock<Processor>` 3 個と
`OnceLock<NativeGenerateHooks>`）。
数を優劣として読むべきではない（mc-spine は server の runtime 状態を
プロセスに 1 つ持つ設計）が、**P3-2 で worker に渡せない状態がどこにあるか**は
この差にそのまま現れる。

### 6.6 無かったもの（negative findings）

探して**見つからなかった**ものを記録する。次に同じ調査を繰り返さないため。

- **`streaming.rs` は存在しない**。依頼文が名指しした
  `crates/horizon-world/src/streaming.rs` は HEAD に無い。近い役割は
  `worldgen/feature_region.rs`（`TerrainRegion` / 隣接 chunk の窓）と
  `worldgen/generator_structures.rs`（近傍の structure start を引く）が担っている。
- **lighting は無い**。`horizon-world` に light engine は無く、光は
  `LevelAccess::block_light` / `sky_light` の**問い合わせ**（既定 `0`）である。
  mc-spine の光の作業に対して Horizon から借りられるものは無い。
- **`horizon-world` に `rayon` は無い**。並列化は `std::thread::scope` +
  `Arc` + `OnceLock` + `Mutex` で書かれている（`generator.rs` の test が
  `thread::scope` + `Barrier`）。「並列化 = rayon を入れる」ではない。

## 7. 適用しないもの

- Horizon のコードは 1 行も取り込んでいない（LGPL-3.0-or-later。`horizon-core` の
  paletted / storage / ticks は特に、MIT 時代の revision からでも本調査では写さない）。
- 26.3 のデータ・式・registry・packet ID は 1.21.1 に持ち込まない
  （[README](README.md) の判断表のとおり）。
- `tests/verify.sh`、gate、`policy.json`、`deps_target.py`、テストは触っていない。
  `#[ignore]` も新しい `mod` も足していない。
- `[profile.fast]`（Horizon の「release 相当だが速くリンクする」profile）は入れていない。
  mc-spine の release は既に `lto = true` で、必要になった時点で別途測る話である。

## 8. 再現手順

```sh
# profile を 1 段ずつ測る（target dir は worktree の外に置く）
mkdir -p "$HOME/.cache/mc-spine-bench/tmp"
export TMPDIR=$HOME/.cache/mc-spine-bench/tmp          # /tmp が tmpfs で埋まる時
export CARGO_TARGET_DIR=$HOME/.cache/mc-spine-bench/before

t0=$(date +%s%N); cargo test --offline --locked --bin mc-spine --no-run; t1=$(date +%s%N)
echo "cold build $(( (t1-t0)/1000000 )) ms"
t0=$(date +%s%N); cargo test --offline --locked --bin mc-spine -- --test-threads=1; t1=$(date +%s%N)
echo "suite $(( (t1-t0)/1000000 )) ms"
stat -c '%s' "$CARGO_TARGET_DIR"/debug/deps/mc_spine-*[0-9a-f]
```

`Cargo.toml` を戻して同じ 2 行を実行すれば before が取れる。測定は
`systemd-run --user --scope -p MemoryMax=14G` で 1 本ずつ、事前に
`resource_gate.py --class light --need-mem-gb 8` が GO を返すことを確認して行った。
