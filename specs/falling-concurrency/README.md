# 落下と並行操作のハードニング（close-defects から繰り越し）

Status: 設計。実装前。逐次修正で収束しないと判断された範囲を、性質テストとして固定する。

## なぜ

F の統合レビューで 12 件の並行/順序の欠陥を逐次修正したが、同じ静的レビューを
繰り返すと新しい端ケースが出続けた（[close-defects](../close-defects/README.md)）。
個別のケースを足すのをやめ、**守恒**と**順序**を性質として検査する。

## 対象

落下ブロックの遷移（設置 → エンティティ化 → 着地 → ドロップ）と、
セーブ・破壊・設置・チャンクのホットセット変更が同じティック列で交差する場合。

## 性質

1. **ブロック守恒**: 任意の保存スナップショットで、
   `world のブロック数 + falling のエンティティ数 + drops の個数` が初期と一致する。
   二重化も消失もしない。
2. **順序**: ティック列が同じなら、スレッドの実行順に関係なく最終状態が同じ
   （決定的な seed と固定した interleaving で検査）。
3. **無デッドロック**: 保存ロックとワールドロックの取得順が固定で、どの
   テストも期限内に終わる。
4. **ホットセット非依存**: 落下は region のホットセットに依存しない。変更が
   途中で入っても、次のティックで正しいセルが処理される。

## テスト案（`src/tick_tests.rs` に追加）

- `a_support_placed_under_a_falling_entity_is_landed_on`: 落下中に支持ブロックを
  置き、着地位置が新しい支持の上面になる。保存/再読込でも同じ。
- `a_knockout_and_a_save_in_the_same_tick_conserve_the_block`: 破壊と保存を
  同一ティックで交互に実行し、各スナップショットで性質 1 を検査。
- `two_knockouts_of_adjacent_supports_conserve_every_block`: 2 スレッドで隣接
  支持を壊し、性質 1 と 3 を検査（反復回数は固定、時間に依存しない）。
- `a_save_during_entity_conversion_has_no_duplicate_drop`: エンティティ化の
  途中状態で保存し、再読込後のドロップ数が 1。
- `region_change_mid_fall_keeps_the_entity`: 落下中に `set_test_region` で
  ホットセットを変えても、次のティックで着地する。

各テストは `world_guard` と `WorldSnapshot` で世界を復元し、反復回数を固定する
（時間・スレッド数に依存する assert を置かない）。変異（`with_save_guard` の
ロック除去など）で赤になることを `tools/mutation_check.sh` に登録する。

## 完了条件

- 追加した性質テストが緑で、対応する変異で赤になる（gate の load-bearing 証明）。
- `tests/verify.sh` が緑（142 passed 以上、mutation 46 proved 以上）。
- 新しいフレークがないこと: 同一 binary で 3 回連続実行して緑。
