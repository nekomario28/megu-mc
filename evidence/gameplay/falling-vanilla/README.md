# vanilla 落下ブロックの契約（F の土台）

`FallingBlockEntity`（obf 名 `cjg`）の 1.21.1 バイトコードから読み取った契約。
このディレクトリは**証拠**であり、実装はまだ vanilla 準拠ではない。
現行 mc-spine は落下を「1 tick に 1 セル動くブロックのキュー」で近似しており、
vanilla の速度・加速・着地位置とは一致しない。

- `FallingBlockEntity.javap.txt`：`javap -p -c` の生出力（978行）
- `FallingBlockEntity.mapping.txt`：同名の難読化解説（40行）

## 読み取った事項

| 項目 | vanilla | 出典 |
|---|---|---|
| 既定重力 | `getDefaultGravity()` は `0.04` | `FallingBlockEntity.javap` の `aZ()` |
| 重力の適用 | `Entity.applyGravity()` は `dy += -0.04`（`getGravity()!=0` のとき） | `Entity.javap` の `bb()` / `ba()` |
| 落下中 | 毎 tick `time++` → `applyGravity()` → `move(MoverType.SELF, delta)` | `tick()` 先頭 |
| 空中での減衰 | `move` 後に速度を掛ける処理は無い（`bP()` はポータル処理） | `tick()` と `Entity.bP()` |
| 着地判定 | `onGround()`（または特殊フラグ）で着地分岐へ | `tick()` offset 227 |
| 着地時 | `level.getBlockState(pos)` を見て、必要なら `blockState.canSurvive`/waterlogged を補正し、`setBlock(pos, state, 3)` して `level.levelEvent(2001, …)`、その後 entity を破棄 | `tick()` offset 239〜 |
| BlockEntity | `blockData != null` かつ `state.getBlock() instanceof EntityBlock` なら復元 | `tick()` offset 517〜 |
| ドロップ | 着地できない場合のみ `dropItem && !cancelDrop` で item 化（`disableDrop`/`cancelDrop` で抑止可能） | `tick()` offset 280〜、`disableDrop() -> s` |
| 保存 | `addAdditionalSaveData` は `Time`/`BlockState`/`TileEntityData`/`DropItem`/`CancelDrop`/`HurtEntities`/`FallHurtMax`/`FallHurtAmount`/`BlockData` を書く | mapping の `b(CompoundTag)` |
| 落下ダメージ | `setHurtsEntities(perDistance,max)` と `causeFallDamage` を持つ | mapping の `b(F,I)` / `a(F,F,DamageSource)` |

1 tick ごとの位置は `y_n = y0 - 0.04 * n(n+1)/2`（減衰なし）。1 セル落下に約 7 tick、
2 セルに約 10 tick かかる。現行の 1 セル/tick とは明確に異なる。

## まだ確かめていないこと

- `move` の衝突で着地したときの**正確な最終 y**（bounding box のオフセットとスナップ）。
- `blockState.canSurvive` が偽のときの代替ブロック・ドロップの条件。
- 溶岩/水に入ったときの挙動、`cancelDrop` の設定経路。
- vanilla を**実行して同じ入力で位置列を取得**すること（バイトコードからの読みは静的証拠で、
  同一入力の実行対照ではない）。Q の要求はこの実行照合を求める。
