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

---

## 抽出の実測（次に着手する人へ）— **私のコンテキストでは完了できなかった**

**移動対象の実測値**：

| 対象 | 行 | 大きさ |
|---|---|---|
| `gen_cf`（配置の本体。`Cf` を match する） | **2153–3016** | **864行** |
| config 解釈（`RT`〜`p_cf`、`Cf` は 1201） | 1083–1468 | 約385行 |
| `TreeExtra`（型） | tree.rs 1519 | **14行** |

**`feature.rs` は 3,085行**なので、featuredefs 側は**約1,300行（42%）**の移動になる。

**「最小の修正」は最小ではない**：`TreeExtra` を下位へ移す案は、
その14行が `BSP` / `FS` / `Deco` / `RootP` / `TrunkPl` / `FP` / `IP`（**すべて `tree.rs`**）に
依存するため、**木のデータ型クラスタごと動かす**ことになり、移動量は百行単位になる。

**2つの選択肢（両方とも数百行）**：

| 案 | 動かすもの | 向き |
|---|---|---|
| **A**（推奨） | featuredefs 一式（`Cf`・`p_*`・`gen_cf`、約1,300行）を**上位**へ | config 言語は生成の層の上にあるべき、という本来の向きに一致 |
| B | 木のデータ型クラスタを**下位**へ | `TreeExtra` が registry の語彙（`BSP` 等）に依存するので、向きとしては不自然 |

**案 A の手順**：
1. `src/featuredefs.rs` を作り、`gen_cf`（864行）を**そのまま**移す。
   この関数は `Cf` / `FReg` / `FC` / `Xor` / `P3` を要求するだけなので、**移すだけで `feature → tree` の
   3参照のうち2つ（`p_tree` と `gen_tree`）が消える**。
2. `Cf` と `p_cf` を移す（`tree::TreeExtra` の型参照が featuredefs 側へ移る）。
3. `feature.rs` に残るのは registry だけになる → **`depsgx inspect deps.py` の error が 3 → 0**。
4. **各段でコミット**し、そのたび `depsgx` の error 数が減ることを確認する。
   **テストが緑のままであること**が唯一の証拠。

**移すときに必要になる `pub(crate)`**：`FC` / `FReg` / `Xor` / `P3` / `BSP` / `FS` / `Deco` /
`RootP` / `TrunkPl` / `FP` / `IP` / `ore_should_place` など。**先に列挙してから着手すること。**

**なぜ私が止めたか**：1,300行の移動は、途中で止まると**ビルドが壊れた状態**が残る。
検証済みの状態で止め、実測値と手順を残す方が次の1手として価値が高いと判断した。
