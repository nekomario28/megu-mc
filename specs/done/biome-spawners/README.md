# Biome spawn tables — own-jar data and one runtime consumer

固定1.21.1のbiomeが持つ`spawners`/`spawn_costs`を既存のfeature registryへ読み、
夜間hostileの種類選択をbiome依存にする限定契約です。vanillaの`NaturalSpawner`
全体やmob lifecycleを閉じるものではありません。

## なぜこの形か

既存の夜間cycleはbiomeを見ずに4種類を固定順で切り替えていました。biome JSONには
8カテゴリのweighted entryとcost、確率がありましたが、loaderはそれらを一切読んで
いませんでした。データを別のspawn frameworkへ移すのではなく、既存の`FReg`と
mob ownerへ接続しました。

選択関数は、このserverが実際に動かせる4種類だけを候補にし、固定jarのweightと
entry順を使います。Endのように候補が無いbiomeでは既存のrotationへfallbackし、
存在しないspawn entryを捏造しません。`spawn_costs`は読み込み・照合までで、
energy budgetの適用は未実装です。

## 維持する不変条件

- `spawners`のカテゴリ名・entry順・type・min/max・weightを保存する。
- `creature_spawn_probability`はJSON名`creature_spawn_probability`を読み、省略時は
  `MobSpawnSettings`のdefault `0.1`を使う。
- `spawn_costs`の`energy_budget`/`charge`をf64のまま保持する。
- 実クラス`Biome.getMobSettings()`を正解とし、donorの表や推測したweightを使わない。
- 対応していないmobを黙って追加せず、fallbackを明示する。
- suite緑をNaturalSpawner parityやwhole-world acceptanceと呼ばない。

実装は [`feature.rs`](../../../src/feature.rs) の `SpawnerData` / `spawners_for` /
`choose_supported_hostile`、本番接続は [`main.rs`](../../../src/main.rs) の
`spawn_etype_for_biome`、検証は [`generation_contract_tests.rs`](../../../src/generation_contract_tests.rs)
と [`biome_spawn_oracle.py`](../../../tools/biome_spawn_oracle.py) が持ちます。

## 閉じていないもの

`spawn_costs`の適用、group spawn、difficulty/category cap、despawn/persistence、
個別mobのspawn placement、natural spawnerの周期、whole-worldのmob比較、性能は
別です。既存の夜間cycle自体もvanillaのNaturalSpawnerではありません。

## 捨てた道

- 全spawner typeをそのままmobへ追加する案: 既存runtimeが動かせないtypeの挙動を
  捏造することになる。
- biomeを見ない固定4種のままにする案: データを読んでも本番経路が変わらない。
- `spawn_costs`をweightへ混ぜる案: energy budgetは別の制約で、weightの代用にできない。
