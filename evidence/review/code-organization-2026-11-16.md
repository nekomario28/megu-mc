# tick セクションのフッタ所有者を1つにした（code-organization 2026-11-16）

対象は non-worldgen の persistence ユーティリティ。`src/tick_persistence.rs` 1ファイルだけを触る。
新しいモジュール宣言・ファイル移動・依存追加・整形の巻き添えはない。

## 範囲

`TickState` はセーブの1セクションで、末尾12バイト（本体長 u64 + `TIK1`）のフッタで自分を枠取る。
同じフッタを**2つの消費者**が別々に読んでいた:

| 消費者 | 用途 | 変更前の読み方 |
|---|---|---|
| `prefix()` | 手前のセクション（`GEN1`）を切り出す | マジック確認 → `checked_sub(12)` → 長さ → `checked_sub(size)` |
| `TickState::read()` | 自分の本体を解析する | マジック確認（2回目）→ `prefix()?.len()` → `payload.len() - 12`（12の2回目） |

同じ規則の所有者が2つあり、境界を2回計算していた。実測（`git show HEAD:src/tick_persistence.rs`）:

- `b"TIK1"` の出現 **3**（const なし）、フレーミング中のリテラル `12` **2**、境界計算サイト **3**
- `read` の中に**入れ子関数4つ**（`take`/`pos`/`count`/`read_entries`）。書き側は
  `position`/`entries` としてモジュール直下にいるのに、読み側だけ関数の中にいた
- 末尾検査が2つの規則を1つのメッセージに畳んでいた:
  `if !input.is_empty() || generated_fluids.iter().any(|(_, id)| *id > 3) { "invalid tick state tail" }`
  → 流体IDの範囲外でも「tail」と報告する

**挙動の差**（潜在バグ、負の対照の対象）: 本体長0のフッタで `read` は `Err`、`prefix` は `Ok(&[])`。
`prefix` は「手前のセクション」を空として返すため、呼び出し順が変われば `GEN1` が何もない所から
解析され得た。`main.rs` は `read` を先に呼ぶので現在は表に出ていない。

## 変更後の所有（実測）

- フッタの読み手は `framed()` **1つ**。`(手前, 本体)` を返し、`prefix` と `read` はその結果を使うだけ
- マジック **1**（`const MAGIC`）、フッタ長 **1**（`const FOOTER_LEN: usize = 12`）、境界計算 **1**
- `b"TIK1"` の出現 **1**、`12` の直書き **0**、`read` の入れ子関数 **0**、
  モジュール直下のコーデック関数は書き2・読み4で対称（`position`↔`read_position`、`entries`↔`read_entries`）
- 末尾検査と流体ID境界を分離（拒否は同じ、診断だけが原因を指す）
- 行数: 200 → 317。テスト節は 52 → 138（**+86 はテスト**）、実装節は 148 → 178
  （+30 のうち約12行は説明コメント）

`git diff --numstat`: 168 added / 51 removed。実装差分は「2箇所の境界計算を1つにし、
入れ子関数を書き側の隣へ出した」だけで、式・分岐・反復順・保存形式は変えていない。

## 挙動を保存した理由（呼び出し側の全数確認）

全ツリー（`src/ tools/ tests/ specs/ *.md`）で当該シンボルを検索した結果は
`/home/yuu/mc-spine-local/audit/code-organization/call-sites.log`:

- `tick_persistence::prefix` … `main.rs:9780`（`GEN1` 解析用）、`main.rs:27303`（カート検査）、
  `mineshaft_tests.rs:647,724`。いずれも返り値のスライスだけを使い、本体長0の受理に依存していない
- `TickState::read` … `main.rs:9779`（ロード経路）と本ファイルのテスト。
- `TickState` 構造体の生成 … `main.rs:9756` のみ（`append`）。
- エラー文字列を照合する消費者は無し（`grep -rn "invalid tick state\|truncated tick state" src/ tests/ tools/`
  の一致は定義側の `src/tick_persistence.rs` と `src/scheduled_ticks.rs` だけで、`assert` で比較する
  テスト・ツールは0件）。`"invalid fluid tick id"` を新設したのはこのため。

保存形式の互換は変えていない: `append` が書くバイト列は同一（`out.extend_from_slice(MAGIC)` は
`b"TIK1"` と同じ）。受理/拒否の判定が変わる入力は**1つだけ**（本体長0のフッタ。`read` は
変更前から拒否、`prefix` が受理していたものを拒否に揃えた）。流体ID>3 の拒否は変更前と同じで、
変わるのはメッセージだけ。

## テストと受領書

`/home/yuu/mc-spine-local/audit/code-organization/final-receipt.log`（source-bound）:

- `src/tick_persistence.rs` sha256 = `9bc68c9102782b7b0768c0d102699dcdac4e0076635c8d2af8c5179852d25cae`
  （実行前後で同一 = 走行中に内容は変わっていない）
- HEAD = `e7ead8e66466b0b6467450cd3ea31d86a0e39cd3`、開始 2026-11-16T14:00:32+09:00 / 終了 14:00:34+09:00
  （ログに `Compiling mc-spine` とバイナリの mtime 14:00:34 があり、この実行が最終内容の再ビルドである）
- テストバイナリ sha256 = `30e3d820cd9d47186dad6c0fd168cc95334038fbbcd6a5f8f5fdb18e43ae2997`
  （144,499,136 bytes。直前の版と同一ハッシュ = 最後の差分が同一行数のコメントのみで、行番号が動いていないことと一致する）
- `tick_persistence::tests` **5 passed / 0 failed**（530 filtered）
- `persistence_tests::` **13 passed / 0 failed**（子プロセスの isolated fixture 各1 passed を含む）
- ビルドは `MemoryMax=6G` `MemorySwapMax=2G` `CPUQuota=200%` の named scope、`-j 2`、
  `CARGO_TARGET_DIR=/home/yuu/mc-spine-local/target/code-org-26c63f09`（他workerと非共有）
- `cargo test --no-run` の警告30件に `src/tick_persistence.rs` を指すものは無い（`build-2.log` を
  `grep tick_persistence` して0件）。この変更が増やした警告は無い。clippy は **NOT RUN**（別ビルドが必要で、
  本変更の判断には不要）
- `rustfmt --edition 2021 --check src/tick_persistence.rs` = clean

新規テスト4本のうち3本は**不変条件**（フッタ境界が1箇所から出ていることの固定）で、
1本が**負の対照**。

### 負の対照

`prefix` だけを変更前の実装（自前のフレーミング）に戻すと
`a_footer_that_frames_no_body_is_refused_by_both_consumers` が落ちる
（`/home/yuu/mc-spine-local/audit/code-organization/negative-control.log`）:

```
panicked at src/tick_persistence.rs:257:9:   (変更前実装に戻した版の行番号、ログからの引用)
prefix accepted a section with no body
test result: FAILED. 0 passed; 1 failed; ... 534 filtered out
```

## 未解決（このコミットで触らないもの）

1. **同じフッタ規則の2人目の所有者**: `src/generated_content.rs:381-397` が
   `checked_sub(12)` / `u64` 長 / `checked_sub(size)` を自前で持つ。1つにするとしたら
   `framed()` がマジックを引数に取る形になるが、そのとき汎用のセクション・フレーミングの
   所有者が `tick_persistence` という名前のモジュールになる（正しい所有者は `main.rs` の
   セーブ外皮 = `save_world_into`/`load_world_from`）。`main.rs` は本作業の対象外で、
   `generated_content.rs` は worldgen 統合候補と活動が重なるため触っていない。
2. **`catch_panic` の置き場所**: `src/connection_limits.rs` の末尾「tick supervision」節にある。
   listener のポリシー・モジュールに tick 監督が同居しており、モジュール冒頭の説明
   （「Connection-level limits and abuse guards for the listeners」）と一致しない。
   移す先（新モジュール宣言）は本作業では禁止。
3. **`src/cached_lookup.rs` のモジュール説明が `///`**: 1行目からの説明が `CAPACITY` に付いており、
   `mod cached_lookup` にはモジュールドキュメントが無い。1行の修正だが別の責務なので分離した。
4. **テスト用フィクスチャの重複**: `furnace()`（`tick_tests.rs:818`）と `new_furnace()`
   （`furnace_cache_tests.rs:87`）は同じ構造体の2つのコンストラクタ。共有の置き場（main の
   テストスコープ）を決める必要があり、別コミット相当。

死んだコードの全数走査（非worldgen、`fn` 名の全ツリー出現数=1）では、ヒットは
`tick_tests.rs::profile_game_tick` の1件のみ。これは`#[ignore]`で隔離profilerから
名前指定で実行する入口なので死んでいない。

## 次の2–3の境界（提案）

- `generated_content::Snapshot::read` のフッタを `tick_persistence` 側の1実装へ寄せる
  （上記1。外皮の所有者を `main.rs` に置く設計判断が先）。
- `catch_panic` を tick 監督の所有者へ移す（上記2。`tick_stats` は計測で監督ではない）。
- `cached_lookup.rs` と `furnace` フィクスチャの整理（上記3・4。どちらも1ファイル規模）。

## 並行作業との排他

触ったのは `src/tick_persistence.rs` と本ページのみ。`main.rs`・worldgen/feature/structure 系・
`block_registry.rs`・`scheduled_ticks.rs`・`tools/dev_check.py` 系・README/CONTRIBUTING/specs/
DESIGN/TODO・`evidence/foundation/module-layers/policy.json`・`deps_target.py` は未変更
（`git status --porcelain` で確認）。レイヤグラフは変更前後で同一:
`tools/rust_module_graph.py` の展開から **42 modules / 108 edges / added [] / removed []**、
`policy.json` の `allowed_edges` と完全一致（`layers.log`）。`depsgx inspect` 側は
未インストールのため **NOT RUN**（導入はネットワークを伴うため行っていない）。
`tools/debt_ratchet.sh` は wall-clock 0 / ignored 1 で PASS。

参考（ docs 側の担当）: `specs/README.md` の「層ゲートは20 modules / 47 edges」は現在の
42 modules / 108 edges と一致しない。
