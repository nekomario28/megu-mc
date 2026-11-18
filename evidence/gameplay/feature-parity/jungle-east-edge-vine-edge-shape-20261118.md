# Track 5B: the jungle east edge is the missing `VineBlock.updateShape` half of the tree edge pass

Status: **測定で決着。仮説は CLOSED(表面グループ全量)。** 残差 121 → 47 で、
閉じた 74 セルはすべて樹冠/地表グループ、残る 47 セルは dripstone_caves の地下グループ(この lane の
対象外)。`specs/worldgen-parity/finite-acceptance-set.md` §6 の jungle-temple 行は **HOLD のまま**
(47 ≠ 0)だが、その内訳は「樹冠は一致、地下の dripstone ポインタだけが残る」に変わった。

これは索引であり置き換えではない。数値はすべて下記 raw log に紐づく。

## 0. Base state

| | |
|---|---|
| base commit | `018f8dc38569dad1a6e3257d2b8a6345bcff39a6` (`perf/worldgen-parallel`, clean) |
| instrument commit | `238da7752d7c9d232806ce13ec01c4bd18449c44` |
| native jar | `captures/matrix-v1/nat-4242-jungle-temple/server.jar` sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` |
| decompiled reference | `mc-spine-local/scratch/vanilla-src-5b/` (`tools/vanilla_src.sh`、jar SHA-1 検証済み) |
| raw logs | `mc-spine-local/audit/track5b-jungle-20261118T085038/` |
| captures | `captures/matrix-v2/nat-4242-jungle-vine/`(計装 run)、`…-vine-all/`(全域 vine trace)、`…-jungle-tree/`(`-Dtreechunk` 付き) |
| binaries | pre-change `7de2b614bc359fd2…`、post-change `1ef609abeda1a8e5…`(dump sha は下記) |

## 1. 参照実装(測定ではなく読み)

`TreeFeature.place` は `updateLeaves` の返す shape に対して
`StructureTemplate.updateShapeAtEdge(level, 3, shape, x, y, z)` を回す。その per-face lambda
(`ent.a(jd$a,int,int,int,jd$a,dcx,int,ji,int,int,int)`)は 1 face につき

1. inside(`p`)と outside(`q`)の state を**書き込み前に**読む、
2. `inside.updateShape(dir, outside, level, p, q)` → 変化したら `setBlock(p, …, flags & -2)`、
3. `outside.updateShape(dir.getOpposite(), insideNew, level, q, p)` → 変化したら書き込み、

の順で、outside には **inside の新しい state** を渡す(`VineBlock` はその引数を使わない。順序だけが
効く)。

`VineBlock.updateShape`(`dov.a(dtc,ji,dtc,dcx,jd,jd)`、`javap -c` で確認):

- 短絡は **`Direction.DOWN`**(index 0)であって `UP` ではない。`ji.<clinit>` は
  `a=DOWN b=UP c=NORTH d=SOUTH e=WEST f=EAST`。
- それ以外は `getUpdatedState`(`dov.i`)してから、`dov.n`(=`dov.g` の 5 プロパティ
  `{north,south,west,east,up}` の真偽数 > 0)が偽なら `Blocks.AIR`。
  `dov.g` は `PipeBlock.PROPERTY_BY_DIRECTION` から `DOWN` を除いたもの(`PipeBlock.h` は 6 面を持つ)。
  つまり **`up` だけ立っている vine は air にならない**。tick 経路の 4 面述語 `dov.p` は別物。
- `getUpdatedState` の `up` 再計算は `canAttachTo(level, above, DOWN)`、すなわち
  **上のセル**の面。`above` は `jd.d()`(= `a(ji.b)` = `above()`)。
- 水平 4 面は `Direction.Plane.HORIZONTAL` の順で、`pos.relative(d)` が vine 側に面を持つか、
  だめなら **上のセル**が同じ面を持つ vine か(hanging chain)。

`MultifaceBlock.canAttachTo` は `isFaceFull(shape(pos), dir.getOpposite())` なので、ポートの
`BReg::supports_multiface(st, d)`(multiface oracle で検証済み、`assets/multiface_support_1211.bin`)に
そのまま対応する。

旧 worktree `t3-548b811e` の未コミット draft はこの 3 点をすべて逆に取っていた(短絡を UP、
`up` と chain を **下**のセルから、air 判定を 4 面のみ)。読みのみで参照し、実装は canonical 上で
書き直した。

## 2. 計装(Phase A)

`tools/carve-agent` に 4 フックを追加(`VSHAPE`/`EDGE`/`BPRED`/`HMP`)。`VSHAPE` は
`dov.updateShape` の両 `areturn` を踏み、`(dir, x, y, z, before, after)`(`Block.getId`)を
順序保存の `SculkLog` writer に出す。`CarveLog` はフィルタ前の呼び出し数を数え、shutdown で
`VINESUM` を出す — 空トレースが「フックが動かなかった」のか「フィルタで全部落ちた」のかを
区別できるようにするため。build gate(`tools/build_carve_agent.sh`)は
`VSHAPE`/`EDGE`/`BPRED`/`HMP` と `dov`/`eij`/`eir` の配線を要求する。

capture の同等性(計装が何も動かしていないこと):

| capture | `native.bin` sha256 | 備考 |
|---|---|---|
| `matrix-v1/nat-4242-jungle-temple`(2026-11-16) | `77964c224c9eeddf…a715bf` | 既存 |
| `matrix-v2/nat-4242-jungle-vine`(今回、`-Dvinechunk=74,-41`) | `77964c224c9eeddf…a715bf` | **byte 一致** |
| `matrix-v2/nat-4242-jungle-vine-all`(今回、`-Dvinechunk=all`) | `77964c224c9eeddf…a715bf` | **byte 一致** |
| `matrix-v2/nat-4242-jungle-tree`(今回、`-Dtreechunk=74,-41 -Dvinechunk=74,-41`) | `77964c224c9eeddf…a715bf` | **byte 一致** |

再取得のセルは 2026-11-16 の capture と一致した(相違は無い = 報告すべき finding は無い)。
boundary: `frozen`, seed 4242, chunk (74,-41) 1×1, `min_y` -64 / height 384, stage `structures`,
`runtime_writes 0`, decorated_chunks 9。

## 3. vine trace(chunk (74,-41) と 9 chunk 全域)

| | native | port(修正後) |
|---|---|---|
| `updateShape` 決定数(全域) | 33,705 | 9,471 |
| うち chunk (74,-41) | **196** | — |
| state が変化した決定(全域) | **90**(すべて `vine[…] -> air`) | 83 |
| うち chunk (74,-41) | **0** | — |

chunk (74,-41) の 196 決定は**すべて無変化**で、y 73..75 の帯(x 1198/1199)は
`vine[east=true]`/`vine[south=true]`/`vine[up=true]` がそのまま残る。つまり残差セルの座標そのものに
vine の消去は無い。変化した 90 は隣接 chunk(x 1147..1246, z -698..-595)の樹冠で、`feature=7,9`
(jungle tree)の同一 placement 窓に属する。

両側が同じ (dir, cell) を訪れた 1,170 件では `before` が 1,086、`after` が 1,076 で一致した
(不一致 94 はすべて「立っている面の組み合わせ違い」= 既存の decorator 差で、変化の有無は一致)。
規則の不一致は 1 件だけ: `(dir=2, 1202, 71, -671)` で native は `vine[west=true]` を保ち、
port は air にした — 隣接セルの state が両側で違うための入力差で、規則差ではない。

## 4. 測定(Phase C): 121 → 47、回帰 0

同じ capture(`matrix-v2/nat-4242-jungle-vine`)を、`tree.rs` だけを 018f8dc に戻した binary と
修正 binary で比較(`tools/port_triage.py`, `--mode full`, 1×1 (74,-41)):

| | pre-change | post-change |
|---|---|---|
| port dump sha256 | `06f8723098…`(2026-11-16 の記録と一致) | `20fc4d9d9c…` |
| `all_air_kinds` | **121 / 98,304** | **47 / 98,304** |
| non_air | 34,307 | 34,353(native 34,353) |

- **閉じた 74 セル**(`closed-cells.tsv`)、新規の差は **0**。
- 閉じた 74 はすべて樹冠/地表グループ: x 1184..1199, y 68..81, z -656..-641。
  内訳は `oak_leaves[distance=1..6] -> air` 53、`air -> oak_leaves` 9、
  `short_grass`/`fern` の両方向 12、`oak_log[axis=x] -> air` 1 —— **葉グループと
  east-edge band(x 1198..1199, y 73..75)は残差 0**。
- 残る 47 は dripstone_caves の地下グループ(pointer/`dripstone_block`/`stone`→`deepslate`、
  y -39..45、`(1195,-39,-647)` 等)。この lane では追わない。

計装の非侵襲性: `VINE_EDGE_TRACE=all` を付けた binary の dump sha も `20fc4d9d9c…`(trace 無しと同一)。

## 5. 何が起きていたか(測定による帰属)

`TreeFeature.place` の obstruction 走査は `ignore_vines=false` の config で vine を障害物として扱う。
帰属は推測ではなく placement 列で測れる: 同じ window・同じ capture で、`TREE_TRACE=74,-41`
(port、1 chunk 半径)と native の `-Dtreechunk=74,-41` の **tree origin 列**を比べる。

| | origin 数 | native との一致 |
|---|---|---|
| native (`-Dtreechunk=74,-41`) | 109 | — |
| port pre-change | 110 | 集合も順序も**不一致**(19 origin がずれる) |
| port post-change | 109 | 集合も順序も**完全一致**(`native order == fixed order`) |

つまり edge pass の vine 半分が欠けていたことで、直前の tree が置いた「支えの無い vine」が残り、
**後続の tree placement の列そのものがずれていた**(pre-change だけに現れる origin の例:
`(1200,71,-657)`, `(1214,69,-625)`。いずれも native には無い)。修正後は origin 列が native と
同一になり、閉じた 74 セル(葉の距離値 1..6 の帯で `air`/`leaves` が入れ替わり、
`oak_log[axis=x]` が 1 本消え、地表の草/シダが入れ替わる)はこの 1 段のずれの解消として説明できる。

当初仮説の文言のうち「後続の `block_predicate_filter` が空を期待して落ちる」は**測れていない**:
`BPRED` trace は取ったが、この chunk で verdict 差を帰属できていない。測れた因果は
「edge pass が vine を落とす → 後続 tree placement の入力が変わる」であり、規則そのもの
(短絡方向・`above` からの `up`・`up` を含む air 判定・chain)は
`the_tree_edge_pass_applies_vine_update_shape` が pin している。

## 6. ゲート

| ゲート | 結果 |
|---|---|
| serial suite | `661 passed; 0 failed; 1 ignored`(修正前は 660 passed / 1 ignored) |
| `--test-threads=8` | `661 passed; 0 failed; 1 ignored` |
| 64-chunk dump (seed 0, N=8 at (0,0)) | pre-change と **byte 一致** `670fdfe3472812…`、25,165,824 B |
| 同 dump の compiled vs interpreter(`MC_SPINE_DF_INTERP=1`) | **byte 一致** `670fdfe3472812…` |
| `fixture_suite` manifest-promotable | `MATCH 8 / DIFF 0 / INVALID 0 / NOT_RUN 0` exit 0 |
| `fixture_suite` manifest(9 行) | `MATCH 8 / DIFF 1`(village-taiga 行、この lane の対象外) |
| mutation `MUTATION_STABILITY=1` | 2 件 red/red/green。短絡を UP にすると DOWN 短絡の assert が、`above` を `below` にすると `up` の assert が落ちる(`mutation-logs/`) |

注: `evidence/foundation/performance/README.md` は 64-chunk dump の **size と アーム間の
byte 一致**を記録しており、sha256 は記録していない(タスク記述と相違)。ここでは pre/post と
アーム間の一致を測り、`670fdfe3…` を新たに記録する。

## 7. この文書が主張しないこと

- jungle-temple 行の受入(MATCH)は主張しない。残差 47 が残る限り HOLD である。
- 残る 47 セル(dripstone_caves)の原因・帰属は主張しない。別 subsystem。
- 全域の vine 決定数が native 33,705 / port 9,471 と 3.5 倍違う件は、**この lane では追っていない**。
  shape の訪問集合と `updateShapeAtEdge` の呼び出し元(template 経路を含む)の差であり、
  今回の残差の原因ではない(残差は 47 まで落ち、樹冠は 0)。
- 全 seed / 全座標への一般化はしない。
