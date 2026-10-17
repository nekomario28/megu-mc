# 落下ブロック（砂・砂利）

## 実装：vanilla の `FallingBlockEntity`

以前は「世界に残したブロックを1 tick に1セル動かす」近似だった。vanilla はブロックを
世界から外し、`FallingBlockEntity` として落とす。実装をエンティティモデルへ置き換えた。

- 支持を失った重力ブロックは `spawn_fall` で**世界から消え**、`Fall { eid, x, y, z, dy, state, time }`
  になる。`note_falling` は changed cell とその上の2セルだけを見る（走査しない）。
- 物理はバイトコード契約（[falling-vanilla](../falling-vanilla/README.md)）どおり：
  毎 tick `dy -= 0.04`、`y += dy`。位置は `y_n = y0 - 0.02 n(n+1)`、減衰なし。
- 着地は、**掃引**した支持の上面 `top` に達した時点。1 tick の移動が1ブロックを超えても、
  旧位置から新位置までを走査するので、間に挟まった1ブロックの床を飛び越えない。
  `top` のセルへブロックを設置し、`note_block_update` を送ってエンティティを破棄する。
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
  エンティティへ変換され、世界のブロックは**明示的な空気**へ置き換わる（下の地形を露出しない）。
- `a_fast_falling_block_lands_on_a_thin_platform_it_crosses`：落下速度が1ブロック/tick を
  超えても、掃引が途中の1ブロックの床を捉えて着地する。

- `a_save_concurrent_with_falling_conserves_every_block`：落下中に別スレッドが `SAVE_LOCK` 下で
  スナップショットを取り続け、それらをすべて再読み込み・着地させてブロックが保存されることを
  検査する（遷移が save と直列化されていないと、途中状態のスナップショットで欠落/重複する）。

`tools/mutation_check.sh` が対照を赤にする：セル自身/上のセルの検査除去、資産集合→手書き3状態、
重力を `0.04→0.08`、着地面を上げる、掃引の開始点を旧位置→新位置に戻す、`FAL1` の明示空気を
非空気にする、冷チャンク判定除去、`FAL2` タグ改変、通過中エンティティによる新ブロック抑制。

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
- **着地セルの原子的確保**：着地は `fallable_state(current) && set_block_if(..., Some(current))` で
  再検査と書込みを世界ロック下で CAS する。飛行中に接続スレッドが着地セルへ置いたブロックを
  上書きしない（失敗時は上に乗せて次tickで着地）。
- **エンティティ化の原子的確保**：`spawn_fall` は check-then-clear ではなく既存の
  `set_block_if(..., Some(st))` で CAS し、実際に外したスレッドだけがエンティティを作る
  （2つの接続が同じブロックを二重に落とさない）。
- **配置後の通知は世界の現在状態**：アイテム配置だけでなく `/setblock`・`/fill`・piston の
  配置も `place_block` を通し、`set_block` の後に `block_at` を通知する。未支持の砂を置くと
  その場でエンティティ化するが、要求した砂を通知するとクライアントに幽霊ブロックが残る
  （`placing_unsupported_sand_queues_air_not_a_ghost` と変異で固定）。
- **世界↔エンティティの遷移と save**：`spawn_fall` と `falling_tick` の着地適用、および
  下世界で item 化するドロップの生成を再入可能な `with_save_guard`（`SAVE_LOCK`）で挟む。
  signal スレッドの save が「ブロックは消えたがエンティティは未登録」「着地ブロックは置いたが
  エンティティはまだキュー」「ドロップは作ったが飛行中エンティティがまだキュー」という
  中間状態を見ない（`spawn_fall` は再帰するので最外殻だけがロックする）。
- **再帰上限を超える柱**：`block_updated` の深さ上限で破棄せず、更新をキューに積んで最外殻から
  反復処理する。64 を超える支持除去柱も1回で全段エンティティ化する
  （`a_long_unsupported_column_converts_in_one_pass` を高さ70にし、変異で固定）。
- **並行 save の可視性**：`falling_tick` はキューを `mem::take` せず clone し、処理後に処理済み id
  だけ除いて生存者を戻す。SIGTERM/`save-all` が tick 中に走っても、飛行中エンティティが空の
  キューとして保存されない（`beginning_a_falling_tick_leaves_the_queue_visible` と変異で固定）。
- **高速落下のすり抜け**：支持走査を新位置のセルからではなく旧位置の足元から始め、新位置まで
  掃引する。1 tick で1ブロックを超えて動いても、途中の1ブロックの床を飛び越えない
  （`a_fast_falling_block_lands_on_a_thin_platform_it_crosses` と変異で固定）。
- **FAL1 変換の地形露出**：旧セルのオーバーレイを削除すると生成地形が露出して二重になるため、
  明示的な空気を記録する（`a_legacy_fal1_save_converts_its_blocks_to_entities` と変異で固定）。

## なお残る限界

- `note_falling` は `py <= MIN_Y` を skip する（世界底面の重力ブロックは変換しない）。
- `canSurvive` 補正、`levelEvent(2001,…)`、`cancelDrop`、落下ダメージは未実装。
  信頼性19種（sand/red_sand/gravel/concrete_powder）は `EntityBlock` でも
  hurting block でもないので `BlockEntity`/`setHurtsEntities`/`causeFallDamage` は N/A。
- **同一入力の vanilla 実行照合は未実施**（バイトコード導出のみ）。

## 同一入力オラクルの道具（途中）

`tools/falling_oracle.py` は `/setblock` で床と砂を置き、サーバが送るブロック更新
（`0x49` セクション更新）を列ごとに集めて、最終列を JSON で出す。チャンク本体は復号しない。

- **mc-spine**: `MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS=1` で `/setblock` をチャット経由で実行。
  `(x,z)=(0,0)`、床 stone@150、砂@153 → 最終列 `150=stone, 151=sand`（床の1つ上に着地）。
- **vanilla**: コンソール（FIFO stdin）経由で同じ入力列。**空サーバは tick しない**ため
  （`pause-when-empty-seconds` 既定やチャンク tick にプレイヤーが必要）、CLI クライアントを
  スポーンに接続してから実行した。`/setblock 0 150 0 stone`、`/setblock 0 153 0 sand`、10秒後：
  **150=stone, 151=sand**、149/152/153 は空、falling_block エンティティ無し。
- **mc-spine の結果と一致**：同じ入力で砂は床の1つ上（y=151）に着地する。
  `tools/falling_oracle.py` の mc-spine 出力は `final_column {150:stone, 151:sand}`。

## 独立レビュー（flash）の指摘と対応

`97b9daf`（下世界ドロップの並行 save テスト）を `opencode-go/deepseek-v4.1-flash` で
レビュー。2件の指摘を修正した（`c5ec1e8`）：

- **保存則がアイテム同一性を見ていなかった**：全ての drop を合計していたため、sand を失って
  別アイテムだけ残ったスナップショットでも 1 になってしまう。**sand アイテムだけ数える**
  ようにした。
- **restore がパニック安全でない**：restore はアサーションの後ろの直線コードなので、
  途中で panic すると world/falling/drops/forceloaded/region を次のテストへ漏らす
  （hot set の pin はこれを隠していただけ）。**`WorldSnapshot` ガード**にして全 unwind 経路で
  restore するようにし、両落下テストで使う。

save-guard 変異は依然として両テストを赤にし、verify は緑（137 passed / 44 proved）。

### レビュー修正への再レビュー（flash）でもう1件

`c5ec1e8` を再レビュー。**RAII ガードの `.lock().unwrap()` は、アサーション失敗でミューテックスが
poison されるとデストラクタ内で panic し、二重 panic でテストバイナリ全体を abort** させる
（＝単一の赤がバイナリ全体の異常終了になり、restore も走らない）。poison 耐性
（`unwrap_or_else(|e| e.into_inner())`）に修正。saver スレッドも `StopOnDrop` で panic 時に停止する。
save-guard 変異を与えても **abort せず 2 failed** になることを確認（`verify` は 137 passed / 44 proved）。
