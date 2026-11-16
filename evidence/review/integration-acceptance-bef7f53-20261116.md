# Integration acceptance — Track 5A/5C + 6A + 6B + 6C

状態: **統合候補を構築し、実 SHA 上で検証した。canonical checkout へは未着地。**
この文書は索引であり、置き換えではない。数値はすべて下記の実行ログに紐づく。

**2026-11-16 追補（最終 remediation）**: 独立レビュー `5dcaf3a` の F1–F6 をこの文書に反映した。
変更は文書と `src/feature.rs` の docstring だけで、production の意味は変えていない。
新しい測定は `/home/yuu/mc-spine-local/audit/final-landing-20261116-1415/` にあり、
以下の各節で「継承（当時の測定、いま再検証不能）」と「今回測った」を分けて書く。
canonical `perf/worldgen-parallel`（`e7ead8e`）はこの remediation の間も**未変更**。

- 作業 worktree: `/home/yuu/.t3/worktrees/mc-spine/t3-b2eb8ad1-ebf296b8314a2ff7`
- branch: `t3/worldgen-integration-audit`
- base: `e7ead8e66466b0b6467450cd3ea31d86a0e39cd3`（`perf/worldgen-parallel`）
- **統合候補（コード）: `bef7f531ab902e64d05ef04d339c9f8a796f0e68`**
- この報告 commit は `bef7f53` の上に docs のみを積む
- 実行台帳: `/home/yuu/mc-spine-local/audit/deepseek-int-20261116-125633/`

## 1. 選んだ commit / 捨てた commit

| 順 | 候補内 SHA | 出所 | 選定理由 |
|---|---|---|---|
| 1–10 | `9fcb5da` (fast-forward) | `t3/mc-spine-evidence-validation` の e7ead8e..9fcb5da | chain 全体をそのまま。`247602f 06adb62 0b2d92c b083d80 35a49c7 0a8c3c4 a539e62 6a2e88f 14d1916 9fcb5da` |
| 11 | `787f781` | 6A `f166588` | 実装 commit のみ |
| 12 | `b7ca4f5` | 6B `085206b` | 最終 diagnostics commit のみ |
| 13 | `bef7f53` | 6C `864bf73` | 最終実装 commit のみ |

捨てた重複（`git patch-id --stable` で同一を確認。推測ではない）:

- profile 変更 1 patch `945d4d41b83561795d444b70cc2b9858769fc41a` が4本:
  `247602f`(採用) / `84182d1`(6A の親) / `2fc5247`(6B の親) / `4971273`(6C の親) → 後ろ3本を棄却
- 5C/6B 共通テスト 1 patch `c5578119a1cf727202d882fddb1aeba38c4cbb97`:
  `06adb62`(採用) / `dfe52eb`(6B) → `dfe52eb` を棄却

その結果 `89ac2124`/`fc9dfdf` 相当の内容も profile 変更も二重適用されていない。
`t3/548b811e`（Track 5B）は `e7ead8e` のまま一切触っていない。その dirty な
`src/feature.rs` / `src/tree.rs` / carve-agent は読んでいない。

## 2. 衝突と解釈

衝突は1箇所だけ: `src/generation_contract_tests.rs`（6C の適用時）。

原因: 5A/5C が pinned-facts のテスト塊を、6C が iteration-order のテスト塊を、
どちらも**ファイル末尾**に追記したため、git が共通の末尾 `}` で分割した。

解釈: 両側とも独立した完全な `#[test]` ブロックで、互いを書き換えていない。
**両方を残した**（HEAD 側を先、6C 側を後）。HEAD 側の最後のテスト
`face_support_answers_the_support_type_the_caller_asks_for` を明示的な `}` で閉じ、
既存の共通 `}` が 6C 側 cocoa テストを閉じる。

検証（推測でなく計測）:

- conflict marker 0
- 波括弧の総数 1543 / 1543
- `bash -n tools/mutation_check.sh` OK
- mutation entry 423 件、重複名 0
- 全 423 entry の anchor が対象ファイルに**ちょうど1回**存在（後述 §6）

どの側のテスト・変異・診断も落としていない。

## 3. 設計判断・既定値・所有者・文脈の限界

6A が確立した所有者の移動（それ自体が主張の中心）:

- 物理述語の唯一の所有者が `assets/block_state_facts_1211.bin`（26,684状態 × 5 byte =
  133,420 byte）。旧 `state_flags` bit0/5/6、`luminance`、`opacity` は**読み手ごと撤去**。
- `is_solid` は `blocks_motion` の別名（40 箇所の呼び出し名を保つため）。値は native 述語。
- `side_solid` = `is_face_sturdy(.., Support::Full)`。FULL/CENTER/RIGID は
  **enum 定数名から**引き、配列位置に依存しない。
- `can_occlude` / `is_liquid_block` は**生産消費者なし**（監査用の列）。既定値は
  「表にあるが誰も読まない」。将来の呼び出しがヒューリスティックでなく事実を読むため。

6C が確立した所有者: `Direction.Plane.HORIZONTAL` の順序は `tree::horizontal_dir` が単独所有し、
`plane_horizontal()` がそこから導出する。trunk の `getRandomDirection`、cocoa/mangrove の walk、
cherry の branch が同じ配列を読む。

文脈の限界（重要）:

- 表は **state ごとの性質**で、`initCache()` が構築時に解決するキャッシュを読む。state 局所性は
  6A が stone / air / `EmptyBlockGetter` の3世界で全列 0 差分を測った**実測**であり、
  「任意の文脈で同じ」ことの証明ではない。
- `SupportType.FULL.isSupporting`（`doa$1`）の本体は自分では近傍セルを読まず、
  `isFaceFull(state.l(level,pos), dir)` に委譲する。`dtc.l` は level/pos を取る衝突形状
  accessor（`dtb.b_` → `dtb.b(..., CollisionContext.empty())`）で、`dtc.k` はキャッシュが
  あればそれを、無ければ**同じ4引数**を呼ぶ。よって bytecode から言えるのは「本体は近傍を
  読まない」までで、block entity を持つ位置の live な形状は3世界の測定の外側にある。
  表では `moving_piston` 2063..2074 が `blocksMotion|colEmpty|fluidEmpty` で FULL/CENTER/RIGID
  すべて空（= `EmptyBlockGetter` の答え）。native の live な moving piston は運搬 block の
  形状を返すので面があり得るが、当 port は moving_piston を**生成しない**
  （`src/main.rs:11019` は名前→id、`src/feature.rs:3399` は判定用の名前一致のみ、
  piston シミュレーション無し）ため生成経路では到達しない。**live 文脈での一致は未証明**で、
  新しい runtime consumer も足していない。
- したがって表の適用範囲は「観測したキャッシュ済み/クエリ/生成済みの state」に限る。
  この範囲で **文脈依存なのは `getRawBrightness` の側だけ**であり、6B が扱う残差の
  明るさ半分は表では解けない（6B の結論どおり）。
- 3つの空/石世界での all-state 一致は「任意の文脈で同じ」ことの証明にはならない。
  表の妥当性は (a) 3世界での all-state 0 差分（上記の限定つき）、(b) 26,684状態の
  再輸出一致、(c) 呼び出し地点ごとの native 本体の同定、の3点で支えている。

## 4. 最初に観測した分岐と、実際に走った経路

- 6A の3つの修正が効く状態に、**village-taiga の 4 diff は到達していない**
  （6A 自身の測定と同じ。bit-identical は「届いていない」の証拠であって「無効」の証拠ではない）。
  よって 6A は (i) 契約テスト、(ii) `--check` 再輸出、(iii) 変異、の3経路で押さえた。
- 6C の jungle +5 は、差分セルの実座標と、cocoa walk だけを戻す切り分けで経路を特定した（§7）。
- 6B は「書込みは成功したのに save に無い」を、FCELL 28 行と保存チャンク 24 セルの
  **セル単位一致**で示した（§8）。

## 5. 検証の終了値と件数（すべて候補 SHA 上）

| 検証 | 結果 | ログ |
|---|---|---|
| build (`cargo test --no-run`, `CARGO_TARGET_DIR` 専用) | exit 0 / 22.29 s / cold | `build-no-run.log` |
| `--list` | exit 0 / **542 tests** | `test-list.txt` |
| 新規契約テスト 10 本（個別 filter, N=1 ずつ） | **10/10 PASS** | `focused-tests.log` |
| full Rust suite 1 回 (`--test-threads=1`) | **541 passed / 0 failed / 1 ignored / 92.52 s / exit 0** | `full-suite.log` |
| `tools/layers_check.sh` | PASS (42 modules, 108 edges) | `static-gates.log` |
| `tools/debt_ratchet.sh` | 0 over limit | 同上 |
| `python3 tests/gate_scripts.py` | PASS（6 self-test すべて） | 同上 |
| Python unit（`tests/verify.sh` の 37 ファイル） | **当初の記載「37/37 exit 0」は誤り**。`python-units.log` は 36 entry = 35 exit 0 + `layers_gate_test` exit 1（depsgx 不在で 16 failures）。末尾の `layers_gate_exit=0` は変数だけで、合格再実行の出力は保存されていない。再実行は §5.1 | `python-units.log` |
| `tests/layers_gate_test.py`（17 tests） | depsgx を PATH に置いた再実行で **17/17 OK**（§5.1） | `final-landing-20261116-1415/python-units-rerun2/` |
| `tools/block_state_facts_from_jar.py`（`--check` 相当） | **26,684 states match**（pinned jar sha1 `59353fb4…`） | `exporter-check.log` |
| focused mutation（追加 15 entry のうち 14） | **14 proved / 0 skipped / 0 parked / 0 not-a-gate**。追加集合は 15（5C 3 / 6A 6 / **6B 1** / 6C 5）なので当時 1 件未実行だった（§6・§16-2） | `mutations-summary.log`, `mutation-logs/` |
| 6B の未実行 1 件の追加実行（2026-11-16） | `a traced column names the layer that answered it` = **ok / proved**（`src/generation_context.rs`、filter `cel_column_trace_tags_the_layer_that_answered`）: baseline green → mutant `test result: FAILED. 0 passed; 1 failed`（`left: (1, "terrain") right: (1, "rc")`、`error[E` 0 件）→ restored green | `final-landing-20261116-1415/mutation-logs/` |

ログ digest（sha256）:

```
fa4c22039cce59ab158a585293fc438eb232c892e8c146737a8a67c0f5de60af  full-suite.log
f1640ee36ce734163a06edd6834eac4f1ccd507702c3cf6392645d30b61c1d40  focused-tests.log
a47c0faf44b18d3b4030ee8e78e1f5436a489abddc69e72b168c069badbeaad8  static-gates.log
ab5a32faddb65cb61778030adfc4898202e540a58517b79d5a0fa861b5af6c3a  python-units.log
d557519ea4e467ed637b6362c81bdeab01ce262f50a5049e2f28a926b4756601  exporter-check.log
37e2860e90a8d45fe7ebb86c61a32d326f4a7ec15edd2ac6edd931f618d2d5ad  mutations-summary.log
17574a8bd31390618dea4ae9f3183ead319acf019066075e066680828cb37c2e  fixtures.log
```

### 5.1 Python unit の再実行（2026-11-16, 最終 remediation）

最初の `python-units.log` は 37 ファイルのうち 36 本しか走っておらず（`gate_scripts.py` は
`static-gates.log` 側）、その 1 本 `layers_gate_test` は `depsgx` が PATH に無く
**16 failures / exit 1** だった。末尾の `layers_gate_exit=0` は変数への代入だけで、
合格した実行の出力は残っていない。よって「37/37 exit 0」は記録として成立しない。
この履歴は消さない（誤った試行として残す）。

再実行（`final-landing-20261116-1415/python-units-rerun2/`）:

- 一覧は `tests/verify.sh` の `^python3 tests/*.py` 行を機械抽出（**ファイル 37**）。
- 1 ファイルごとに start / end / exit を `entries.log` に記録し、1 つでも exit != 0 なら
  その場で停止する。集計は行ではなく**明細の集計**で、`SCANNED == ATTEMPTED == ENTRY 行数`、
  `PASS+FAIL == ATTEMPTED`、`FAIL == 0` を assert する（`run_python_units.sh`）。
- 結果: **ファイル 37/37 exit 0**、うち unittest のケース合計 **196**（29 ファイルが
  `Ran N tests` を出す）。`summary.txt` は `SCANNED=37 ATTEMPTED=37 PASS=37 FAIL=0`。
- 検査器の同定: `depsgx` **1.0.0**、import 元 `/home/yuu/gxkit/packages/depsgx/src`
  （editable、gxkit **HEAD `973b293a…`**、`packages/depsgx` は clean）。CI の pin は
  `d6b9ad8…` だが、この host の venv は pin ではない（`merge-base --is-ancestor` = no、
  `d6b9ad8..973b293 -- packages/depsgx` は 8 files / +120 / −27）。**したがって
  「pinned depsgx」とは主張しない**。検査器が落ちうることは、同じ gate が `gate_probe.py` に
  cycle 2 + layer-violation + forbidden-dependency を要求する形で確認される。
- 負の対照: `MC_SPINE_DEPSGX` を環境に置いたまま実行すると `layers_gate_test` が 4 件失敗し、
  ランナーは `ASSERT FAILED` / exit 1 で止まった（`python-units-rerun/`）。集計 assert が
  **発火しうる**ことの実測で、テスト側の env 依存（§17）でもある。

suite の 92.52 s は、5A/5C の `[profile.test] opt-level = 1` によるもの。同じ chain が
記録した冷間 623.72 s と比較して **6.7 倍**だが、これは同一 SHA の前後比較ではないので
「623 s → 92 s になった」とは主張しない（profile 変更込みの 1 点測定）。

最終状態の確認: `git diff --stat bef7f53 HEAD -- src/ assets/ tools/ Cargo.toml` は空
（この報告 commit は `evidence/` だけを触る）。最終 HEAD `32dad84` 上でも full suite を
1 回実行し、**541 passed / 0 failed / 1 ignored / 92.22 s / exit 0**（`final-suite.log`）。

ただし `final-suite.log` には `start` / HEAD / `git status` / `EXIT=` の行が無い
（`full-suite.log` にはある）。したがって **その 1 本の HEAD 束縛は log 単体では確認できない**。
コード同一性は §0 の tree object 比較（`bef7f53` と `2799a36` の `src`/`assets`/`tools`/
`Cargo.toml` が同一）で別途成立しているので、数値は「`bef7f53` のコードに対する測定」として
有効だが、`final-suite.log` 単体を HEAD 束縛の証拠として引用しない。以後の suite は
stamp（start / HEAD / status / EXIT / finish）を必ず残す（§16-6）。

`--list` の 542 と suite の 541+1 ignored は一致する。`ignored` の 1 件は
`debt_ratchet.sh` が「parked gates: 1」として許容しているもの。

## 6. 変異の質（件数ではなく赤の中身）

`MUTATION_LOG_DIR` を設定して、変異時の FAILED assertion と復元後の re-green を保存した。

- mutant log 14 本すべてが **assertion failure**（`test result: FAILED`）で、`error[E…]` の
  コンパイルエラーは 0。つまり「ビルドが壊れて赤」ではない。
- restored log 14 本が存在し、いずれも green（復元制御）。
- 追加 entry は **15 件**（5C 3 / 6A 6 / 6B 1 / 6C 5）で、上の 14 本はそのうちの 14 件。
  **6B の `a traced column names the layer that answered it` は当時未実行**で、赤を誰も
  観測していなかった。2026-11-16 に focused で 1 回だけ実行し、assertion failure と
  restored green を保存した（§5 の表）。既存 14 本の log は 14 本のまま（数え直していない）。
- 0 件選択は合格にしない: `MUTATION_FILTER=no_such_test_filter_xyz_20261116` は
  `0 proved` / **exit 1**（`final-landing…/05b-mutation-zero-selection.log`）。
- 0 件選択・未実行・`#[ignore]` を成功として数えない実装（5A/5C の
  「filter が 0 件なら PASS ではない」修正）が入った上での実行。

新規 anchor の曖昧さ・死んだ anchor の確認（衝突解決後）:

- 423 entry 全件について anchor 文字列が対象ファイルに**ちょうど1回**存在することを
  自前の shell-word 走査で確認。0 回の entry は無い。
- 途中で 2 件が「不在」と出たが、いずれも `'"'"'` 連結引用の解析漏れで、bash で
  デコードすると 1 回存在した（`e7ead8e` 時点と同一、統合の影響なし）。
- 6A/6B/6C が触った `tools/mutation_check.sh` の自動マージで、名前重複も欠落も無い。

## 7. 前変更 module への再生（テストが変更を識別できるか）

**6C / `src/feature.rs`（disk, blue_ice）**: `src/feature.rs` を `b7ca4f5`（6C の直前）へ戻すと
**コンパイルは通る**（＝ API に依存しない純粋な挙動テスト）。結果:

| test | pre-6C feature.rs |
|---|---|
| `disk_walks_the_radius_box_in_between_closed_order` | **FAILED** `the column at (1,64,-1) must take the draw … left: 10 right: 1` |
| `blue_ice_spends_the_native_two_ints_per_attempt_stream` | **FAILED** `a candidate above sea level - 1 must not draw` |
| `attached_to_leaves_…` | PASS（このテストは `tree.rs` 側なので想定どおり） |
| `cocoa_binds_…` / `horizontal_plane_order_…` | PASS（同上） |

この「通ってしまった」は失敗ではなく、テストの所有 module を正しく示す負の対照である。

**6C / `src/tree.rs`**: `src/tree.rs` を `b7ca4f5` へ戻すと **コンパイルできない**
（`E0432: unresolved import crate::tree::horizontal_dir`、`hoff`/`run` の可視性）。
つまり `horizontal_plane_order_is_north_east_south_west` は新しい API を固定するテストで、
module 差し替えでは再生できない。この経路の識別力は §6 の変異
（`the horizontal plane is walked north, east, south, west` proved）で示した。
**module 差し替えで証明できたのは feature.rs 側 2 本、API 固定のため変異で証明したのが
tree.rs 側、という区別をここに明記する。**

## 8. 残差の指標（reference の同定・stage・footprint を明記）

reference はすべて pinned 1.21.1（bundler jar sha1 `59353fb4…` / 内側
`a8ae1f98…`）、seed 4242、dimension は各行のとおり。`--mode full`（structures 後）。

| fixture | コマンド | diffs |
|---|---|---|
| stronghold | `…/nat-4242-stronghold/world -96 35 --n 1` | **0 / 98304** |
| ruined-portal | `…/nat-4242-ruined-portal/world 6 7 --n 1` | **0 / 98304** |
| nether-fortress | `…/nat-4242-nether-fortress/world -25 6 --n 1 --preset nether` | **0 / 32768** |
| end-outer | `…/nat-4242-end-outer/world 10 10 --n 1 --preset end` | **0 / 32768** |
| village-taiga 3x3 | `…/nat-4242-village-taiga/world 4 -32 --n 3` | **4 / 884736** |
| nether-fossil 3x3（49-pop 記録なし） | `…/nat-4242-nether-fossil/world -25 19 --n 3 --preset nether` | 60 / 294912 |
| nether-fossil 3x3（**81-pop**） | `…/captures/t6b/nat-nf-str/world … --pop-order nat-nf-str/poporder.tsv --pop-order-scope recorded` | **4 / 294912**（air→red×2, air→brown×2, basalt 0） |
| jungle-temple | `…/nat-4242-jungle-temple/world 74 -41 --n 1` | **121 / 98304** |

読み方の注意:

- nether-fossil の 60 は pop order を与えない場合の値。6B が「32」と書いたのは
  49-pop の記録順を与えた場合で、別条件。**同じ条件の数字として並べない。**
- 81-pop の `4 / 294912` は当時の capture（`/tmp/captures`、`poporder.tsv`）に対する測定で、
  そのディレクトリは**現存しない**。同条件の再現はできず、**継承**として扱う（§16-6）。
- end-city (`65 45`) は比較不能（port の End dump は 0..128、city の purpur は y=136）。
  監査の記載どおりで、今回も実行していない。

## 9. 6A: sculk の FULL/CENTER（監査の未検証項目を jar で決着）

pinned official mappings (`server.txt` sha1 `03f89854…`) はこの host に無く
`tools/bytecode_probe.sh` は設計上 exit 2 で止まる。そこで**難読化名を挙動から同定**して
同じ pinned jar から決着した（詳細 `deepseek-int-20261116-125633/6A-sculk-support-type-verification.md`）。

- `SupportType` = 定数プールに `FULL`+`CENTER`+`RIGID` を持つ唯一のクラス = `doa`
- 3引数 `isFaceSturdy` = `dtb$a.d(dcc,jd,ji)`（本体が `doa.a`=FULL を積んで4引数へ委譲）
- `canSupportCenter` = `dfy.a(dcz,jd,ji)`（`doa.b`=CENTER）
- `SupportType.FULL.isSupporting`（`doa$1`）= `isFaceFull(state.l(level,pos), dir)` — 本体は
  近傍セルを読まないが、`l` は level/pos を取る衝突形状 accessor。**state 局所性は3世界の実測**
  で、live な block entity 位置は測定外（§3）
- `SculkShriekerBlock` = `dts.G`(`can_summon`) を参照する唯一の block = `dmt`。
  **`dmt` は `canSurvive` を宣言せず、`SupportType` も参照しない。**

| port site | native 本体 | 判定 |
|---|---|---|
| `spc_can_spread_from` | `SculkPatchFeature.canSpreadFrom` = `edi` | FULL 一致 |
| `gen_sculk_patch`(shrieker) | `SculkPatchFeature.place` = `edi` | FULL 一致 |
| `spc_cursor_can_spread` | `ChargeCursor.canSpread` = `dmu$a` | FULL 一致 |
| `spc_vein_can_spread` | `dkz$a` → `MultifaceBlock.canAttachTo` `dky.a` | **近似** |

近似の中身: native は `isFaceFull(collision) || isFaceFull(support)` の OR、port は
`Support::Full` の片方（collision）だけ。よって vein の site で port は reference より
**厳しい**。FULL/CENTER の取り違えではなく、6A 以前（`is_solid && side_solid`）より
緩い方向の変更でもない。**未修正。** この arm 2 の欠落は **6A が持ち込んだものではない**
（`787f781` の親でも片側だけ。独立レビューが `9fcb5da..787f781` の diff で確認）。記録のみで、
この pass では直さない。6A の README 残件3 を、この確定内容で更新した。

## 10. 6B: 主張の全数再測定（当時の一致。いまは継承）

| 主張 | 独立測定 | 一致 |
|---|---|---|
| Nether の `getRawBrightness` 464/464 = 0 | `nat-nf-str/run.log` の `MLIGHT` 464 行すべて `light=0` | ✓ |
| taiga 401/403 = 0（1と2が各1） | `nat-taiga-mlight/run.log` の 403 行 = 401×0, 1×1, 1×2 | ✓ |
| 28 writes / save 24 / 欠落4 = port の余分4 | FCELL mushroom(state 2089/2090) = 28、全て `chunk=-25,19 wrote=true`。save は red 12 + brown 12 = 24。余分4セルは native で全て `air` | ✓ |
| 81-pop で 4 diffs、basalt 0 | `4 / 294912`、air→red×2 + air→brown×2 | ✓ |
| NCEL 226 / 149 columns（両側） | native `NCEL` chunk(-25,22) = 226 / 149 distinct。port `CEL_TRACE=-25,22` = 226 | ✓ |
| 7 columns が 1-4 低い、top は 128 | 209 key 突き合わせで差分 7 sample / 7 column、delta −1,−1,−2,−3,−4,−4,−4、両側 `top=128` のみ | ✓ |
| `BEARDPIECE … nether_fossil start=-24,22 bb=[-384,88,366,-382,91,367]` | port `BEARD_TRACE=all` で同一行 | ✓ |
| `BEARD -25,22 rigids=1 junctions=0 interesting=73` | 同一行 | ✓ |
| reference の fossil start は (-22,20),(-22,22) のみ | native `structures.starts` を 9x9 走査 → 2 件。port は (-24,22) も | ✓ |

6B の capture は統合候補の source に結ばれている（`inputs.sha256` の
`Agent.java`=`ca4ab2df…`, `CarveLog.java`=`c2b7877a…`, `SculkLog.java`=`d7a36549…`,
`build_carve_agent.sh`=`2062866f…` が全て候補と一致、jar は sha256 `e3bc5569…`）。
ただし当時の capture と計器出力は `/tmp/captures` 配下で、**現存しない**（native fixture
`/tmp/nat-4242-nether-fossil` 本体は残るが `poporder.tsv` は無い）。引用された hash 値が
候補と一致することは独立レビューが確認したが、`inputs.sha256` のファイル自体は残っていない。
よって上の表は**継承主張**として読む（今回、再測定はしていない。§16-6）。

**主張しないこと**: 28 writes / 24 saved は「どの writer が最終的に消すか」を特定していない。
`hasPostProcess` / `postProcessGeneration` / lighting は依然として未測定の仮説である。
`UNINITIALIZED_RAW_BRIGHTNESS = 0` 単独では taiga が 4→5 に悪化するので再導入しない。

## 11. 6C: jungle 116→121 の受入判定

独立に測った事実:

1. native の順序は jar で **NORTH, EAST, SOUTH, WEST**（`ji$c.<clinit>` が
   `ji.c, ji.f, ji.d, ji.e` を積み、`ji.<clinit>` は `a=DOWN b=UP c=NORTH d=SOUTH e=WEST f=EAST`）。
   曖昧さは無い。→ 6C の変更は native と一致する。
2. 候補バイナリで **121 / 98304**。`horizontal_dir` だけ逆向きに戻すと **116 / 98304**。差 +5。
3. 121 側の内訳: `air -> oak_leaves` **9 セル**（x=1198..1199, y=73..75, z=-655/-656 付近、
   native は air = **port が新しく置いた差**）、`short_grass`/`fern -> air` 11 セル
   （うち 4 が新規に正しい）、残りは dripstone/deepslate 等（独立レビューが座標まで確認）。
4. **切り分け（継承・再証明不能）**: `horizontal_dir` は正しいまま cocoa decorator の walk
   **だけ**を旧配列に戻すと **116** に戻る、という記録がある。ただし revert した2変種の
   source は保存されておらず、before と cocoa-only の dump は **同一 sha256**
   （`70b3afe0…`）であるため、「cocoa だけ戻した」と「両方戻した」は保存物から区別できない。
   よって「+5 の全量が cocoa pass の描画消費に帰属」は**継承主張**である。
5. cocoa は **両側 0 セル**（port の dump と native の保存チャンクの双方で chunk (74,-41) の
   cocoa 状態は 0）。位置は動いていない（独立レビューも確認）。

判定: **native の意味修正（N,E,S,W）は ACCEPT / fixture 指標は HOLD**。この2つは別物である。
+5 は「正しい規則を、既に壊れている世界状態に適用した」結果だというのは**読み**であって
測定ではない（9 セルは新規の差）。east-edge の canopy 欠陥が直れば数字が下がる、も**仮説**で、
測定された原因ではない。座標を動かして数字を合わせることはしない。再測定の条件は
「east edge の樹が native と一致」。

## 12. FAIL / SKIP / NOT RUN（明示）

- **NOT RUN**: `tools/mutation_check.sh` の 423 entry 全通し。1 entry あたり baseline + mutant の
  2 ビルドが必要で、予算外。代わりに 6A/6B/6C/5C が追加した 15 entry のうち **14 を focused で
  proved** し、残る 6B の 1 件は 2026-11-16 に focused で proved した（§5・§6）。
  全通しの未実行を「green」とは扱わない。
- **NOT RUN**: `tests/verify.sh` の wire smoke / adapter check (`--features fabric-probe`,
  `neoforge-events`) / 8 スレッド suite ×3。今回の変更集合（worldgen の述語・順序・診断）に
  対して必要だが、時間予算を超える。上記 §5 の個別 gate と full suite 1 回で代替した。
- **NOT RUN**: end-city fixture（比較不能、§8）。
- **SKIP された変異**: 0（focused 実行内）。
- **環境依存**: `tests/layers_gate_test.py` は `depsgx` が PATH に必要。PATH 無しでは 16 失敗
  するが、それは環境の欠陥であってコードの欠陥ではない。2026-11-16 の再実行では
  `/home/yuu/gxkit/.venv/bin` を前置して **17/17 OK**（depsgx 1.0.0、§5.1）。ただし
  `MC_SPINE_DEPSGX` を export したままだと、このテストの「検査器が無い」系 fixture が
  環境変数を継承して別の理由で落ちる（実測 4 失敗、§5.1 の負の対照）。
  `tools/layers_check.sh` も depsgx を必要とする。

## 13. 残っている欠陥と次の作業（順序と完了条件）

1. **`MultifaceBlock.canAttachTo` の support-shape 腕が無い**（pre-existing、port が厳しすぎる。
   6A の導入ではない）。
   完了条件: `Support::Full` の vein site を `isFaceFull(collision) || isFaceFull(support)` に
   する契約テストが native の 2 面を区別して落ち、変異で赤、fixture が悪化しないこと。
   注意: 支持形状は asset に無いので、まず資産の追加輸出が要る。
2. **`nether_fossil` の余分な start `(-24,22)`**（6B §4）。完了条件:
   `NetherFossilStructure#findGenerationPoint` の妥当性/高さ判定を port の fossil arm と
   並べ、`(-24,22)` が消え、`(-22,20)`/`(-22,22)` が残り、jungle/taiga/stronghold が
   悪化しないこと。`BEARD_TRACE` で `BEARD -25,22 rigids=0` を確認できること。
3. **`hasPostProcess` / 初期化後 light の模型が無い**（6B §3）。完了条件:
   `hasPostProcess` オラクルと初期化 light の模型を入れ、shape 再検査と
   `UNINITIALIZED_RAW_BRIGHTNESS = 0` を**同時に**着地させ、taiga が 4 のままであること。
4. **jungle east-edge の canopy 欠陥**（6C §11）。完了条件: `x=1198..1199` の樹が native と
   一致し、jungle の diffs が 121 から 116 未満へ下がること。
5. **49-pop fixture の再生成**（6B §5）。完了条件: 残差を 49-pop で語る比較が消えるか、
   footprint artifact であると明記されること。

## 14. 文書の是正（この commit で行ったこと）

- `evidence/foundation/block-state-facts-20261116/README.md`: 残件3（sculk の FULL が未検証）
  を、jar からの決着内容（3/4 一致 + vein の OR 近似 + `SculkShriekerBlock` は `canSurvive` を
  持たない）に置き換え、節を追加。
- `evidence/gameplay/feature-parity/light-and-lost-writes-20261116.md`: 独立再測定の表と、
  証拠の強さの境界（port 側計器 / capture の source 束縛 / 最終 writer は未特定）を追記。
- `evidence/gameplay/feature-parity/collection-and-iteration-order-audit-20261116.md`:
  独立再測定、切り分け実験、受入判定（component ACCEPT / fixture 未達）を追記。

`evidence/foundation/module-layers/policy.json`、`deps_target.py`、
`policy.json` は触っていない。production module の追加、`#[ignore]` の追加、
assertion や golden output の弱体化はしていない。

## 15. 文書と実装の不一致（報告のみ、移行しない）

- `tools/carve-agent/CarveLog.java` の `overworld()` は、実際には `-Ddim` と
  `DIM[0]`（min Y）を比較する **min-Y セレクタ**。名前が示す dimension 判定ではない。
  Nether の `MLIGHT` が記録できているのはこのため（`-Ddim` 未設定なら true）。
  既存の計器の挙動なので、改名はこの pass では行っていない。
- `knowledge`: 6A README の「残件」にあった未検証前提は本 pass で解消したが、
  実装（`Support::Full` を vein site に使うこと）は変えていない。docs と code の
  どちらかに寄せる判断は上記 §13-1 の完了条件に委ねる。

## 16. 最終 remediation（2026-11-16, F1–F6）

独立レビュー `5dcaf3a` の指摘への是正。**production の意味は変えていない**: `src/` の変更は
`src/feature.rs` の docstring 1 箇所だけで、ほかは `evidence/` の文書。検証と生ログは
`/home/yuu/mc-spine-local/audit/final-landing-20261116-1415/`。

1. **F1（Python の件数）**: §5 の「37/37 exit 0」を撤回し、当時の log の実数（36 entry /
   35 exit 0 / `layers_gate_test` exit 1）と、変数だけの `layers_gate_exit=0` を明記した。
   同一条件の再実行は **37 ファイル / 37 exit 0**（unittest 196 ケース）、depsgx は 1.0.0
   （gxkit HEAD `973b293a…`、CI pin `d6b9ad8…` ではない）で、「pinned」とは主張しない（§5.1）。
   集計 assert は赤い実行で exit 1 になることを実測した（負の対照）。
2. **F2（未実行 mutation）**: 追加 15 entry のうち未実行だった 6B の 1 件を focused で proved。
   baseline green → mutant `test result: FAILED`（assertion、コンパイルエラー 0）→ restored
   green。既存 14 log は 14 のまま、新しい 3 本を別ディレクトリに保存（§5・§6）。
   0 件選択は exit 1。ただしこれは**1 回の観測**である: `MUTATION_STABILITY` は既定 off で、
   2 回目も赤いこと（flake でないこと）は確かめていない。stability 経路も再コンパイル済み
   バイナリと assertion の同一性を再検査しないので、1 回の赤を「stable な catch」とは
   主張しない（§17）。
3. **F3（FULL の文脈依存）**: §3 の「FULL は他セルを読まない ⇒ 文脈依存は明るさだけ」を
   撤回し、表の適用範囲を観測したキャッシュ済み/クエリ/生成済み state に限定。
   `moving_piston` 2063..2074 と live block entity が測定外であること、port が moving_piston を
   生成しないことを明記（§3・§9）。vein の arm 2 欠落は pre-existing と明記（§9・§13-1）。
4. **F4（feature.rs の内訳）**: 全面を DOWN に寄せて合計が合わない内訳を削り、面別の実数を
   持つ README を指すようにした。コメントのみ（`src/feature.rs` の docstring）。
5. **F5（6A README の accessor 表）**: `k`=1 / `l`=986 の表を**同定の根拠としては撤回**し、
   履歴として残した。`dtc.k` と `dtc.l` は同じ4引数計算でキャッシュの有無だけ、という
   bytecode の読みを併記し、`k` の採用は旧資産 0 不一致とアンカーで支えると書き直した。
6. **F6（receipt / 出所）**: `final-suite.log` に stamp が無いことを明記し、HEAD 束縛の証拠に
   しない。6C の AB 変種 source 未保存・before/cocoa-only dump 同一 sha、6B の
   `/tmp/captures` 不在を明記し、該当表を**継承**に落とした。§11 の「正しい規則 /
   east-edge が直れば改善」は**仮説**に限定した（9 セルは新規の差）。

## 17. テスト側の計測差（観測のみ。テストは変更していない）

- `tests/layers_gate_test.py` の fixture は `PATH` を差し替えるが、`MC_SPINE_DEPSGX` など
  他の環境変数は継承する。「検査器が無い」ことを確かめるテストが、operator の export で
  別の理由（`cargo` 不在）に落ちる（2026-11-16 実測: 17 中 4 失敗）。テストの verdict が
  環境に依存する。
- 同じ fixture は `CARGO_TARGET_DIR` が環境にあるとそれを使う（既定 `ROOT/target`）。
  operator の別ツリー向け target を共有しうる（今回の実行は自分の worktree の target）。
- `tools/mutation_check.sh` は復元ハッシュ不一致時に `git checkout -- "$mut_file"` する。
  これは**未コミットの編集を破壊しうる**経路である（テストが defect を誘発する形）。
- 元の F1 の誤集計は、`exit=` 行だけを出す inline ループに集計 assert が無かったことに
  よる。今回のランナーは `SCANNED == ATTEMPTED == ENTRY` / `PASS+FAIL == ATTEMPTED` /
  `FAIL == 0` を assert し、赤い実行で実際に exit 1 になることを示した（§5.1）。
- `tools/mutation_check.sh` は src を**その場で書き換える**。SIGKILL されると mutant と
  `.mutation-backup` が残る（次回起動は backup を見て exit 2 で拒む）。本 pass で過去の
  SIGKILL 残留が実際に起きたことは確認していない（未証明）。今回の変異実行は隔離 worktree と
  専用 target で、他のビルド・読み手を同時に走らせずに行った。canonical
  `/home/yuu/mc-spine` ではハーネスを実行していない。
- 同ハーネスの restored 制御は「復元後の再ビルド＋再 green」を記録するが、復元の検証は
  **source の sha256** までで、使われたバイナリの同一性は検査しない。今回の実行では追加で
  検証した（§18）: `src/*.mutation-backup` 0、`git status` 0 行、receipt のバイナリ sha256
  `8aba6f80…` は復元ログ（14:23:26）より**後**のビルド（14:29:14）で、変異で赤くなるテストが
  そのバイナリで green。mtime と挙動の両方で「mutant バイナリではない」ことを示す。

## 18. 最終候補の実測（2026-11-16, HEAD `aeea52a`）

最終候補 `aeea52ad…`（`2799a36` + 独立レビュー報告 + この remediation）上で、source-bound に
取り直した。すべて `/home/yuu/mc-spine-local/audit/final-landing-20261116-1415/`。

| 検証 | 結果 | ログ |
|---|---|---|
| `tools/layers_check.sh`（depsgx 1.0.0） | PASS（42 modules / 108 edges / probe 4 codes） | `gates-layers-debt-receipt.log` |
| `tools/debt_ratchet.sh` | 0 over limit（parked gates 1） | 同上 |
| `python3 tests/evidence_receipt_test.py` | 8/8 OK | 同上 |
| Python unit 37 ファイル | **37/37 exit 0**（unittest 196 ケース） | `python-units-rerun2/` |
| focused 契約テスト 11 filter（`tools/evidence_receipt.py`） | **11/11**: 各 filter が 1 test を実行して pass | `receipt/receipt.json` |
| `--list` | 542 tests | 同上 |
| full Rust suite 1 回 (`--test-threads=1`) | **541 passed / 0 failed / 1 ignored / 91.77 s / exit 0** | `final-suite.log`（start/HEAD/status/EXIT/finish 付き） |
| mutation（6B の 1 件） | baseline green → mutant FAILED → restored green。**1 回の観測**（stable とは主張しない） | `mutation-logs/` |

束縛:

- receipt の binary は `mc_spine-5263732b84b8ac33`、74,884,240 bytes、sha256
  `8aba6f80559f2aa393540279eee38e44c98b0f0d6efc3224d6a34319d6a21f47`。
  `source_at_start` と `source_at_finish` は HEAD `aeea52a` / dirty 0 で一致。
- 復元とバイナリの検証（`08-restore-and-binary-verification.txt`）: `src/*.mutation-backup` 0、
  `git status` 0 行、復元ログ 14:23:26 UTC 05:23:26 → receipt のビルド 05:29:14（後）、
  そのバイナリで変異対象テストが green。
- suite は同じ `CARGO_TARGET_DIR` と同じ HEAD で、ログに start / HEAD / status / EXIT /
  finish を残した（`final-suite.log`）。
- この remediation の後続 commit は docs のみで、`src` / `assets` / `tools` / `Cargo.toml` の
  tree object は変えない（実行 SHA と最終 docs HEAD で同一性を確認する）。
- **canonical への着地は親の指示で HELD**（FF の authorization は取り消された）。
  `/home/yuu/mc-spine` は `e7ead8e667…` / `perf/worldgen-parallel` / clean のまま触っていない。
