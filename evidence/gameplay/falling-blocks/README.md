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

### 計測を検証した結果：矛盾はコード側にある

`falling_tick` の**3つの終了経路すべてに出力**を入れて実行：

```
FT2 top len=1
FT2 took 1
FT2 exit behaviour=0 at (9, 153, 9)     ← behaviour が 0（B_NONE）
EX after tick ["0:stone", "1:sand", ...]  ← 砂は base+1 にいる
```

**計測は機能している**（`breg()` は `Some`、ループには入っている）。
そして**矛盾が確定した**：

| 呼び出し | 同じ座標 (9,153,9) について |
|---|---|
| `note_falling_above`（キューに積む側） | **B_FALLING** と判定 ✓ |
| `falling_tick`（数ms後） | **behaviour 0** と判定 |

**同じ `block_at`、同じ座標、間に挟まるのはキューの push だけ。**
そして砂は `base+1` にある — **誰かが動かした**。

**したがって「動かしたのは `falling_tick` の移動分岐ではない」ことだけは確定した。**
`falling_tick` は `behaviour 0` を見て抜けており、移動は**別の経路**が起こしている。

### state id と名前を両方の読み出し点で出力した結果

```
EX placing sand=112 at 153
EX read back = 112                                 ← 置いた直後は sand
NFQ check (9,153,9) above=112 name=sand beh=2      ← キュー判定は sand を見ている
NFQ check (9,154,9) above=0   name=air  beh=0
FT3 read  (9,153,9)  st=0     name=air  beh=0      ← 同じ座標が空気
```

**確定したこと（すべて出力に基づく）**：

1. `set_block(9,153,9,sand)` は効いている（read back = 112）
2. キューに積む判定は**同じ座標で sand を見ている**
3. `falling_tick` は**同じ座標で空気を読む**
4. その間に挟まるのは**キューの push だけ**
5. 砂は最終的に `base+1` にいる（**誰かが動かした**）

**したがって、`set_block(9,152,9,0)` の呼び出し連鎖のどこかが
(9,153,9) のオーバーレイ項目を消しているか、動かしている。**
`falling_tick` の移動分岐ではないことは既に確定している。

### 消えた瞬間を特定した

`set_block` の前後で同じ座標を出力：

```
WHEN placed sand: sand_at_153=112
WHEN cleared base+2: sand_at_153=0      ← set_block(9,152,9,0) の後で消えている
WHEN queue=[(9, 153, 9)]
```

**確定**：`set_block(9,152,9,0)` が
**(9,153,9) のオーバーレイ項目を消している**（1つ上）。

`set_block_if` の書き込みは `local_idx(x,y,z)` で**正しいキー**を使っており、
対象位置以外を触っていない。したがって**原因はオーバーレイ書き込みの後の連鎖**にある。
`set_block_if` の残りの処理は：`signs().retain` / `chests().remove` / `furnaces().remove` /
spill のドロップ / `block_updated(x,y,z)` / `refresh_hidden_neighbors(x,y,z)` /
`note_block_update(x,y,z,st)`。

**次の一手**：この連鎖の各段の後で `block_at(9,153,9)` を出力し、
どの呼び出しが消しているかを1つに絞る。

**これは落下ブロックの問題ではなく、`set_block` の一般的な問題である可能性がある**
（隣接ブロックを消す経路がある）。**説明できるまで落下の速度も移動量も主張しない。**

## 未着手

- 落下中の見た目（vanilla は落下ブロック**エンティティ**。我々はtickごとにブロックを移す）
- 落下中のプレイヤーへのダメージ
- `concrete_powder` の着水時の固化
