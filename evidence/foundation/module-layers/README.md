# モジュールの層を depsgx で検査した（S2.2 の前提）

`gxkit` の `depsgx` はモジュール依存グラフを検査する
（循環・層違反・禁止依存・到達不能）。**S2.2 で `main.rs` を分割する前に、
分割先の層が既に成立しているか**を機械的に確かめるために使った。

## 結果：**3 errors — 既存モジュールに循環がある**

```
inspect: 13 problems (3 errors)
  [1] error feature   cycle: dependency cycle: feature → tree → feature
  [2] error tree      layer-violation: 'tree' (layer 4) depends on 'feature' (layer 3)
  [3] error worldgen  layer-violation: 'worldgen' (layer 4) depends on 'feature' (layer 3)
```

**実測した辺**（`crate::` 参照から）：
`main → feature`、`feature → tree`、`feature → worldgen`、`tree → feature`、
`tree → worldgen`、`worldgen → feature`。

**これは `main.rs` の分割が「片付け」ではないことを意味する。**
光エンジン（362行）を新しいモジュールへ出しても、
**そのモジュールはこの循環に加わるだけ**で、層は良くならない。
**S2.2 の最初の仕事は、層を作ることではなく循環を壊すことである。**

### 循環を壊す道筋

`feature` は registry（ブロック知識の所有者）であり、
`tree` と `worldgen` はその上に載る生成の層である。したがって向きは
**`tree`/`worldgen` → `feature` だけ**でなければならない。
逆向きの `feature → tree` / `feature → worldgen` が壊すべき辺である。

**測るべき次の一手**：`feature.rs` の中で `tree::` / `worldgen::` を参照している箇所を列挙し、
それが**本当に registry の責務か**（そうなら呼び出し側へ押し出せる）を1つずつ判定する。
**押し出せないものだけ**を下位の層へ移す。

## この測定の盲点（正直に）

`cached_lookup` / `effects` / `java_compat` / `landing` / `world_rng` が
「どの根からも到達できない」と info で出ているが、**これは私の測定の穴**である。
`main.rs` はこれらを `mod x;` で宣言し、**`crate::` を付けずに** `landing::player_move` のように呼ぶ。
私の辺の抽出は `crate::` だけを拾ったので、**`main` からの辺が落ちている**。
**dead code ではない。** 次に測る人は `mod` 宣言と裸の `<mod>::` 参照も拾うこと。

**教訓**：**ツールの info は「コードが悪い」ではなく「私のモデルが不完全」であることが多い。**
今日、protogx でも同じ形を3回踏んだ（毎回コードの方が寛容だった）。
