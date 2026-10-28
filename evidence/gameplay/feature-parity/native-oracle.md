# feature の native oracle と harness の契約

2026-10-27。対象は `tools/structure-oracle/` の `NativeHarness.java` /
`FeatureOracle.java`、runner の `tools/feature_oracles.py`、port 側の
`src/generation_contract_tests.rs::generic_feature_cases`。feature 型を1つ移植する
たびに同じ配管を作り直していたので、必要な能力と**測って分かった限界**をここに固定する。

## 何をするものか

- `FeatureOracle <cases.json> <out.tsv> [cells.tsv]` は、cases ファイル
  （`type` / `class`（registry との一致を assert）/ `config` / `scenes`（origin 相対の
  ply 列）/ `seeds` / `oy` / `extra` / 任意の `box`）から、**固定 jar の実 feature
  クラス**を scripted level に対して実行し、行
  `scene, oy, seed, wrote, cells, md5, rngProbe` を書く。`rngProbe` は配置後の
  `nextInt(1000)` 3連で、**セルが変わらない過不足な draw** も落とす。
- `tools/feature_oracles.py <type> --cases <json> [--write] [--cells <path>]` が runner。
  `pinned_runtime` が jar の SHA-1 を検査してから javac/java を回す。
- port 側は同じ cases ファイルを `p_cf` に渡し、同じ scene を組み、`wrote` / セル数 /
  properties 込み digest / rng probe を照合する。型を足すのは「cases ファイル + `p_cf`
  の腕」で済み、テスト本文は増えない。
- digest が合わないときは `GENERIC_CASE_DUMP=/tmp/port.tsv cargo test <type>` で port 側の
  セルを oracle の `--cells` 形式で吐き、cell 単位で diff する（今日の2つの形状バグは
  これで特定した）。

## level proxy が返すもの（契約）

| 呼び出し | 返し方 |
|---|---|
| `getBlockState(pos)` | scene の state（feature が書いたセルは write-through） |
| `setBlock(pos, state, flags)` | 記録する。build height 外は native と同じく拒否 |
| `getFluidState(pos)` | その state 自身の fluid state（worldgen に流動は無い） |
| `isStateAtPosition(pos, predicate)` | その state に predicate を適用（`canBeReplaced` 系がここを通る） |
| `getMinBuildHeight` / `getMaxBuildHeight` / `getHeight` | `LevelHeightAccessor.create(minY, height)` の実装 |
| interface の default body | proxy に束縛して実行（block 知識は proxy 側が持つ） |
| それ以外の abstract | **throw**。半端にモデル化した level で比較しない |

## データで裏づけた答え

- **block tags**: pack の `tags/block/*`（入れ子は展開）。
- **fluid tags**: 固定 jar の `data/minecraft/tags/fluid/*`。`LevelReader.isWaterAt` は
  `getFluidState(pos).is(FluidTags.WATER)` なので、未 binding だと水が水でなく、
  そこから導く `waterlogged` が常に false になる。実測: `pointed_dripstone` の水セルに
  `waterlogged=false` が記録されていた。
- **config decode は `RegistryOps`**（`BuiltInRegistries.REGISTRY` 経由）。素の `JsonOps`
  では `huge_fungus` の `replaceable_blocks`（`matching_blocks`）が
  `Can't access registry minecraft:block` で落ちる。
- **block state decode は実 `BlockState.CODEC`**。scene の `{"Name":..,"Properties":..}` を
  自前で綴らない（property の値の表現を推測しない）。
- **`ChunkGenerator` は実物**を渡す: `NoiseBasedChunkGenerator` + vanilla の
  `NoiseGeneratorSettings.dummy()`（minY −64 / height 384 / sea level 63。level proxy と
  同じ数）。`getSeaLevel()`/`getGenDepth()` は abstract で、`null` を渡すと feature が
  そこで落ちる（`basalt_columns`, `huge_fungus`）。

## 測った限界（再挑戦しないための記録）

1. **`ChunkGenerator` をサブクラス化して answers を差し込むことはできない。** jar の
   クラスは署名済みで、default package に足したクラスは
   `java.lang.SecurityException: class "ProbeChunkGenerator"'s signer information does not
   match signer information of other classes in the same package` になる。だから上の
   実 generator 方式にしてある。
2. **`ServerLevelAccessor.getLevel()` は供給できない。** proxy に来るのは
   `UnsupportedOperationException: public abstract aqu ddl.E()`。entity を作る feature
   （`end_spike` の End Crystal）はこの harness では駆動できず、whole-chunk / entity
   対応まで延期する。
3. **scene は build height 外のブロックを持てない。** port 側は `Gen::feat_write` が範囲外を
   落とすので、native の scene と port の scene が別物になる。build height 外の水を必要と
   する形状の切り落としは比較できない（`pointed_dripstone` の `floor`@319 のように scene が
   範囲内で済む場合だけ切れ目を検証できる）。
4. **`wrote` 列は feature の戻り値ではない。** vanilla の coral 形状は駒の失敗後も `true` を
   返し、port の `Cf` dispatch は成功フラグを持たない。両側が言えるのは
   `place(...) && cells > 0` だけなので、そう記録している。
5. **tag の順序が要る。** `getRandomElement(tag, rng)` は pack の `values` 順に index
   するので、`BReg` は集合とは別に `tag_order` を持つ。

## この harness で検証済みの型（2026-10-27 時点）

`huge_brown_mushroom`, `huge_red_mushroom`, `pointed_dripstone`, `coral_tree`,
`coral_claw`, `coral_mushroom`, `huge_fungus`（warped / crimson の2 config）。
それぞれ `tests/data/*_1211.tsv` と `tests/feature_cases/*.json` を持ち、
mutation gate が `tools/mutation_check.sh` に登録されている。

未検証の残りと理由: `basalt_columns`（generator が載ったので oracle 可能・未着手）、
`sculk_patch`（SculkSpreader の owner が必要）、`fossil`（template パイプライン待ち）、
`end_spike`（限界 2）、`bonus_chest`（biome 到達 0）。

## 再現

```bash
# 既存 fixture との一致検査（native を実行する）
tools/feature_oracles.py huge_fungus --cases tests/feature_cases/huge_fungus.json

# fixture を記録する（意味の変化を説明した後だけ）
tools/feature_oracles.py <type> --cases tests/feature_cases/<type>.json --write

# port 側のセルを出して cell 単位で diff する
GENERIC_CASE_DUMP=/tmp/port.tsv cargo test <type>
```

`tools/vanilla_src.sh <official class...>` は同じ固定 jar を Vineflower でソースに落とす
（読解用・repo 外・jar と mappings の SHA-1 を検査）。README の「未スクリプト化」は解消済み。

## The route that works when the harness cannot: the instrumentation agent

The two limits above (no `ChunkAccess`, `getChunk` answered but biomes unresolvable) close this
harness for features whose path reaches the level's chunk or a biome query - `sculk_patch` is
one. The repository already carries the other instrument: `tools/carve-agent/`, an ASM
`javaagent` that instruments the pinned server itself and logs through a bootstrap class. It is
driven with `JAVA_AGENT=<carveagent.jar> tools/chunk_world_fixture.sh <tag> <cx> <cz> <n> features`,
and it observes the *intermediate* state a scripted level cannot:

- `SCURSOR r=<round> i=<index> x y z charge decayDelay updateDelay facingMask` at
  `SculkSpreader$ChargeCursor.update` entry - `r` counts `updateCursors` calls and `i` is the
  cursor's index inside that round, because a cursor that reaches charge 0 is dropped and the
  list shifts. The port prints the same line under `SPC_CURSOR_TRACE=1`, so a cursor's
  trajectory is two lists aligned by (round, index) instead of two cell sets.
- `SCMARK` at the sculk call sites, `SSPREADALL types=<n>` at `MultifaceSpreader.spreadAll`
  entry, and `-Dfeaturebox=...` for `FCELL x y z state=<id> feature=<gi>,<step> chunk=<pop>`
  (the id comes from the jar's own registry, the same numbering the port's asset uses).

Two properties make this a usable oracle: the hooks only read fields and call `jd.u()/v()/w()`,
so they draw no random and cannot change a spread; and the port can emit the same shapes, so
the comparison is index-aligned rather than aggregate. The method is written up, with what it
found, in [`specs/worldgen-parity/sculk-patch.md`](../../../specs/worldgen-parity/sculk-patch.md).
