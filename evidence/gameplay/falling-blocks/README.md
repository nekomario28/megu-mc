# 落下ブロック（砂・砂利）

## 実装：vanilla の `FallingBlockEntity`

以前は「世界に残したブロックを1 tick に1セル動かす」近似だった。vanilla はブロックを
世界から外し、`FallingBlockEntity` として落とす。実装をエンティティモデルへ置き換えた。

- 支持を失った重力ブロックは `spawn_fall` で**世界から消え**、`Fall { eid, x, y, z, dy, state, time }`
  になる。`note_falling` は changed cell とその上の2セルだけを見る（走査しない）。
- 物理はバイトコード契約（[falling-vanilla](../falling-vanilla/README.md)）どおり：
  毎 tick `dy -= 0.04`、`y += dy`。位置は `y_n = y0 - 0.02 n(n+1)`、減衰なし。
- 着地は `new_y` が支持ブロックの上面 `top` に達した時点。`top` のセルへブロックを設置し、
  `note_block_update` を送ってエンティティを破棄する。
- エンティティは `interest_tick` が `minecraft:falling_block`（type 40、object data = block state）で
  spawn し、`falling_tick` が `0x70` で位置を配信、着地で `0x42` 破棄する。
- **冷えたチャンクのエンティティは tick しない**（ブロックは既に世界から消え、エンティティが待つ）。
- 下が固体でないまま `MIN_Y` を割ったエンティティは破棄する。

## ゲート

- `sand_falls_under_vanilla_gravity_until_it_lands`：支持を外すとブロックが消えてエンティティになり、
  1〜9 tick の `y` が `y0 - 0.02 n(n+1)` と一致し、10 tick 目に床の上面へ設置される。
- `two_stacked_sand_blocks_fall_without_deadlocking`：支持除去で2段とも**同じ呼び出しで**エンティティ化し、
  双方が着地する（旧実装のロック順デッドロックの回帰）。
- `every_falling_block_becomes_an_entity_and_lands`：資産の落下19種すべてがエンティティ化し、
  状態を保って床の上面へ着地する。
- `a_falling_block_waits_in_a_cold_chunk`：冷チャンクではブロックが消えてエンティティになり、
  tick されず位置も変わらない。
- `persistence_tests::the_falling_entities_survive_a_save_and_load_round_trip`：位置・速度・状態・age が
  保存・再読込される。
- `persistence_tests::a_legacy_fal1_save_converts_its_blocks_to_entities`：旧 `FAL1`（セル）は
  エンティティへ変換され、世界のブロックは除去される。

`tools/mutation_check.sh` が対照を赤にする：セル自身/上のセルの検査除去、資産集合→手書き3状態、
重力を `0.04→0.08`、着地面を1上げる、冷チャンク判定除去、`FAL2` タグ改変。

## 保存形式

- 旧 `FAL1` = セル `(i32,i32,i32)` の列。
- 新 `FAL2` = `eid(i32) x y z dy(f64) state(u32) time(i32)` の列。ローダは `FAL2` を優先し、
  無ければ `FAL1` を変換して読む。`NEXT_EID` は読込んだ最大 eid を超えるよう更新する。

## 残る限界（Q）

- **同一入力の vanilla 実行照合は未実施**。上式はバイトコードからの静的導出であり、
  vanilla を起動して位置列を観測したものではない。
- `move` の衝突スナップ、`canSurvive` が偽のときの代替・ドロップ、溶岩/水、`cancelDrop`、
  落下ダメージ（`fallDistance`/`hurtEntities`）は未実装。着地できない位置への落下は
  現状そのまま設置する（vanilla は条件により item 化）。
- エンティティの bounding box は無い（点として扱う）。横方向の速度も無い（vanilla も 0）。
