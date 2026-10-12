# 実体を含むfixture — それまで見えていなかった最大の費用

## なぜ作ったか

slice 18で「残り20ヶ所のhoistはfixtureに要素が無くて測れない」と書いた。
fixtureにdrop・xp・作物・Mobを入れて測った。

## 発見: tick 667ms、その72%が drops

| fixture | tick | 上位stage |
|---|---:|---|
| 空 | 1.18ms | environment 22% |
| idle Actor 1 | 10.09ms | hot_regions 88% |
| 稼働炉400 | 17.11ms | hot_regions 52%, furnaces 41% |
| **entity_heavy** | **667ms** | **drops 72%**, environment 27% |
| entity_heavy_furnaces | 674ms | drops 71%, environment 26% |

600ドロップ・600xp・600作物・600Mobで **667ms/tick**。
これは20TPSを破綻させる水準（1tickに667msもかかる）。

## 原因: drop_tick のマージが O(n²)

`src/main.rs` の drop_tick にあるvanilla準拠のマージ：

    for i in 0..dp.len() {
        for j in (i + 1)..dp.len() {
            if ait == bit && dp[i].comps == dp[j].comps
                && (ax-bx)² + (ay-by)² + (az-bz)² < 0.25
                && acnt + bcnt <= max { ... break }
        }
    }

600ドロップで約18万ペア/tick。1ペアあたり約2.7µs。
`dp[i].comps == dp[j].comps` のVec比較が距離判定より先に評価されている。

## 修正方針（未実装）

セルバケットで近傍だけを比較する。
`|ax-bx| < 0.5` なら `floor(ax)` と `floor(bx)` は高々1違い。
したがって**同じセルと隣接26セル**に入るものだけが候補。

等価性を保つには：
- `i` は元の順序で回す
- 各 `i` について近傍27セルのindexを集め、`j > i` でフィルタし、**昇順に並べて**試す
- 最初に一致した `j` でmergeしてbreak（元の挙動と同じ）

順序を変えると「どのドロップが吸収されるか」が変わるので、
**sortしてから比較する案は等価ではない**。候補の並び順を元index昇順に保つこと。

## 状態

fixtureのみ追加。drop_tick は未修正。
serial/parallelとも32 passed・1 ignored、Fabric/NeoForge check合格。
