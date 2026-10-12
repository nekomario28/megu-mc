# 飛翔体のdragを速度に適用（レビュー [P1] の修正）

## 指摘

issue #7：
> Arrows move `x += dx*0.99` then gravity changes only `dy`. `dx,dz` never decay,
> so this is *not* Vanilla's per-tick drag; flight path and collision differ
> increasingly over time. After 60 ticks with horizontal speed 1, the no-drag
> integration yields 59.4 blocks vs ~45.3 for a repeated 0.99 drag.

そのとおりだった。slice 13で**係数をvanilla値（0.99）にした**が、
**積分器は非vanillaのまま**だった。抗力が変位に乗るので `dx,dz` が減衰しない。

## 修正

vanilla の順序にした：速度を `scale(0.99)` → 重力を加算 → その速度で位置を積分。

| 飛翔体 | 変更 |
|---|---|
| 弓矢 | `dx*=0.99; dy-=0.05; x+=dx` |
| パール | `dx*=0.99; dy-=0.03; x+=dx` |
| 釣針 | 同上（値は既存のまま、積分順序のみ） |

3箇所を1つずつ適用し、各段階でビルドとテストを確認した。

## ゲートの作り直し

最初に書いた検査は**算術の性質だけ**を見ていたため、
実装を元に戻しても通ってしまった（production を読んでいない）。

`the_production_arrow_loop_decays_horizontal_velocity` に置き換えた。
本番の `arrow_tick` を game_tick 経由で駆動し、
各tickの**速度**と**変位**を観測する。

- 速度が毎tick `× 0.99` で減衰する
- 変位がその速度に**等しい**（`dx*0.99` ではなく `dx`）
- 5tick後に速度が 0.96 未満

負の対照：変位にdragを戻すと失敗。

**教訓：算術の性質ではなくproductionの経路を検証する。**
slice 09以降で繰り返している同じ誤り。

44テスト pass（serial／8thread）、wire smoke pass、Java adapter × 2 pass。
