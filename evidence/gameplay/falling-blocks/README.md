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

## 発見した実バグ：`falling_tick` のデッドロック

**`falling_tick` が `falling()` を保持したまま `set_block` を呼んでいた。**
`set_block` → `block_updated_inner` → `note_falling_above` が**同じロックを取る**ので、
**2段積まれた砂**（動かしたブロックの上のブロックも落下中）で**デッドロックする**。

**1段では現れない**：上のブロックが B_FALLING でなければ
`note_falling_above` はロックを取る前に return する。
**段数とキュー順序の両方に依存する**ので、テストを1つ書いただけでは出ない。

**修正**：ロックはキューの取り出しと戻しの2箇所だけで持つ。
**ゲート**：`two_stacked_sand_blocks_fall_without_deadlocking` —
2段が両方落ち、下が着地して上がその上に乗る。

**負の対照は完全ではない。** ロックを保持する版に戻すと
テストが失敗する（exit 101、ハングではない）ことは確認したが、
**意図した順序をテストから強制できず**、キュー順序が偶々デッドロックを避ける場合がある。
**順序依存であることを記録する**（テストからは固定できていない）。

## 未解明：1回の呼び出しで2セル落ちる（出力で確認）

デバッグ用のテストで列とキューの長さを毎tick出力した：

```
FALL tick1 queue=0 ["0:stone", "1:sand", "2:air", "3:air", "4:air"]
```

- 砂は base+3 に置いた。床は base（石）
- **1回の `falling_tick` の後、砂は base+1** — **2セル動いている**
- キューは空
- **`start`（ループ前）の行が出力されなかった** — これも未解明

`falling_tick` の本体は1エントリにつき1セルしか動かさない。
したがって**別の経路が同時に動かしている**か、**キューに複数のエントリが入っていた**。

候補：
- `note_falling_above` が `block_updated_inner` 経由で**追加のエントリを積んだ**
  （`set_block(x, y, z, 0)` の後、上のブロックを検査する経路）
- `set_block` の連鎖がもう1回の移動を起こした
- テストのループが1回で抜けた（キューが空だった）

**1行ずつ追った結果（絞り込み）**：

```
TRACE after clearing base+2, queue=[(9, 153, 9)]
FT enter queued=[(9, 153, 9)]
TRACE after one tick, queue=[]
```

- ループには**入っている**（`FT enter`）
- **`FT move` が出ていない** → 移動の前に早期終了している
- キューは空

`falling_tick` の早期終了は3つ：

| 終了 | キューが空になるか | ブロックが動くか |
|---|---|---|
| `behaviour_of != B_FALLING` → `continue` | なる | ならない |
| `y <= MIN_Y \|\| !fallable` → `continue` | なる | ならない |
| **`breg()` が `None` → `return`** | **なる** | **ならない** |

**3つとも観測と一致する。** ただし個別に確認した範囲では：

- `behaviour_of(sand)` = **2（B_FALLING）** ✓ 発火しない
- `fallable(9, base+2, 9)` — base+2 は空気 ✓ 発火しない

**したがって残るは `breg()` が `None` の経路のみ。**
しかし `note_falling_above` は同じ `breg()` を呼んで**キューに積めている**ので、
`Some` だったはずである。**矛盾している。**

**この矛盾を解くまで主張しない。** 次の一手：
- `falling_tick` の中で `breg()` の結果を出力する（`FT enter` の隣で）
- `reg` を取る前後で状態が変わっていないか確認する

## 未着手

- 落下中の見た目（vanilla は落下ブロック**エンティティ**。我々はtickごとにブロックを移す）
- 落下中のプレイヤーへのダメージ
- `concrete_powder` の着水時の固化
