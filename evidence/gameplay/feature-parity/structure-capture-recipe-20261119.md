# 構造物 capture のレシピ(`tools/structure_capture.py`)と batch 実測

Status: **レシピは既存行2件を byte 一致で再現した(採否ゲート合格)。batch は
`ruined_portal` B-run と `ocean_monument` が 0 差、`igloo` 8 / 98,304 と
`village_plains` 159 / 98,304 が HOLD。`src/` は無変更。**

## 0. Base & Reference Provenance

| | |
|---|---|
| base commit | `0fa6807`(`perf/worldgen-parallel`、worktree `t3-0ff56908`、dirty 0) |
| port binary | `target/debug/mc-spine` sha256 `7dc623bf3372e5e8d71ad6cab6b65196f2a3ba7f125862234b4371524c4ef7ca`(`cargo build --offline --locked -j6`) |
| pinned server jar | `<home>/tmpx/carverfresh/server.jar` sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` |
| official mappings | `<local>/audit/historical/fossil-placement-20261106/server.txt` sha1 `03f8985492bda0afc0898465341eb0acef35f570` |
| captures | `<local>/captures/matrix-v3/`(新規)、`matrix-v1|v2`(既存参照) |
| raw | `<local>/audit/p21t3-20261119/` |

## 1. なぜレシピか

`matrix-v1|v2` の構造物行は毎回同じ6段の手作業で作られていた。段はどれも既存の道具が持ち、
レシピはそれらを繋ぐだけで、新しい比較器も新しい採否規則も持たない。繋ぐ順序が固定されて
いないこと自体がずれの場所だった(候補でない chunk を焼く、`structures.starts` を見ずに
「構造物がある」と扱う、`expected_runtime_writes` を宣言し忘れて行が INVALID になる)。

段と owner:

| 段 | owner | 何を保証するか |
|---|---|---|
| admission | `mc-spine --worldgen-structure-candidates <seed> <cx> <cz>` | セル候補・frequency・exclusion。**必要条件のみ**(probe 自身が `biome_and_pieces_unchecked=true` と印字する) |
| biome hint | `MC_SPINE_BIOME_AT` + `datapacks/.../has_structure/*.json` | 試行順。判定ではない |
| capture | `tools/carve-agent/native_fixture.sh` | frozen / `structures` stage / `order.tsv` / `boundary.json` |
| start 確認 | capture した chunk の `structures.starts` | 要求した構造物の start が**保存 chunk に在る**こと |
| 比較 | `tools/port_triage.py --height-window` | build/env/dump/triage を fail-closed に1本化 |
| 行 | `tools/fixture_suite.py` の読み取りコード | `inputs.sha256` と `expected_runtime_writes` の宣言 |

セル候補の列挙は `floorDiv(cell * spacing, spacing) == cell` を使う。probe は「**聞いた chunk**
のセル候補」を返すので、セルの角を1回聞けばセルが1つ埋まる。角と答えを比較すると
(offset が 0 のセル以外)永久に一致しない — 実装中に一度これをやって igloo の候補が
0件になった。回帰は `tests/structure_capture_test.py::AdmissionScan` が持つ。

## 2. 採否ゲート: 既存行の再現

同じ入力で作り直し、**native と port の両方**が既存 artifact と byte 一致することを要求した
(片側だけの一致は「比較が空回りしていない」ことの証明にならない)。

| 行 | 新 capture の `native.bin` | 既存参照 | port dump | 既存 dump |
|---|---|---|---|---|
| jungle `(74,−41)` overworld/noise | `77964c224c9eeddff4bd0cea6ccbfc559647f44003ee001cccd6f36b33a715bf` | 同左 (`matrix-v1/nat-4242-jungle-temple`) | `77964c22…a715bf` | 同左 (`matrix-v2/dumps/jungle`) |
| fortress `(−25,6)` nether/dimension | `4df69f134515e18fb4e6afe7f15e8905a52cc4607ffe65a3d03658e27f643d92` | 同左 (`matrix-v2/nat-4242-nether-fortress-256`) | `4df69f13…643d92` | 同左 (`matrix-v2/dumps/fortress`) |

`order.tsv` も両行で既存と byte 一致(jungle `67ef353b…`、fortress `7fb14b7f…`)。fortress は
`expected_runtime_writes` を **`fixture_suite.runtime_write_record` で自動導出**し、
count 16 / `nether_brick_fence[…]×16` / `writes_sha256 81cdbd85…` が既存宣言と bit 一致した
(宣言と検証が同じ owner を持つことの実測)。`boundary.json` の差は `height_window` の追加と
`game_times`(1,1 vs 2,2)だけで、どちらも内容ではない。

レシピが吐いた行は自前 manifest で `1 MATCH / 0 DIFF / 0 INVALID, exit 0`(jungle)、
`2 MATCH … exit 0`(jungle+fortress)。

## 3. `ruined_portal` の §4-5(2つ目の独立 native run) — 充足

- capture: `matrix-v3/nat-4242-ruined-portal-b`(frozen, overworld/noise, 1×1 `(6,7)`,
  独自の `order.tsv` `0c452278…`、`boundary.json` `runtime_writes 0`)。
- start 確認: `structures.starts` に `minecraft:ruined_portal`、`rupo#0`
  BB `[97,100,112,106,107,123]`。
- **port vs native B = 0 / 98,304**、`--world-b` を A に向けた
  **native A vs B = 0 (all_air) / 0 (non_air)**。
- dump `2e4e3fedab3ba71f092747a18038ed2943d1a32fa791233244c120997ac95f9e`(既存
  `matrix-v2/dumps/ruined-portal/dump.bin` と byte 一致)。
- 行 `ow-4242-structures-6-7--64-384-b` として `manifest-promotable.json` に昇格。

**正直な限定**: A と B の `native.bin`(= `2e4e3fed…`)と `order.tsv` は **byte 一致**した。
窓が決まれば decorate 集合も決まるので、この A/B は「同じ窓が2回とも同じ結果を出す」ことの
確認であって、**順序の独立性の証明ではない**。§4-5 の「A/B 両方に 0 差」は満たすが、
「独立した順序」を主張する行ではない(順序を入力にする比較は §2 の `(3,−2)` 行が担う)。

## 4. batch の結果

| 構造物 | chunk (seed 4242) | 結果 | 行 |
|---|---|---|---|
| `minecraft:monument` | `(202,−369)` overworld | **0 / 98,304** | `ow-4242-structures-202--369--64-384` 昇格 |
| `minecraft:ruined_portal` | `(6,7)` overworld (B run) | **0 / 98,304** | `…-6-7--64-384-b` 昇格 |
| `minecraft:igloo` | `(37,−512)` overworld | **8 / 98,304** | 昇格しない(理由は §5 と別ページ) |
| `minecraft:village_plains` | `(−338,−384)` overworld | **159 / 98,304** | 昇格しない(別ページ) |

候補探索は `--scan-radius 512`(igloo, spacing 32)/ `384`(monument, village)。biome hint は
igloo 1026 候補中 45 が snowy、monument 576 中 49 が deep ocean、village_plains 510 中 76 が
plains/meadow を返し、いずれも**最初の hint 通過候補**で start が出た(igloo `(37,−512)`、
monument `(202,−369)`、village `(−338,−384)`)。

monument は chunk 全体が `minecraft:omb#0` BB `[3203,39,−5933,3260,61,−5876]` の内側で、
quart biome は `deep_cold_ocean` 1536/1536。両側 non_air 28,976、`WORLD_SURFACE`/
`MOTION_BLOCKING` とも 63(水面)で一致。

## 5. 未解と次の計測

- `igloo` の 8 セル: **帰属確定**。`CarveLog.featureCell` に opt-in の
  `-Dfeaturestack=1` を足して採り直した capture の stack が
  `IglooPiece.postProcess` → `TemplateStructurePiece.postProcess` → `StructureTemplate`
  の shape 更新経路を指し、その先の `SnowyDirtBlock.updateShape`(`snow_block` 直上の
  grass を `snowy=true` にする)が writer。port 側の欠落は
  `src/template_structures.rs::update_shape` の家族表に `SnowyDirtBlock` が無いこと。
  詳細は [igloo-snowy-under-floor-20261119.md](igloo-snowy-under-floor-20261119.md)。
- `village_plains` の 159 セル: 詳細は
  [village-plains-jigsaw-pieces-20261119.md](village-plains-jigsaw-pieces-20261119.md)。
  次の計測は port 側の jigsaw piece 列(bb・template id・rotation)を native の
  `structures.starts.Children[]` と piece 単位で突き合わせ、最初に分岐する piece を決めること。
- `--use-pop-order` 経路(capture の `order.tsv` を `MC_SPINE_POP_ORDER` として渡す)は
  実装済みだが、この batch では**使っていない**(既定順序で 0 になったため)。順序を入力に
  する比較が必要な行では明示的に使う。

## 6. 何を主張しないか

- この batch は §2 の凍結 window 集合の外(monument/igloo/village_plains の座標)である。
  `finite-acceptance-set.md` の §2 は改定していない(§6 の充足状況だけを更新した)。
- `igloo`/`village_plains` の残差を「構造物固有のバグ」と分類していない。測ったのは
  「8 セル / 159 セルが、どの pass で、どの piece BB の中で、どちらの側が書いたか」まで。
- suite の緑(14 行)は `manifest-promotable.json` の行集合についての主張であって、
  構造物全体の parity ではない。
