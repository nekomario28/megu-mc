# 独立レビュー — 統合候補 `bef7f53` / 最終 docs HEAD `2799a36`（Track 5A/5C + 6A + 6B + 6C）

状態: **独立レビュー完了。source 変更なし。判定は APPROVE (bounded) + 3 点の明示 HOLD（docs/gate、コードは不要）。**
対象と参照元は下の §0。数値はすべて本レビューが自分で走らせた測定か、SHA で束縛された保存ログに紐づく。
継承しただけの主張は §6 に分離した。

- レビュー用 checkout: `/home/yuu/.t3/worktrees/mc-spine/t3-bcaff3af-3038d8506b9c7851`（base `e7ead8e`、独自 branch）
- 読み取り専用で参照した候補 worktree: `/home/yuu/.t3/worktrees/mc-spine/t3-b2eb8ad1-ebf296b8314a2ff7`
- private log: `/home/yuu/mc-spine-local/audit/deepseek-int-20261116-125633/final-review-20261116-1351/`
  （`parse_tests.py` / `cmp_union.py` / `entries.py` / `cmp_mut.py` / `anchor_check.py` /
  `facts_crosscheck.py`+`.out` / `jungle_cells.py` / `jungle-cells-121.log` / `tests.json`）
- 資源: JVM・GPU・cargo ビルド・新規 capture なし。git / 保存 dump / 保存 javap / python のみ。

## 0. 対象

| 項目 | 値 |
|---|---|
| コード SHA | `bef7f531ab902e64d05ef04d339c9f8a796f0e68` |
| 最終 HEAD | `2799a36b416de9e5205067bee313fe6f95fd6664` |
| base | `e7ead8e66466b0b6467450cd3ea31d86a0e39cd3` |
| 統合元 | `9fcb5da`(5A/5C) / `f166588`→`787f781`(6A) / `085206b`→`b7ca4f5`(6B) / `864bf73`→`bef7f53`(6C) |
| レビュー対象文書 | `evidence/review/integration-acceptance-bef7f53-20261116.md`（318 行） |

**最上位の前提は独立に確認した**: `bef7f53..2799a36` の差分は docs 4 本のみで、
`src` / `assets` / `tools` / `Cargo.toml` の **tree object が同一**
（`bef7f53:src=e233316b65bb…` と `2799a36:src=e233316b65bb…`、assets/tools/Cargo.toml も同値）。
`32dad84..2799a36` は受入文書への +5 行のみ。したがって「コードは `bef7f53`、再現は最終 HEAD」は成立する。

## 1. 重大度順の所見

### F1 (medium・文書の数値矛盾) — §5 の「Python 37/37 exit 0」は引用ログと一致しない

`evidence/review/integration-acceptance-bef7f53-20261116.md` §5 の行:

```
| Python unit 37 本（`tests/verify.sh` の一覧） | 37/37 exit 0 | `python-units.log` |
```

実測（`python-units.log` を機械的に数えた）:

- entry 数 **36**（`^[a-z0-9_]+ exit=` = 36）、うち `exit=0` は **35**。
- `layers_gate_test exit=1` → `Ran 17 tests … FAILED (failures=16)`、`TOTAL_FAILURES=1`。
- 末尾の `layers_gate_exit=0` は変数だけで、**合格再実行の出力は保存されていない**。
- `tests/verify.sh` の 37 本のうち `gate_scripts.py` は `python-units.log` に無く、
  `static-gates.log` 側で PASS している（`gate_scripts.log` の 6 self-test PASS を確認）。

つまり「37 本のうち 36 本がこの log、その中の 1 本が exit 1」であり、`37/37 exit 0` は誤り。
§12 の depsgx 注記は正しいが、§5 の表の行が残っているため読み手は 37/37 と読む。

置換案（§5 の当該行をそのまま差し替え）:

```
| Python unit（`tests/verify.sh` の 37 本。`gate_scripts.py` は `static-gates.log` 側） | `python-units.log` は 36 本 = 35 exit 0 + `layers_gate_test` exit 1（depsgx 不在で 16 failures）。`/home/yuu/gxkit/.venv/bin` を前置した再実行で 17/17 OK（当該再実行の出力は保存されておらず、log 末尾の `layers_gate_exit=0` のみ） | `python-units.log`, `static-gates.log` |
```

### F2 (medium・gate 被覆) — 追加 mutation は 15 件、proved は 14 件。6B の 1 件が未実行

`tools/mutation_check.sh` の entry を機械的に突き合わせた結果（`entries.py` / `cmp_mut.py`）:

- entry 総数 `e7ead8e`=408 → 候補=**423**、重複名 0。追加は **15 名**
  （5C 3 / 6A 6 / 6B 1 / 6C 5。候補−base の差 15 と一致）。
- `mutations-summary.log` の `ok` 行は **14 名**。差分は 1 件:

```
6B: "a traced column names the layer that answered it" \
     src/generation_context.rs '  } else if self.rc.contains_key(&(x, y, z)) {\n      "rc"' \
     '  } else if self.rc.contains_key(&(x, y, z)) {\n      "terrain"' \
     feature::generation_contract_tests::cel_column_trace_tags_the_layer_that_answered
```

この entry は anchor も対象ファイルに 1 回存在し、テストも full suite で通っている（`--list` 542）。
しかし **mutant log が無く、赤くなることを誰も観測していない**。§12 の
「6A/6B/6C/5C が追加した 14 entry を focused で proved」は、追加集合の被覆としては 1 件不足。

置換案（§5 の当該行 + §12 の 1 文）:

```
| focused mutation | 追加 entry は 15（5C 3 / 6A 6 / 6B 1 / 6C 5）。focused で proved したのは 14。6B の `a traced column names the layer that answered it`（`src/generation_context.rs`）は未実行 |
```

```
- 代わりに 5C/6A/6C が追加した 14 entry と、既存 2 件の anchor 付け替えを focused で proved した。
  6B の 1 entry は未実行（上表）。
```

### F3 (medium・推論の範囲) — 「FULL は他セルを読まない、ゆえに文脈依存は明るさだけ」は測定範囲を超える

`integration-acceptance-bef7f53-20261116.md` §3:

> 独立レビューでも `SupportType.FULL.isSupporting` の本体が
> `isFaceFull(getCollisionShape(level,pos), dir)` のみで、level の他セルを読まないことを確認した。
> したがって **文脈依存なのは `getRawBrightness` の側だけ**であり…

自分で bytecode を読み直した結果（`jar-work/` の保存 javap、追加 capture なし）:

| 事実 | 出所 |
|---|---|
| 3 引数 `isFaceSturdy` = `dtb$a.d(dcc,jd,ji)` が `doa.a`(FULL) を積んで 4 引数へ委譲 | `dtb$a` javap |
| `doa$1`(FULL) 本体 = `dfy.a(state.l(level,pos), dir.getOpposite())` | `doa$1` javap |
| `dtc.l` = `dtb.b_` → `dtb.b(state,level,pos,CollisionContext.empty())`（level/pos を取る衝突形状 accessor） | `dtb`,`dtb$a` javap |
| `dtc.k`（表の `is_collision_empty` の元）= state cache があればそれを、無ければ**同じ 4 引数** | `dtb$a.k` javap |

つまり本体は自分では近傍を読まないが、**level を取る形状 accessor に委譲している**。
state 局所性は「3 つのスタブ世界（stone / air / `EmptyBlockGetter`）で 0 差分」という
**実測**で示された性質で、block entity を持つ位置はその測定の外側にある。具体値:
`moving_piston`（state 2063..2074）は表で `blocksMotion|colEmpty|fluidEmpty`、FULL/CENTER/RIGID すべて空
（= `EmptyBlockGetter` の答え）。native の live な moving piston は運搬 block の形状を返すので
FULL 面があり得る。当 port は moving_piston を**生成しない**（`src/main.rs:11019` は名前→id、
`src/feature.rs:3399` は判定用の名前一致のみ、piston シミュレーション無し）ため生成経路では到達しない。
FULL 列の値そのものは旧資産と 0 差分（§4）なので、これは**値の移動ではなく主張の範囲**の問題。

置換案（§3 の当該段落）:

```
- `SupportType.FULL.isSupporting`（`doa$1`）の本体は `isFaceFull(state.l(level,pos), dir)` で、
  自分では近傍セルを読まない。ただし `l` は level を取る衝突形状 accessor なので、
  state 局所性は **3 つのスタブ世界での実測**（block entity を持たない世界）で示された性質である。
  block entity を持つ位置（表では `moving_piston` 2063..2074 が colEmpty・FULL 面なし）は
  この測定の外側だが、当 port は moving_piston を生成しないため生成経路では到達しない。
  したがって「表で解けない残差は明るさ側だけ」は *測った範囲で* と言い切れる、という形に限定する。
```

### F4 (low・コメントの数値) — `face_sturdy` の内訳が表と一致しない

`src/feature.rs:3579-3592`（787f781 で追加された docstring）:

> …denied the support under a repeater (64 states), scaffolding (32), a comparator (16),
> a snow layer (13), a ladder (8) and every carpet (1 each) **on the DOWN face alone**.

pinned 表から自分で数えた実値:

| 面 | 合計 | 内訳 |
|---|---|---|
| DOWN | **105** | repeater 64 / comparator 16 / snow 8 / carpet 17 |
| UP | **35** | scaffolding 32 / snow 1 / azalea 1 / flowering_azalea 1 |
| N,S,W,E | **各 3** | ladder 2 + snow 1 |

scaffolding は UP、ladder は側面、snow は 8(下)+1(上)+4(側)。docstring は全面を DOWN に寄せている
（合計 133 で 105 と合わない）。README 側の面別表（105/35/各3）は正しい。

置換案（`src/feature.rs` の当該 3 行）:

```
/// The body used to be `is_solid(st) && side_solid(st, dir)`, which added a `blocksMotion`
/// gate vanilla does not have: measured against the pinned table that denied the support on
/// DOWN 105 states (repeater 64, comparator 16, snow 8, 17 carpets), UP 35 (scaffolding 32,
/// snow 1, azalea 1, flowering_azalea 1) and 3 per side face (2 ladder + 1 snow). Shared by …
```

### F5 (low・文書と bytecode の不一致) — 6A README の「`k`=1 / `l`=986」表は accessor の差を説明しない

`evidence/foundation/block-state-facts-20261116/README.md` の識別表は
「`k` → 衝突形状リストと 1 不一致 / `l` → 986 不一致」として `k` を採用したと書く。
しかし bytecode では `dtc.l` → `dtb.b_` → `dtb.b(...,CollisionContext.empty())`、
`dtc.k` → (cache があれば cache) それ以外は**同じ 4 引数**であり、両者は同じ計算（cache の有無だけ）。
1 と 986 の差はこの bytecode では説明できない。値の回帰ではない（§4 の 0 差分、旧資産との一致）が、
「`l` は衝突形状ではない」という根拠としてこの表を引用しないほうがよい。
**HOLD（記録の再確認。コードは触らない）**。

### F6 (low・receipt) — 最終 suite と AB 変種の出所が保存物だけでは閉じない

- `final-suite.log` に `start` / HEAD / `git status` / `EXIT=` の行が無い（`full-suite.log` にはある）。
  §5 の「最終 HEAD `32dad84` 上でも 541/0/1」は、この log 単体では HEAD を確認できない
  （コード同一性は §0 の tree 比較で別途成立）。
- 6C の AB は「before(116)」「cocoa のみ(116)」「after(121)」の 3 バイナリ
  （sha `fdcab985…` / `be5954f4…` / `6e3109e0…`）で走っているが、
  **revert した 2 変種の source は保存されていない**（保存物は `tree.rs.pre-revert` = 候補そのもの、
  `tree.rs.orig.sha256` = `cc55fc87…` = 候補 HEAD の `src/tree.rs`）。
  さらに before と cocoa-only の dump は **sha が同一**（`70b3afe0…`）で、
  「cocoa だけ戻した」と「両方戻した」を保存物から区別できない（不活性な編集と無効果な結果は同形）。
  したがって「+5 の全量が cocoa pass に帰属」は継承主張であり、独立再現は不能。
- `/tmp/captures` は現存しない → §10 の 6B 再測定表（MLIGHT 464、NCEL 226/149、poporder.tsv）は
  いま再検証できない（native fixture `/tmp/nat-4242-nether-fossil` 自体は現存）。

### F7 (info) — 6C 監査ページの mappings 前提はこの host では満たせない

`collection-and-iteration-order-audit-20261116.md` は `/var/tmp/mcm/server.txt`（sha1 `03f89854…`）を
前提に `tools/feature_order_scan.py` の出力表を持つ。当該ファイルは現存せず（`/var/tmp/mcm` には
`b/` と `dki.javap.txt` のみ）、`tools/bytecode_probe.sh` も exit 2。統合報告 §9 はこの制約を
正しく書いているので矛盾ではないが、Table A は**継承**として扱う必要がある。

### 追加の観測（欠陥主張ではない・親の判断材料）

`src/tree.rs` の cocoa arm は `logs.iter().filter(|p| p.1 - ty <= 2)` で log を絞る。
6C 監査の F5 bytecode 列（`egy.a` の 27: `nextFloat` / 39: `getOpposite` / 63: `isAir` /
72: `setBlock{AGE=nextInt(3), FACING=dir}`）にはこの絞り込みが現れない。
native が全 log を歩くなら、絞った分だけ draw 数が変わる（= draw-count cascade の別経路）。
**未確認**（bytecode の該当ループ本体を読んでいない）。6C の受入判定そのものは変えないが、
「cocoa は 0 セルなのに世界が動く」機構を語るなら、この絞り込みの native 対応を一度確認したい。

## 2. 問い 1（conflict 解決は意図した union と等価か）→ **YES（測定で支持）**

`src/generation_contract_tests.rs` を fn 単位でパースし、本文を SHA で比較した（`parse_tests.py` / `cmp_union.py`）。

- 候補 `bef7f53`: 132 test、名前重複 0、`#[test]` は全て column 0（入れ子なし）。
- base `e7ead8e`: 121。追加は 11 で、**11 件すべて候補にちょうど 1 回**存在し、
  各 source commit の追加版と**本文 SHA が一致**（`PRESENT-same`）。

| source | 追加 | 候補での状態 |
|---|---|---|
| 5C `9fcb5da` | `cel_layer_trace_lists_exactly_the_layers_cel_find_y_walks`, `nether_mushrooms_survive_a_solid_render_support` | 一致 |
| 6A `f166588` | `face_support_answers_the_support_type_the_caller_asks_for`, `pinned_fact_tables_agree_with_the_older_oracle_assets`, `state_facts_answer_the_predicates_vanilla_separates` | 一致 |
| 6B `085206b` | `cel_column_trace_tags_the_layer_that_answered` | 一致 |
| 6C `864bf73` | `attached_to_leaves_draws_its_probability_before_the_required_empty_check`, `blue_ice_spends_the_native_two_ints_per_attempt_stream`, `cocoa_binds_its_four_probability_draws_to_north_east_south_west`, `disk_walks_the_radius_box_in_between_closed_order`, `horizontal_plane_order_is_north_east_south_west` | 一致 |

- **既存 test の改訂 6 件**（`mushrooms_in_fresh_generation_…`(5C)、`bonus_chest_…`、
  `huge_brown_…`、`huge_red_…`、`live_heightmap_types_…`、`multiface_cells_…`(6A)）も、
  候補は **source 側の改訂版と一致**（base 版とは異なる）。片側だけ残った取り違えは無い。
- base test の欠落 0 / 本文変化はこの 6 件のみ。121 + 11 = 132 で数も合う。
- mutation entry: 423 件・重複名 0。追加 15 件は (name, file, old, new, filter) の 4 field が source と一致。
  base 由来 entry は「source 自身が改訂した 5 件」以外は不変。**anchor 一意性は
  機械抽出できた 392 entry 全部でちょうど 1 回**（0 回・複数回なし。残り 31 件は
  引用の入れ子で私のパーサが 4 field に割れなかったもので、報告側の 423 全件走査を置き換えない）。
- 「末尾 `}` の共有」で問題になりうる入れ子・二重閉じは、上記の「全 `#[test]` が column 0」
  「132 件が全て本文一致」で否定できる（ファイルは実際にコンパイルし 542 を list する）。

**結論: union と等価。** ただし F2 の 1 entry は「追加されたが赤を観測していない」。

## 3. 問い 2（各 consumer は state-local な事実を読む資格があるか）→ **PARTIAL（値は支持、主張は要限定）**

### 3.1 旧資産と pinned 表は値として一致する（自分で再計算した）

`assets/block_state_facts_1211.bin`（26,684 × 5 byte）を旧 4 資産と全状態で突き合わせ（`facts_crosscheck.py`）:

| 列 | 不一致 |
|---|---|
| `block_predicates_1211.bin` bit0 = isSolidRender | **0** |
| `collision_shape_full_1211.bin` | **0** |
| `mineshaft_support_1211.bin` の FULL 6bit | **0** |
| 同 CENTER(down) bit | **0** |
| 状態数（表 = 26,684 = `blocks_1211.json` の id 空間） | 一致 |

README の分離表も同値で再現: `canOcclude && !isSolidRender` **14,747**、
`liquid` vs `!getFluidState().isEmpty()` **9,212**、`blocksMotion` vs `!collisionEmpty` **2,812**、
`RIGID != FULL` **27**、FULL vs CENTER は DOWN **10,621** / UP **9,940** / N **5,923** / S **5,919** / W **5,923** / E **5,919**。
→ 787f781 は**値の移動ではなく所有者の移動**であり、consumer の変更点は合成（composition）側。

### 3.2 FULL/CENTER の呼び出し地点（bytecode で確認したもの）

- `dtb$a.d(dcc,jd,ji)` が `doa.a` = FULL を積む（3 引数 `isFaceSturdy` の意味）→ port の
  `face_sturdy(..., Support::Full)` は native の 3 引数 overload と一致（`feature.rs:3589`）。
- `dky.a(dcc,ji,jd,dtc)`（= `MultifaceBlock.canAttachTo`）の本体は
  `isFaceFull(state.l(level,pos), dir.getOpposite())` **OR** `isFaceFull(state.k(level,pos), dir.getOpposite())`。
  port の vein site（`spc_vein_can_spread`）は `Support::Full` **片方だけ**（= native arm 1）で、
  arm 2 が無い。**これは 6A 由来ではなく pre-existing**: 787f781 の親では
  `is_solid(st) && side_solid(st, dir)` だった（`git diff 9fcb5da..787f781`）。6A は余分な
  `blocksMotion` ゲートを外して native arm 1 に一致させただけで、arm 2 は元から無い。
  §13-1 の「未修正・記録のみ」は正しい区分。
- `dmt`（`SculkShriekerBlock`）は `doa`（SupportType）を参照せず、`canSurvive` も宣言しない
  → 「3 FULL サイト + Shrieker.canSurvive 無し」は成立。
- 資格の限界は F3: FULL は level 付き accessor 委譲なので、state 局所性は実測（3 世界）の範囲。
  BE 位置（moving_piston）は表で空形状、port は生成しないため生成経路では未到達。
- 追加の注意（未修正の挙動差・pre-existing）: `canSupportCenter` は native では
  `#minecraft:unstable_bottom_center` の UP 例外を持つが、port の `Support::Center` site
  （`src/feature.rs:4295` のボーナスチェスト松明）にその例外は無い。当該タグは
  `dp_tag_files` の実データに存在しないため `in_tag` は常に false（`block_registry.rs:763` の
  mineshaft chain anchor の除外も同様に不活性）。データが無いので**現状は実装不能**で、
  6A の変更が持ち込んだものではない（記録のみ）。

## 4. 問い 3（6C の分割判定）→ **native 意味修正は ACCEPT / fixture 指標は HOLD**

- native 順序は bytecode で確定: `ji$c.<clinit>` の HORIZONTAL = `[ji.c, ji.f, ji.d, ji.e]`、
  `ji.<clinit>` の名前 = `a=DOWN b=UP c=NORTH d=SOUTH e=WEST f=EAST`
  → **NORTH, EAST, SOUTH, WEST**（自分で `ji.javap.txt` / `ji_c.javap.txt` から再構成）。
  port の `horizontal_dir` / `plane_horizontal` / cocoa walk はこれに一致。
- 121 と 116 は保存ログにある（after 121 = binary `6e3109e0…`, dump `06f87230…`、
  before/cocoa-only 116 = `fdcab985…`/`be5954f4…`, dump `70b3afe0…`）。
- **cocoa は両側 0 セル**を自分で測った: native region を `chunk_triage.load_native_chunk` で復号し、
  保持 dump と cell 単位比較 → chunk (74,-41) の cocoa は native 0 / port 0、
  差分は **121 ちょうど**（報告と一致）。
- 121 の内訳（自分で座標まで出した）:

| 差分 | セル |
|---|---|
| port 余分な leaves **9** | `(1198..1199, 73..75, -655..-656)` — native は air |
| port が欠く canopy **52 + 1 log** | `(1197..1199, 76..81, -648..-653)`、log は `(1199,76,-650)` |
| その他 | dripstone/deepslate/stone、pointed_dripstone、short_grass/fern 等 |

→ **+5 の 9 セルは「正しい規則を壊れた canopy に当てた」産物ではなく、port が native に無い
leaves を置いた新しい差分**。監査ページ自身も「116 + 9 − 4 = 121、draw-count cascade」と書いており、
統合報告 §11 の「正しい規則を既に壊れている世界に適用した結果」という解釈は測定量を超える。
「east edge の樹を native に合わせれば jungle は改善する」は**仮説**で、AB は数値の移動しか示していない。
- 「cocoa が 0 セルでも RNG/状態は下流に効く」は機構として成立（`nextInt(3)` は `isAir` の内側、
  hit の向きが変われば draw 数が変わり後続 decorator がずれる）。ただし F6 のとおり
  revert 変種の source と before dump は保存されておらず、切り分けは継承。

**推奨**: 6C の **native 意味修正（N,E,S,W）は source-verified として ACCEPT**し、
**fixture 指標（jungle 116→121）は HOLD**（改善/悪化を主張しない）。6C 全体の rollback・
座標合わせ・golden 変更はしない。独立に受け入れられる部分集合は「順序の所有者を 1 つにする」
「cocoa/trunk/cherry が同じ配列を読む」までで、数値の解釈は含めない。

## 5. 自分で再検証した範囲（と、その手段）

再検証（本セッションで実行）:
1. docs-only 最上位差分（tree object 同一性）と `32dad84..2799a36` = +5 行。
2. test union（132 / 11 追加 / 6 改訂 / 欠落 0 / 重複 0 / 入れ子 0 / 本文 SHA 一致）。
3. mutation entry（423 / 重複 0 / 追加 15 / 4 field 一致 / 392 anchor 一意）。
4. 保存ログの digest 7 本が報告値と一致（`full-suite` `fa4c2203…` 等）。`full-suite.log` は
   HEAD=`bef7f53`・dirty 無し・541 passed / 0 failed / 1 ignored（92.52s）。
   `--list` 542。ignored 1 件は `tick_tests::profile_game_tick`。focused 10/10 exit 0。
   static-gates: 42 modules / 108 edges / debt 0 over limit / gate self-test 6 PASS。
   mutant 14 本すべて `test result: FAILED` かつ `error[E` 0（assertion で赤）、restored 14 本。
5. 資産クロスチェック（§3.1）と分離カウントの再計算。
6. bytecode（`doa$1` / `dtb$a.d,r,k,l,b` / `dtb.b_` / `dky.a` / `dmt` / `ji` / `ji$c`）。
7. jungle 121 の cell 単位再現と cocoa 0 の両側確認（§4）。
8. `tools/carve-agent/Agent.java` = `ca4ab2df…` / `CarveLog.java` = `c2b7877a…` が
   `b7ca4f5` 以降の候補と一致（報告 §10 の source 束縛の**引用値**は正しい）。

## 6. 継承のみ（再検証していない主張）

- 6B の 10 行再測定表（MLIGHT 464/464、taiga 401/403、28 writes/24 saved、NCEL 226/149、
  beard 行）と 81-pop の 4 diffs — `/tmp/captures` 不在。native fixture は現存するが
  `poporder.tsv` が無いため同条件の再現は不可。
- oracle の「3 世界で 0 差分」（JVM を起こしていない）。表そのものは §3.1 で別経路から検証した。
- capture の `inputs.sha256` 内容（ファイル自体が不在。引用された hash が候補と一致することは確認）。
- `feature_order_scan.py` の Table A（mappings 不在）。
- 6C の before/cocoa-only 変種（source 未保存）。

## 7. NOT RUN（明示）

- `tools/mutation_check.sh` 423 entry の全通し（1 entry = 2 ビルド）。6B の 1 entry も未実行。
- `tests/verify.sh` の wire smoke / adapter check / 8 スレッド suite。
- `cargo test`（full/部分いずれも。cargo ビルドは一切していない）。
- end-city fixture、JVM オラクル再輸出、`tools/bytecode_probe.sh`（exit 2）、
  新規 capture、`tools/port_triage.py` の再実行（保持 dump を使用）。

## 8. 着地の推奨（親が実行できる形）

**APPROVE (bounded)**: コード `bef7f53`（+ docs `2799a36`）は canonical へ着地してよい。
根拠は §2（union 等価）、§3.1（値の移動なし・0/26,684）、§5（ログ digest と suite の束縛）、
§0（docs-only の最上位差分）。

着地と同時に **HOLD / 是正**（いずれもコード変更不要。親の判断で文書のみ）:

1. F1 §5 の Python 行を実測どおりに（置換文は F1）。
2. F2 追加 mutation の被覆を 14/15 と明記するか、6B の 1 entry を focused で 1 回だけ proved にする。
3. F3 §3 と F4 `src/feature.rs` docstring の断定を「測った範囲」に限定し、内訳数値を直す。
4. F5 6A README の accessor 識別表は再確認まで引用しない。
5. §11 の「正しい規則 / east-edge が直れば改善」を、§4 の cell 内訳（port 余分 9 leaves、
   欠落 52+1）に合わせた記述に差し替える。
6. F6: 以降の suite は `full-suite.log` と同じ stamp（start / HEAD / status / EXIT / finish）を必ず残す。
   AB をするなら revert 変種の source と両 dump を保存する。

残件（§13 の 5 項目）は本レビューでは触らず、そのまま未完了として扱う。
