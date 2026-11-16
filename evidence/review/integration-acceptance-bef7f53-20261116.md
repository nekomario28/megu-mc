# Integration acceptance — Track 5A/5C + 6A + 6B + 6C

状態: **統合候補を構築し、実 SHA 上で検証した。canonical checkout へは未着地。**
この文書は索引であり、置き換えではない。数値はすべて下記の実行ログに紐づく。

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

- `assets/block_state_facts_1211.bin` は **state の性質**。`initCache()` が構築時に解決するため
  近傍に依存しない（6A が stone/air/`EmptyBlockGetter` の3世界で全列 0 差分を測定）。
  独立レビューでも `SupportType.FULL.isSupporting` の本体が
  `isFaceFull(getCollisionShape(level,pos), dir)` のみで、level の他セルを読まないことを確認した。
- したがって **文脈依存なのは `getRawBrightness` の側だけ**であり、6B が扱う残差の
  明るさ半分は表では解けない（6B の結論どおり）。
- 3つの空/石世界での all-state 一致は「任意の文脈で同じ」ことの証明にはならない。
  今回のレビューはその主張をしていない。表の妥当性は
  (a) `initCache` 由来の state 局所性、(b) 26,684状態の再輸出一致、(c) 呼び出し地点ごとの
  native 本体の同定、の3点で支えている。

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
| Python unit 37 本（`tests/verify.sh` の一覧） | 37/37 exit 0 | `python-units.log` |
| `tests/layers_gate_test.py`（17 tests） | OK（**`depsgx` が PATH に必要**） | 同上 |
| `tools/block_state_facts_from_jar.py`（`--check` 相当） | **26,684 states match**（pinned jar sha1 `59353fb4…`） | `exporter-check.log` |
| focused mutation 14 entry | **14 proved / 0 skipped / 0 parked / 0 not-a-gate** | `mutations-summary.log`, `mutation-logs/` |

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

suite の 92.52 s は、5A/5C の `[profile.test] opt-level = 1` によるもの。同じ chain が
記録した冷間 623.72 s と比較して **6.7 倍**だが、これは同一 SHA の前後比較ではないので
「623 s → 92 s になった」とは主張しない（profile 変更込みの 1 点測定）。

`--list` の 542 と suite の 541+1 ignored は一致する。`ignored` の 1 件は
`debt_ratchet.sh` が「parked gates: 1」として許容しているもの。

## 6. 変異の質（件数ではなく赤の中身）

`MUTATION_LOG_DIR` を設定して、変異時の FAILED assertion と復元後の re-green を保存した。

- mutant log 14 本すべてが **assertion failure**（`test result: FAILED`）で、`error[E…]` の
  コンパイルエラーは 0。つまり「ビルドが壊れて赤」ではない。
- restored log 14 本が存在し、いずれも green（復元制御）。
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
- end-city (`65 45`) は比較不能（port の End dump は 0..128、city の purpur は y=136）。
  監査の記載どおりで、今回も実行していない。

## 9. 6A: sculk の FULL/CENTER（監査の未検証項目を jar で決着）

pinned official mappings (`server.txt` sha1 `03f89854…`) はこの host に無く
`tools/bytecode_probe.sh` は設計上 exit 2 で止まる。そこで**難読化名を挙動から同定**して
同じ pinned jar から決着した（詳細 `deepseek-int-20261116-125633/6A-sculk-support-type-verification.md`）。

- `SupportType` = 定数プールに `FULL`+`CENTER`+`RIGID` を持つ唯一のクラス = `doa`
- 3引数 `isFaceSturdy` = `dtb$a.d(dcc,jd,ji)`（本体が `doa.a`=FULL を積んで4引数へ委譲）
- `canSupportCenter` = `dfy.a(dcz,jd,ji)`（`doa.b`=CENTER）
- `SupportType.FULL.isSupporting` = `isFaceFull(getCollisionShape, dir)` — **state 局所**
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
緩い方向の変更でもない。**未修正。** 6A の README 残件3 を、この確定内容で更新した。

## 10. 6B: 主張の全数再測定（すべて一致）

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

**主張しないこと**: 28 writes / 24 saved は「どの writer が最終的に消すか」を特定していない。
`hasPostProcess` / `postProcessGeneration` / lighting は依然として未測定の仮説である。
`UNINITIALIZED_RAW_BRIGHTNESS = 0` 単独では taiga が 4→5 に悪化するので再導入しない。

## 11. 6C: jungle 116→121 の受入判定

独立に測った事実:

1. native の順序は jar で **NORTH, EAST, SOUTH, WEST**（`ji$c.<clinit>` が
   `ji.c, ji.f, ji.d, ji.e` を積み、`ji.<clinit>` は `a=DOWN b=UP c=NORTH d=SOUTH e=WEST f=EAST`）。
   曖昧さは無い。→ 6C の変更は native と一致する。
2. 候補バイナリで **121 / 98304**。`horizontal_dir` だけ逆向きに戻すと **116 / 98304**。差 +5。
3. 121 側の内訳: `air -> oak_leaves` 9 セル（x=1198..1199, y=73..75, z=-655/-656 付近）、
   `short_grass`/`fern -> air` 11 セル（うち 4 が新規に正しい）。
4. **切り分け**: `horizontal_dir` は正しいまま cocoa decorator の walk **だけ**を旧配列に戻すと
   **116** に戻る。+5 の全量が cocoa pass の描画消費に帰属し、trunk の `getRandomDirection` は
   この fixture に寄与しない。監査の機構説明と一致。
5. cocoa は **両側 0 セル**（port の dump と native の保存チャンクの双方で chunk (74,-41) の
   cocoa 状態は 0）。位置は動いていない。

判定: **コンポーネントとして ACCEPT / fixture 指標としては未達**。この2つは別物である。
+5 は「正しい規則を、既に壊れている世界状態に適用した」結果であり、east-edge の canopy 欠陥
（116 の既存 diff、x=1198..1199 に樹が丸ごと無い）が直るまで jungle の数字は改善しない。
座標を動かして数字を合わせることはしない。再測定の条件は「east edge の樹が native と一致」。

## 12. FAIL / SKIP / NOT RUN（明示）

- **NOT RUN**: `tools/mutation_check.sh` の 423 entry 全通し。1 entry あたり baseline + mutant の
  2 ビルドが必要で、予算外。代わりに 6A/6B/6C/5C が追加した **14 entry を focused で proved**。
  全通しの未実行を「green」とは扱わない。
- **NOT RUN**: `tests/verify.sh` の wire smoke / adapter check (`--features fabric-probe`,
  `neoforge-events`) / 8 スレッド suite ×3。今回の変更集合（worldgen の述語・順序・診断）に
  対して必要だが、時間予算を超える。上記 §5 の個別 gate と full suite 1 回で代替した。
- **NOT RUN**: end-city fixture（比較不能、§8）。
- **SKIP された変異**: 0（focused 実行内）。
- **環境依存**: `tests/layers_gate_test.py` は `depsgx` が PATH に必要。PATH 無しでは 16 失敗
  するが、それは環境の欠落であってコードの欠陥ではない（`/home/yuu/gxkit/.venv/bin` を
  前置して 17/17 OK）。`tools/layers_check.sh` も同じ前提。

## 13. 残っている欠陥と次の作業（順序と完了条件）

1. **`MultifaceBlock.canAttachTo` の support-shape 腕が無い**（6A 由来、port が厳しすぎる）。
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
