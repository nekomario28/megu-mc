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

---

## 目標の層を**実装前に**証明した（`deps_target.py` → no problems）

**循環の原因は、`feature.rs` が2つのものを兼ねていることである。**

| 兼ねているもの | 本来の向き |
|---|---|
| **ブロック registry**（`breg` / `name_to_state` / `is_solid` / …） | **最下層**。生成の層が依存してよい |
| **feature/config 系**（`Cf` の言語・配置） | 生成の層の**上**。木を生成するので `tree` に依存する |

**`feature → tree` の実参照は4箇所だけ**である：

| 行 | 内容 |
|---|---|
| 8 | `use crate::worldgen::*;`（実際に使うのは `Gen` / `default_state` / `surface` / `load` 程度） |
| 1243 | `Tree(Box<crate::tree::TreeExtra>)` — config の変種 |
| 1422 | `crate::tree::p_tree(j)?` — config の解釈 |
| 2157 | `crate::tree::gen_tree(ctx, rng, pos, t)` — **木の生成** |

`min_y` の22件は**ローカル引数**であり、モジュール参照ではない（数え間違い）。

**提案（低い層から）**：

```
feature (registry)
  → worldgen          (地形; ブロックIDが要る)
  → tree              (木の形; 地形とブロックIDが要る)
  → featuredefs       (config 言語と配置; 木を生成する)
  → main
```

**`deps_target.py` を depsgx にかけると `inspect: no problems`。**
**実装前に、目標の層が無循環であることが機械的に確認できた。**

**次の一手**：`feature.rs` から config/config 解釈/配置（`Cf`、`p_*`、`gen_*`）を
`featuredefs.rs` へ移す。**1 community = 1 コミット**、テストが緑のまま、
**移すたびに `depsgx inspect deps.py` の error が減ることを確認する。**
