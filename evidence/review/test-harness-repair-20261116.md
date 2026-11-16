# テスト基盤の修理 — 3つの実測欠陥（2026-11-16）

Date: 2026-11-16. 対象: 候補 `ac0896d` の上に積んだ3コミット（`c3e71c0`, `d4e0e6f`, `cbe2cf2`）。
作業は tool-created worktree（`t3-87d06ba5-…`）と private audit dir
`/home/yuu/mc-spine-local/audit/test-harness-repair-20261116T150404/` のみ。
canonical `/home/yuu/mc-spine` への書き込み・push・network・full campaign・native capture は無し。

本書は**その時点の測定記録**であって恒久的な契約ではない。恒久的な契約は
[`tools/README.md` の gate 節](../../tools/README.md)に、動く gate は
`tests/gate_scripts.py` にある。

## 直した3つ

| # | 欠陥（実測済み） | 修理 |
|---|---|---|
| F1 | `mutation_check.sh` が**作業ツリーの `src/*.rs` を直接書き換える**。fake cargo 下で 1 entry あたり 0.621 秒 mutant が tree に存在し、SIGKILL で mutant と `.mutation-backup` が残る | campaign は入力の private snapshot 上で走る。copy は1 campaign 1回、`CARGO_TARGET_DIR` も snapshot 内、終了時に削除。copy と source の sha256 manifest を前後で取り、不一致／copy 中に source が動いたら測らずに exit 2 |
| D1 | stability 再実行が `if [ "$status" = green ]` だけを見る。2回目が compile error / SIGSEGV でも「2回失敗した」= proved | 1回目と再実行を同じ `classify()` に通す。proved は「選択テストが `test result: FAILED` かつ failed ≥ 1」または wire gate の完全一致行のみ。2回目が緑なら flake、compile/crash なら unproven。`MUTATION_STABILITY` は 0/1 以外を exit 2。`tests/verify.sh` の既定を `MUTATION_STABILITY=1` に |
| D2 | 新しい sky-light テスト（`27b6221`）が `world_guard`（Mutex のみ）を取り、`set_block` で世界・floors・band・tick queue 等を書き換え、復元は本文の末尾だけ。panic で残る | 本文を `test_support::isolated_world_test()` の子プロセスで実行（既存15件と同じ owner）。復元は不要になり、panic もプロセスと共に消える。「書いてから panic する」fixture と、parent の fingerprint が不変であることを示す自己テストを追加 |

## 直した後の測定

### 1. 隔離（F1）— 同じ filter・同じ fake cargo で old/new を比較

`pickup_tick_keeps_the_partial_entity_without_a_take_event`（両方の script に存在する entry）。
20 ms ごとに live `src/*.rs` と snapshot の `src/main.rs` を sha256 で採取。

| | old (`ac0896d`) | repaired |
|---|---|---|
| 正常終了 | mutant が **live tree** に 97 sample / 終了時は復元・残渣なし | live tree は**不変**（`b8d446f0…` のまま）、mutant（`3b810b0c…`）は snapshot のみ、終了時に snapshot 削除 |
| SIGKILL | live tree **未復元**、`src/main.rs.mutation-backup` が残る | live tree **不変**、残渣は owned snapshot のみ（利用者のツリーではない） |
| 呼び手の `CARGO_TARGET_DIR` | （script は上書きしない） | **使われない**（sentinel 不変、target は snapshot 内） |

mutant sha `3b810b0c…` は元の測定（`audit/test-causality-tc-20261116/probe-window.json`）と同一で、
同じバイト列が今は snapshot にしか存在しない。
durable な側は `tests/gate_scripts.py` の「sigkill leaves residue only in the private copy」—
自前のプロセスグループを kill し、live tree 不変・残渣は private path であることを要求する。

### 2. 分類（D1）— 11 scenario、script は無改変、cargo だけ決定的 shim

`audit/test-causality-followup-20261116/probe_stability_contract.py` を private copy で old/new に実行
（old = `ac0896d` の script、new = 修理後）。差が出たのは2行だけ:

| scenario | old | repaired |
|---|---|---|
| `stability_second_compile_error` | exit 0, **1 proved** | exit 1, 0 proved |
| `stability_second_harness_crash` | exit 0, **1 proved** | exit 1, 0 proved |

残り9件（既定モードの5件、`second_failed`、`second_green`、`second_zero_selected`、
`second_ignored`）は同じ判定。つまり**本物の catch は落とさず**、偽の2件だけを落とした。
`second_green` は "the failure was a flake"、compile/crash は
"the re-run did not report a failing test (compile|crash) … unproven" と理由を分けて出す。

### 3. 実 Rust（D2 + 統合）— 修理後の harness 経由、`MUTATION_STABILITY=1`

| entry | 結果 | mutant の assertion |
|---|---|---|
| `the light packet assembles spill into its floors`（新規、`src/main.rs:16082`） | 1 proved（2回失敗 → restored green） | `sideways arrival down an open tunnel  left: [0,0,0,0,0]  right: [11,10,9,8,7]` |
| `a traced column names the layer that answered it`（6B, `generation_context.rs`） | 1 proved | `left: (1, "terrain") right: (1, "rc")` |
| `partial insertion is not a successful ItemEntity take callback` | 1 proved | pickup assertion |
| `sky spill loses one level per step` | 1 proved | spill BFS assertion |

assembly 変異だけを private copy で適用した binary identity（`identity_probe.py`）:

| stage | `src/main.rs` sha256 | test binary sha256 | `cargo test sky_light_assembly` |
|---|---|---|---|
| baseline | `b8d446f0…` | `6f76483f…` | ok, 1 passed |
| mutant | `bd5eddda…` | `328418ce…`（別ファイル） | FAILED, 0 passed / 1 failed |
| restored | `b8d446f0…`（pristine と一致） | `6f76483f…`（baseline binary と一致） | ok, 1 passed |

### 4. 全 Rust suite（最終 source で1回）

```
test result: ok. 544 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 93.51s
```

（レビュー用の整理 commit `cbe2cf2` の後に測り直した値。整理は wrapper 1個と未使用引数だけで挙動は変えていない。）

`1 ignored` は既存の parked gate（debt ratchet が `ok ignored tests (parked gates): 1` と数えるもの）で、
本修理では増減していない。ログには隔離 fixture の**意図した**子プロセスの panic
（`the fixture panicked after writing to the shared scene` → `test result: FAILED. 0 passed; 1 failed`）が
そのまま出る。これは観測対象であって suite の失敗ではない（suite の最終行は上の1行）。

### 5. gate 自己テスト（`tests/gate_scripts.py`）

25 case 全 PASS（既存15 + 新規10、最終 source で再測）。新規: 隔離（journal で cwd/target/copy の bytes を検査）、
dirty な source 編集が copy に載ること、copy 中に source が動いたら拒否、SIGKILL 残渣、
stability の5 case（fails twice / green / compile / crash / zero-selected）と 0/1 検証。

### 6. 登録 entry の静的監査（変異なし）

- entry 424（`ac0896d` 423 → +1、**削除 0**、重複名 0）
- 424 件すべてで anchor が対象ファイルに**ちょうど1回**
- distinct filter 243。複数 entry が共有する filter は `tests/*.py` と意図的な複数変異のみ
- 408 と 423 の関係は算術であって謎ではない: `e7ead8e` = 408、worldgen 追加 15 → 423、
  本修理 +1 → 424。追試レポートの「408 対 423 が合わない」は、その時点のツリーが 408 件だっただけ

## 動かした検証と、動かしていない検証

| 検証 | 結果 |
|---|---|
| `tests/gate_scripts.py` | PASS（25 case） |
| `tests/evidence_receipt_test.py` | PASS（8 tests） |
| `tests/layers_gate_test.py` | PASS（17 tests、depsgx を PATH に置いた場合） |
| `bash tools/layers_check.sh` | PASS 42 modules / 108 edges / 0 named errors |
| `bash tools/debt_ratchet.sh` | 0 over limit / 1 parked（既存） |
| 全 Rust suite | PASS 544/0/1（93.51s） |
| 実変異 4 entry（stability 有効） | 4 proved |
| **NOT RUN** | `tests/verify.sh` 全体（mutation 全 424 entry を含む。1 entry = baseline+mutant+再実行+restored で数百ビルド）、native capture / wire / adapter（この修理は pure harness + test のみ）、release ビルド |

### 依存の出所（正直に）

`layers_check.sh` は CI 固定の depsgx（`d6b9ad8db4774919c8b4fb032342e53b2548b660`）ではなく、
ホストにある `/home/yuu/gxkit/.venv/bin/depsgx` **1.0.0（editable HEAD `973b293a`）** で PASS した。
固定版での green ではないし、インストールもしていない。

## 資源

すべて `resource_gate.py --class light` の GO を取り、同じコマンドで
`systemd-run --user -p MemoryMax=… -p CPUQuota=…` の named scope 内で実行。GPU 不使用。
cargo のピーク RSS は 2.2 GB、cold private target は 853 MiB。`-j 2`、直列、release との二重ビルド無し。
private evidence は 82 MiB copy + 853 MiB target を1 campaign ごとに作って消す
（`MUTATION_SNAPSHOT_PARENT` で置き場所だけ変更可）。
