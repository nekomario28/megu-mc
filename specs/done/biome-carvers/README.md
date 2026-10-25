# Biome carver lists — own-jar dialect

固定1.21.1のbiome JSONにある`carvers`を、既存のcarver ownerへ正しく渡す限定契約です。carverのセル形状やmob spawnの実行を閉じるものではありません。数値と実行unitは[検証記録](../../../evidence/foundation/biome-carvers/README.md)を正本とします。

## なぜ壊れていたか

移行後のbiomeは`"carvers": {"air": [...]}`（1件なら`"air": "minecraft:nether_cave"`）です。旧loaderは`carvers`を配列としてだけ読み、オブジェクトでは`None`を返していました。そのため全biomeのcarver列が空になり、`apply_carvers`は一度もconfigured carverを適用していませんでした。テストの`forest carvers`失敗はこの帰結です。

`carver_list`は1.21.1の`air`配列・単一文字列と、変換packのbare配列・文字列の両方を受けます。順序はcarver seed indexそのものなので入れ替えません。`"carvers": {}`は空のAIR stepとして保持し、欠落と混同しません。`nether_cave`はnether biomeの一覧には残しますが、configured carver自体は未対応のまま明示します。

featureのdripstone/rootテストは別の理由で赤でした。テストが`worldgen/feature/`を読み、固定jarは`worldgen/configured_feature/`を使うためです。`pack_cf`は`configured_feature`を先に試し、変換pack向けに`feature`へfallbackします。

## 維持する不変条件

- `carvers`はstep objectであり、`air`は配列または単一文字列。bare配列/文字列は変換pack用。
- biomeごとの一覧順を保つ。空・欠落・未知carverを「carverなし」と混同しない。
- 実クラスの`Biome.getGenerationSettings().getCarvers(AIR)`を正解とし、donorの版・配列を仕様にしない。
- テスト用fixtureの探索はown-jarの`configured_feature`を優先し、変換packの`feature`も読める。
- suiteが緑でもwhole-world parityやcarverセル一致の証明と呼ばない。

実装は [`feature.rs`](../../../src/feature.rs) の `carver_list` / `load_creg` / `pack_cf`、検証は [`generation_contract_tests.rs`](../../../src/generation_contract_tests.rs) と [`biome_carver_oracle.py`](../../../tools/biome_carver_oracle.py) が持ちます。

## 閉じていないもの

`spawners`/`spawn_costs`、nether caveの実行、mob spawn runtime、carverセル単位の新しいnative対照、全seed/chunk、surface full context、forestのstone/deepslate帰属、性能は別です。

## 捨てた道

- bare配列だけを読む案: own-jarの全biomeでcarverが無効になり、12個の既存テストが落ちた。
- objectの`air`を配列だけとする案: `nether_cave`の単一文字列を落とした。
- テストの`feature/`固定: own-jarのdripstone/rootを読めず、5件が赤のままだった。
