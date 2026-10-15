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
重力を `0.04→0.08`、着地面を1上げる、冷チャンク判定除去、`FAL2` タグ改変、通過中エンティティによる新ブロック抑制。

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

## 解決した乖離（独立レビューの指摘）

- **置換可能ブロックの通過**：`fallable = is_air || is_liquid || is_replaceable` にした。
  花などの置換可能ブロックを通過し、着地時に置き換える。`a_falling_block_passes_through_a_replaceable_block`
  と変異で固定。
- **長い柱**：`UPD_DEPTH` を 64 に上げ、30段の支持除去が1回で全段エンティティ化することをテスト。
- **着地セルの再検査**：バッチ着地は設置直前に `fallable` を再確認し、埋まっていれば上に乗せて次tickで着地する。
- **下世界のドロップ**：`new_y < MIN_Y` では破棄せず `block_drop` を item 化。
- **save の直列化**：`SAVE_LOCK` で tick autosave と signal ハンドラの同時書き込みを防ぐ。

## なお残る限界

- `note_falling` は `py <= MIN_Y` を skip する（世界底面の重力ブロックは変換しない）。
- 旧 `FAL1` の変換は記録に状態が無いため `block_at` を読む。オーバーレイに無いセルなら
  地形の `base_state` を持ち上げてしまう（実運用の落下セルはオーバーレイにあるため通常は起きない）。
- `canSurvive` 補正、`levelEvent(2001,…)`、`cancelDrop`、落下ダメージは未実装。
  信頼性19種（sand/red_sand/gravel/concrete_powder）は `EntityBlock` でも
  hurting block でもないので `BlockEntity`/`setHurtsEntities`/`causeFallDamage` は N/A。
- **同一入力の vanilla 実行照合は未実施**（バイトコード導出のみ）。

## 同一入力オラクルの道具（途中）

`tools/falling_oracle.py` は `/setblock` で床と砂を置き、サーバが送るブロック更新
（`0x49` セクション更新）を列ごとに集めて、最終列を JSON で出す。チャンク本体は復号しない。

- **mc-spine**: `MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS=1` で `/setblock` をチャット経由で実行。
  `(x,z)=(0,0)`、床 stone@150、砂@153 → 最終列 `150=stone, 151=sand`（床の1つ上に着地）。
- **vanilla**: コンソール経由の `/setblock`+`/execute if block` を試したが、
  「Test passed」が砂の元位置で返り、チャンクの tick とコマンド実行の確認が取れず**未確定**。
  クライアントチャット経由も vanilla 1.21.1 の署名付きチャット形式に合わず未確定。
  **同一入力の vanilla 実行照合は未完了**で、Q の残件として明示する。
