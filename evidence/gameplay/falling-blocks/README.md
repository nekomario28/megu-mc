# 落下ブロック（砂・砂利）

## 実装

`Reg.behaviour` に `B_FALLING` を追加し、`sand` / `red_sand` / `gravel` /
`*_concrete_powder` を割り当てた。**ランダムtickの挙動IDと同じ欄を使う** —
`behaviour_of(st)` は状態から挙動への唯一の問い合わせであり、
2つ目の消費者（落下）が同じ共通部品を共有する。

- **キューは近傍更新で埋まる**：`block_updated_inner` が
  `note_falling_above(x,y,z)` を呼び、**下が空いたブロックの上**だけを見る。
  走査はしない（vanilla の neighborChanged と同じ位置）
- `falling_tick` は1tickに1ブロック落とし、着地したらキューから外れる
- `set_block` と `note_block_update` を通すので、クライアントにも反映される

## ゲート

`sand_falls_one_block_a_tick_until_it_lands`：
空中の砂が床に着地し、元いた場所と通過した場所が空気になることを検証。

## 未解明（記録）

**tick 数を主張していない。** 2セルの落下が**1パスで終わった**ように観測され、
説明できていない。着地そのものは固定した。
**説明できないものを主張しない**（この枝の規則）。

候補：
- `falling_tick` の `again` への再投入が期待どおりでない
- テストのループ条件が1回で抜けている
- `set_block` 経由の `block_updated` が別の経路で動かしている

## 未着手

- 落下中の見た目（vanilla は落下ブロック**エンティティ**。我々はtickごとにブロックを移す）
- 落下中のプレイヤーへのダメージ
- `concrete_powder` の着水時の固化
