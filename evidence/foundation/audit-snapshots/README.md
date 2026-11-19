# 監査が引用する修正前ソース

2026年9月の wire/security 監査（[AUDIT_WIRE.md](../../../AUDIT_WIRE.md)、
[SECURITY_NOTES.md](../../../SECURITY_NOTES.md)）は、修正前の `src/main.rs` を
根拠に指摘を書いています。その原本は長いあいだリポジトリ外
（`/data/lane-scratch/mc-spine*`）にだけあり、引用がホストの残骸に依存していました。
ここに置き直したのは、引用がリポジトリ内で解決するようにするためです。

| ファイル | 何か | sha256 |
|---|---|---|
| `main.pre-cap.rs` | `read_packet` の事前チェック（cap）前のソース。当時の `src/main.rs.bak-precap` | `f5a28e8c7a01814cd653227dc17e966c24e928a7bb96d004dfdb334798c83207` |
| `main.2026-09-25T1741.rs` | 監査2巡目が見た 17:41 断面 | `cbc30aae6fd9fef15453b93d313f60b757324831ac2cfbdd6a1810bb969feef2` |
| `main.2026-09-25T1809.rs` | 同じ日の 18:09 断面（`AUDIT_WIRE.md` の「New surface since」） | `f0cdc493abbc7a3138bf9390dd2c935d533c1cdbef793a5e3ee92b3d0171e5de` |
| `main.2026-09-25T1641.rs` | 同じ日の 16:41 に配布物へ当てた版（`AUDIT_WIRE.md` が「incident-era build」として引用）。バグのある `write_varint` を実際に積んだビルドの断面 | `3c834269c9ce4cbc3bbb851b73ea2af8e14d21fa658881e7683de0cc26907343` |

これらは**ビルド入力ではありません**。どのrevisionのビルドにも使われず、
ゲートも読みません。当時の指摘が指している行を含むかどうかを確かめるための
記録です。現在の挙動や安全性の証拠ではありません。

## 73. 同梱データは参照ビルドのものではない（1.21.1 データの新方言への変換）

`large_dripstone` の移植中に `speleothem_cluster` が「1.21.1 にクラスが無い」ことに気づいたのが発端。
`tools/vanilla_pack_from_jar.sh`（固定 jar から本物を入れる）と `tools/pack_provenance.py`（差分を計測）を
作って測った結果、**リポジトリの `datapacks/vanilla` は 1.21.1 の worldgen をかなり新しい方言に
変換したもの**で、参照ビルド（固定 jar）が実際に生成に使うデータではなかった。

### 計測（`tools/pack_provenance.py`、ローダが読むディレクトリのみ）

| ディレクトリ | 一致 | 差異 | 1.21.1 に無い（pack のみ） |
|---|---|---|---|
| `configured_feature`（jar）/ `feature`（pack） | 196 | 0 | **69** |
| `placed_feature` | 233 | 0 | **44** |
| `biome` | 0 | 64 | 3 |
| `noise` | 0 | 60 | 4 |
| `density_function` | 2 | 18 | 11（+ jar のみ 15） |
| `noise_settings` | 0 | 1 | 0（+ jar のみ 6）|
| `material_rule` / `material_condition` / `block_state_provider` | 0 | 0 | 42 / 8 / 8 |
| `tags/block` | 120 | 64（JSON同値60） | 132 |

合計 identical 555 / differing 207（うち JSON として同値なのは 60 = 純粋な再整形）。
同梱 pack は overworld の noise_settings 1 つと縮小された density_function 群しか持たない（jar の
`density_function/end/*` 15 ファイルと noise_settings 6 ファイルが無い）ので、エンド/ネザー/洞窟系の
生成は同梱データでは動かない。
`configured_carver` は 4/4 一致、feature 系の中身も**バイト一致**（= 変換は部分的で、feature JSON は
そのままコピーされた）。1.21.1 に無い configured feature には `firefly_bush` `bush` `dry_grass`
`leaf_litter` 系 `fallen_*_tree` `flower_pale_garden` `speleothem_cluster` などが含まれる。

### 方言の差（実物を並べた結果）

- biome: 1.21.1 は `carvers`/`spawners`、pack は `attributes: {minecraft:gameplay/natural_mob_spawns}`
- noise: 1.21.1 `amplitudes`/`firstOctave`、pack `base_amplitude`/`base_octave`/`octave_count`
- density_function: 1.21.1 `y_clamped_gradient`（`from_y`/`to_y`）+ 二項演算 `argument1`/`argument2`、
  pack `gradient` + `axis`/`from_coordinate` + `left`/`right`
- noise_settings: 1.21.1 は aquifer 群が `noise_router` の中、pack は **トップレベル `aquifers`**
- ブロック参照: 1.21.1 `{"Name": ..., "Properties": ...}`、pack `"minecraft:leaf_litter"` /
  `{"id": ..., "properties": ...}`

そのため**ローダ（`src/worldgen.rs`）は pack の方言で書かれている**。本物を入れると
`Gen::load` が `None` を返す（最初に落ちるのは `noise_settings` の解釈: `let ns = ns_json?;`）。

### ブロックレジストリとの不整合（未測定の症状候補）

`assets/blocks_1211.json` は**正しく 1.21.1**（1060 ブロック、`leaf_litter`/`firefly_bush`/`pale_oak_log`
を含まない、最大 state id 26683）だが、データ側は新しいので、pack が名前で参照するブロックの一部は
レジストリに無い。`BReg::state_of` は未知名で **0（= air）を返す**ので、新しい feature が発火する
チャンクでは「1.21.1 に存在しない feature が地形に空気を書く」可能性がある。**これはまだ測っていない**
（森林チャンクの fixture が次の一手: 既存の kelp/ore と同じ forceload + ブロック diff で足りる）。

### 試したこと / 次の手順

1. `tools/vanilla_pack_from_jar.sh` は固定 jar から `worldgen` + `tags/block` を入れる（sha1 検証、
   ディレクトリ存在検査、1080 ファイル）。**インストール自体は成功する**。
2. ただしその状態では 194 テスト中 21+ が落ちる（`Gen::load` が None）。ローダを 1.21.1 方言に
   対応させるのが本作業: 二項演算の `argument1/2`、`y_clamped_gradient`、noise_settings の
   `noise_router` 内 aquifer、noise の `firstOctave/amplitudes`（`normal_new_l` 系は既にある）、
   biome の `carvers`/`spawners`、ブロック参照の `Name`/`Properties`。
3. その後、golden 値と fixture を測り直す（feature JSON は同一なので feature 移植の golden は
   影響を受けない見込み、terrain 系は影響を受ける）。
4. **森林チャンクの fixture** で「新しい feature が空気を書く」症状の有無を測る（未測定の穴）。
5. 作業は branch `pack/1211-own-data` に隔離（canonical は緑のまま）。

この発見の意味: 参照世界は固定 jar が生成しているのに、port はその jar のデータを読んでいなかった。
`feature_type_coverage.py` の「overworld 到達型 3」も pack 由来の数字で、うち `speleothem_cluster` は
**1.21.1 の feature ではない**（変換で入ってきたもの）。真の未実装到達型は 2（`fossil` `sculk_patch`）。

## 74. P0-2 到達性監査スナップショット（2026-11-19）

`tools/p0_2_audit.py` / `tests/p0_2_audit_test.py` を `d918eaca`（`Cond::Biome` を `u128` に同期）に
同期した後の、固定 jar 対象データパックでのたどり着ける領域（density functions / surface rules /
placement modifiers / configured features / biome carvers+spawners+spawn_costs / structures +
structure_set）と `Cond::Biome(u128)` アンカー確認の結果。

| ファイル | 何か | sha256 |
|---|---|---|
| [p0-2-reachability-20261119.md](p0-2-reachability-20261119.md) | 2026-11-19 の `tools/p0_2_audit.py` 同期版監査結果: HEAD `d918eacaee795387031ef6446e68c685e59b9c11` で `python3 tools/p0_2_audit.py --json` → failures 0（bleaf_count: overworld 53、nether 5、end 5、すべて `< 128` `Cond::Biome(u128)`） | `bf0762e8cec386d09723ce258d1e6b891305369eff3992604f618c247eaff5a1` |
