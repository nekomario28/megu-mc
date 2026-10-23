# 投射物の掃引範囲（レビュー [P2] の修正）

## 指摘

issue #7：矢のヒット判定が `tick` の移動区間の**一部しか走査していない**。

    for st in 1..=4 {
        let t = st as f64 / 4.0;
        let (sx, sy, sz) = (
            ax - a.dx * 0.9 * (1.0 - t),   // ← 0.9 は ドラッグ係数
            ...
        );

そのとおりだった。`a.dx` はそのtickに**実際に適用された変位**（0.99 を掛けた後）
なので、掃引すべき区間は `[現在位置 - a.dx, 現在位置]` 全体。ところが
`0.9 * (1.0 - t)`（t = 0.25, 0.5, 0.75, 1.0）は区間の**後ろ 67.5%** しか標本に
しない。残る先頭 32.5% は、現在位置の標本（t=1）の 1.2 幅の箱では覆いきれない。

## 測定：どこから見えるか

最深の標本は「tick の開始位置 + 0.325 × 変位」にあった（`0.9 * 0.75 = 0.675`）。
開始位置にいる対象がその箱（`rx = 1.2`）から外れる条件は

    0.325 × 変位 > 1.2   →   変位 > 3.69

`the_projectile_sweep_covers_the_whole_tick` がこの境界を実測して
`first_gap == 3.7`（3.5 では差が出ない）で固定している。矢の1tick目の変位は

- フルチャージの一射：`3.0 * 0.99 = 2.97` → **閾値以下**。まっすぐ飛ぶ矢では
  この欠陥は出なかった（レビューが「high-speed crossing」を要求した理由）。
- 長く落下した矢：重力 0.05 / ドラッグ 0.99 で `dy → -4.95`（`|a.dx|` は更に加算）
  → **閾値を超える**。至近距離の対象を跨ぐ tick で取り落とす。

## 修正

掃引を関数に切り出し、区間全体を標本にする。

    fn projectile_sweep(pos, d) -> [(f64, f64, f64); PROJECTILE_SWEEP_STEPS + 1] {
        // back = 1 - i / STEPS  →  i = 0 が「tick の開始位置」
    }

ヒット判定の箱も `projectile_reaches(sample, target, rx, y_lo, y_hi)` に切り出し、
mob（1.2 / 0.4 / 2.1）と player（0.7 / 0.3 / 2.0）の両方が同じ関数を使う。
標本は1tickに1回だけ作る（従来は対象ごとに4点を再計算していた）。

## ゲート

- `tick_tests::the_projectile_sweep_covers_the_whole_tick`
  - 最初の標本 == tick の開始位置、最後 == 現在位置（区間全体）
  - 開始位置の対象が旧区間では**当たらない**こと（旧式を test 内で再構成して確認）
  - 旧区間が取り落とす最小変位 == 3.7（実測）
- `tick_tests::the_production_arrow_loop_hits_a_target_it_leaves_the_bow_inside`
  - 本番 `game_tick` で、至近距離（弓の 0.1 ブロック先）の mob に初tickで
    6.0 ダメージが入る（`hp 20 → 14`）

どちらも `tools/mutation_check.sh` のアンカーで「旧区間
（`back = 0.9 * (1 - (i+1)/STEPS)`）に戻すと赤くなる」ことを確認済み
（mutation check 55 proved / 0 not a gate）。
