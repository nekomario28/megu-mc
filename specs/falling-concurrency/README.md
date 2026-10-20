# 落下と並行操作のハードニング（close-defects から繰り越し）

Status: 実装済み（2026-10-20）。`src/tick_tests.rs` の 5 テストと
`tools/mutation_check.sh` の対応変異で固定した。

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

## テスト（`src/tick_tests.rs`、2026-10-20 実装）

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
（時間・スレッド数に依存する assert を置かない）。

実装時の注意:

- 「支持を壊す」ためのセル消去は**上から順に**行うか、消去後に `falling()` を
  空にする。下のセルを先に消すと、その上のブロックが entity 化し、前の scenariof
  の entity がスナップショットへ混入する（テスト側の取り違えで 1 件実際に踏んだ）。
- ドロップの個数は block state ではなく **item id** で数える
  （`name_to_item("sand")`）。`Drop.item` は item id で、block state と一致しない。
- 着地のたびに支持の上面を assert し、同じテストで保存/再読込の前後を比べる。
  再読込後に「ブロック数が 1」であることが、entity の cell を ghost として
  書き戻す実装を落とす。
- 変異は**有限時間で終わる**ものだけを登録する。`spawn_fall` の CAS を
  「セルを同じ値のまま成功させる」へ変えると、`note_block_update` が同じセルを
  再び falling として積み続け、ティックあたりの仕事が増えてテストが停止しない
  （実際に 3 分以上スピンし、実行を kill して `src/main.rs` を復元した）。
  着地側の `if false` のような「置かない」変異は 0.2 秒で赤になる。
- 変異の実行が中断されたら、まず `src/*.mutation-backup` を確認して
  `cp` + `touch` で復元し、`sha256sum` が HEAD と一致することを確かめてから
  再実行する（kill では EXIT trap が走らない）。

## 完了条件

- [x] 追加した性質テストが緑（5 件、`cargo test` 147 passed / 1 ignored）。
- [x] 対応する変異を `tools/mutation_check.sh` に登録（5 件追加）。
- [x] `tests/verify.sh` が緑（2026-10-20、`fix/20tps-integrated`）。
- [ ] 新しいフレークがないこと: 同一 binary で 3 回連続実行して緑（下記の実行記録）。
