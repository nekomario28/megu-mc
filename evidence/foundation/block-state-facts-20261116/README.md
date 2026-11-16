# Track 6A — 26,684状態の物理述語表を pinned 1.21.1 jar から輸出し、合成していた判定を1つの所有者に寄せる

## 何を作ったか

- `tools/block-state-facts-oracle/BlockStateFactsOracle.java` + `tools/block_state_facts_from_jar.py`
  で、pinned jar（SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`）から **26,684状態 × 5バイト =
  133,420バイト** の `assets/block_state_facts_1211.bin` を決定的に輸出する。
- `BReg` に O(1) の読み口を置き、**この表を物理述語の唯一の所有者**にした:
  `is_air` / `blocks_motion`（= `is_solid`）/ `can_occlude` / `is_solid_render` /
  `is_liquid`（流体状態あり）/ `is_liquid_block`（native `liquid()`）/ `is_collision_empty` /
  `is_collision_shape_full_block` / `luminance_of` / `opacity_of` /
  `is_face_sturdy(st, dir, Support::{Full,Center,Rigid})` / `side_solid`（= FULL）/ `face_plane`。
- 34列の内訳は flags 8ビット、`lightEmission<<4 | lightBlock` 8ビット、
  `isFaceSturdy` 6方向 × `SupportType` 3種 18ビット。

## 表が「状態の性質」であることの測定

`BlockStateBase` は `initCache()` で全述語を解決するので、近傍に依存しないはずである。
3つの世界（至る所 stone / 至る所 air / `EmptyBlockGetter.INSTANCE`）で全26,684状態を走査:

| 列 | air世界 | EmptyBlockGetter |
|---|---|---|
| 10述語すべて | 0 差分 | 0 差分 |
| 18面ビット | 0 差分 | 0 差分 |
| `getCollisionShape().isEmpty()` | 0 差分 | 0 差分 |

→ 状態IDだけで引ける表として妥当。**この確認をせずに表を焼くと、neighbour依存の値を
「状態の事実」として固定してしまう**（実際、最初に衝突形状として選んだメソッドは
outline で、下記の通り 5,124状態を反転させた）。

## 輸出器を壊しうる取り違え（実測）

`dtb$a` には `(BlockGetter, BlockPos) -> VoxelShape` を返すメソッドが5つある。
`tools/structure-oracle` の既存オラクルが使っていた `c` は **outline** で、衝突形状ではなかった。
候補ごとに、`assets/pblocks.json` の衝突形状リストと全状態で突き合わせた結果:

| メソッド | 資産との不一致 |
|---|---|
| `k` | **1**（`pitcher_crop[age=0,half=upper]`）|
| `l` | 986 |
| `c` / `j` | 5,124 / 4,881 |
| `m` | 21,613 |

`k` を採用し、`short_grass` / `torch` / `dirt_path` をアンカーに、
**outline を掴んだら落ちる**ようにした（5,124状態が反転したまま公開されるところだった）。

同じ理由で `isEmpty()` も名前ではなく振る舞いで同定している
（`VoxelShape` と `FluidState` で別名、型ごとに0引数booleanが複数ある）。
`SupportType` は難読化されておらず `[FULL, CENTER, RIGID]` の名前で取れるので、
3平面は**enum定数の名前から**引く（配列位置には依存しない）。

## 監査: 旧資産は既に正しかった。間違っていたのは合成

26,684状態すべてで新旧を突き合わせた結果、**旧資産の値は一致していた**:

| 旧 | 不一致 |
|---|---|
| `is_air`（`st==0 または state_flags bit0`）| 0 |
| `is_solid`（`state_flags bit6`）| 0 |
| `is_liquid`（`state_flags bit5`）= `!getFluidState().isEmpty()` | 0 |
| `luminance` / `opacity` | 0 / 0 |
| `side_flags & 0x3F` = FULL 面 | 0 |
| `block_predicates` bit0 = `isSolidRender` | 0 |
| `collision_shape_full` = `isCollisionShapeFullBlock` | 0 |
| `mineshaft_support` の FULL 6bit + CENTER(down) | 0 |
| `collision_shapes` リスト = 衝突形状が空 | 1（`pitcher_crop[age=0,half=upper]`、資産は空・native は衝突する）|

**したがって「値の取り違え」は無かった。** 壊れていたのは、その上に各呼び出し地点が
組み立てていた判定である:

### 欠陥1: `feature::face_sturdy` の余分な `blocksMotion` ゲート

旧 `face_sturdy` は `is_solid(st) && side_solid(st, dir)`。vanilla の
`isFaceSturdy(level, pos, dir)` に無い条件を足していた。pinned 表との差:

| 面 | 差分状態数 |
|---|---|
| DOWN | 105 |
| UP | 35 |
| NORTH/SOUTH/WEST/EAST | 各 3 |

内訳は repeater 64、scaffolding 32、comparator 16、snow 13、ladder 8、カーペット各1。
いずれも「自分の形が覆っている面」を持つのに `blocksMotion=false` なので、旧判定は
その面の支持を否定していた。→ `reg.is_face_sturdy(st, dir, Support::Full)` に置換。

### 欠陥2: `canSupportCenter` を FULL で近似していた

ボーナスチェストの松明は native では `canSupportCenter(level, pos.below(), UP)`
（= `SupportType.CENTER`）。FULL と CENTER は **UP だけで 9,940状態**異なる
（フェンスの支柱、壁、ピストン、チェスト等は「中心は支えるが面は覆わない」）。
→ 松明の2箇所だけ `Support::Center` にした。他の sculk 系4箇所は native の3引数
`isFaceSturdy`（FULL）のままで、これは vanila の呼び出しと一致する。

### 欠陥3: 衝突形状の問いを資産リストで答えていた

`reg.collision_boxes(cur).next().is_none()` → `reg.is_collision_empty(cur)`。
差は1状態（`pitcher_crop`）だけで挙動はほぼ同じだが、**問いの所有者が1つになる**。

## 34ビットに必要な理由（各列が独立であることの測定）

| 対 | 異なる状態数 |
|---|---|
| `canOcclude` vs `isSolidRender` | 14,747（向きは一方向のみ: solid render ⇒ occlude は常に真、逆は 0）|
| `liquid()` vs `!getFluidState().isEmpty()` | 9,212（waterlogged な植物は流体を持つが liquid ではない）|
| `blocksMotion` vs `!collisionEmpty` | 2,812 |
| `RIGID` vs `FULL` | 27（cauldron 系4種 + composter + hopper。`RIGID = FULL | up`）|
| `FULL` ⊄ `CENTER` | 0（全状態で FULL ⊆ CENTER）|
| FULL vs CENTER（DOWN/UP/N/S/W/E）| 10,621 / 9,940 / 5,923 / 5,919 / 5,923 / 5,919 |

## ゲート

- `bash tools/layers_check.sh`: PASS（`block_registry` は foundation 層のままで、新しい依存なし。
  `include_bytes!` はモジュール依存ではない）
- `bash tools/debt_ratchet.sh`: 0 over limit
- `python3 tests/gate_scripts.py`: PASS
- `cargo test --offline --locked --bin mc-spine -- --test-threads=1`: 533 passed / 0 failed / 1 ignored
- `bash tools/mutation_check.sh --offline`: **新規6件とアンカー付け替え2件を個別に proved**
  （`MUTATION_FILTER` で該当エントリのみ実行）。414エントリ全体の通し実行はしていない
  （1件あたり baseline + 変異の2回ビルドが必要）。通しの `python3 tests/gate_scripts.py` は
  gate 機構の自己テストで、変異の証明ではない。

新規の契約テスト（`src/generation_contract_tests.rs`）:

- `pinned_fact_tables_agree_with_the_older_oracle_assets` — 旧3資産との全状態一致。
  状態IDのずれ・平面の入れ替えを検出する（旧資産はここ以外では読まない）
- `state_facts_answer_the_predicates_vanilla_separates` — cave_air、waterlogged 植物、
  leaves/glass/piston の occlude と render、短草と松明の衝突形状、発光/遮光、
  フェンス支柱の FULL/CENTER
- `face_support_answers_the_support_type_the_caller_asks_for` — 旧ゲートが否定していた
  repeater/comparator/snow/carpet/ladder の面、および FULL/CENTER/RIGID の分離

`tools/mutation_check.sh` に追加した変異（すべて赤くなることを確認済み）:
`is_air` を `st==0` に、`blocks_motion` を air ビットに、`is_collision_empty` を full ビットに、
`is_liquid` を `liquid()` に、発光を `&0xf` に、面平面の添字を `direction` に、
レコード長を1に。既存2件（note block の solid、ボーナスチェストのセル判定）は
実装移動に合わせてアンカーを付け替えた（付け替え前は SKIP になっていた）。

## 生成結果が変わらないことの測定

`tools/port_triage.py /tmp/nat-4242-village-taiga/world 4 -32 --n 3 --mode full --samples 3`
を、変更前（stash して再ビルド）と変更後で2回:

| | 9/9 chunk の diffs | 内訳 |
|---|---|---|
| 変更前 | 4 | water[level=1]→air 2、brown_mushroom→air 1、air→fern 1 |
| 変更後 | 4 | 同一サンプル・同一ペア |

→ この fixture では**ビット同一**（変更が届かない）。欠陥1〜3が効く状態に生成が
到達していないためで、述語そのものは契約テストと変異で押さえている。
他の fixture（sculk 系・雪層・カーペット上の設置）は未測定で、そこが残件。

## sculk 4箇所の支持平面（独立レビューで確定、2026-11-16）

pinned `server.txt` mappings が無い環境でも決着できるよう、obfuscation 名を behavior から同定した
（`SupportType` = 定数プールに `FULL`/`CENTER`/`RIGID` を持つ唯一のクラス = `doa`、
`BlockState` = `dtc extends dtb$a`、`SculkShriekerBlock` = `dts.G`(`can_summon`) を参照する唯一の block）。

- 3引数 `isFaceSturdy` は `dtb$a.d(dcc,jd,ji)` で、本体は `doa.a`(FULL) を積んで4引数へ委譲する。
  `doa$1`(FULL) の本体は `isFaceFull(getCollisionShape(level,pos), dir)` — state の衝突形状だけで決まり、
  近傍を読まない。よって pinned 表の列として妥当。
- `canSupportCenter` は `dfy.a(dcz,jd,ji)` で `doa.b`(CENTER) を使う。CENTER と FULL は別平面。

| port の site | native の本体 | 呼び出し | 判定 |
|---|---|---|---|
| `spc_can_spread_from` | `SculkPatchFeature.canSpreadFrom` = `edi` | `dtc.d(dcc,jd,ji)` | FULL 一致 |
| `gen_sculk_patch`(shrieker) | `SculkPatchFeature.place` = `edi` | `dtc.d` → `dmt.d` | FULL 一致 |
| `spc_cursor_can_spread` | `ChargeCursor.canSpread` = `dmu$a` | `dtc.d(dcc,jd,ji)` | FULL 一致 |
| `spc_vein_can_spread` | `SculkVeinSpreaderConfig` `dkz$a` → `MultifaceBlock.canAttachTo` `dky.a` | `isFaceFull(collision) \|\| isFaceFull(support)` | **近似**（下記） |

残る差は1点だけ: `MultifaceBlock.canAttachTo` は衝突形状と support 形状の**両方**を OR するが、
port は `Support::Full` の片方（衝突形状）だけを問う。したがって vein の site では port が
**reference より厳しい**（付くはずの vein を拒みうる）。FULL と CENTER の取り違えではなく、
6A 以前の `is_solid && side_solid` より緩い方向への変更でもない。未修正・記録のみ。

## 残件（意図的に触っていない）

1. **`liquid()` を使う呼び出し地点は無い。** `is_liquid` は `!getFluidState().isEmpty()`
   （旧 `FL_LIQUID` と全状態で一致）のまま。`feature::is_water_fluid` などは
   waterlogged 植物を「水」として扱う必要があるので、`liquid()` に置き換えると壊れる。
   `is_liquid_block` は輸出だけしてあり、監査用。
2. **`can_occlude` の本番消費者も無い。** 光は `lightBlock`（`opacity_of`）で減衰させるのが
   正しい問いなので、列だけ持たせている。
3. **sculk 系4箇所の FULL — 2026-11-16 の独立レビューで jar から決着済み（下記）。**
   未検証だった前提は「`SculkShriekerBlock.canSurvive` が `canSupportCenter`（CENTER）かもしれない」
   だったが、`SculkShriekerBlock`（`dmt`）は `canSurvive` を宣言せず `SupportType` も参照しない。
   この前提は成立しない。詳細は §「sculk 4箇所の支持平面（独立レビューで確定）」。
4. `assets/block_predicates_1211.bin` の bit0、`collision_shape_full_1211.bin`、
   `mineshaft_support_1211.bin` の面ビットは**同じ native 呼び出しの独立な再記録**として
   残した（`pinned_fact_tables_agree...` が読む）。挙動の所有者は新しい表のみ。
   `mineshaft_support` は bit7（chain クラスの instanceof）だけが表に無い事実として残る。
