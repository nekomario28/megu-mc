# 15 — region構築の候補を測定して不採用

## なぜ

20TPS同一条件で `hot_regions` が支配的だった（Actor 88–97%、炉 44–47%）。
HashSetへの重複insertをVec+sort+dedupへ置き換える候補を作り、測定した。

## 結果

| fixture | 比 |
|---|---:|
| 空 | x0.98 |
| idle Actor 1 | x1.23 |
| 休止炉400 | x1.11 |
| 稼働炉400 | x1.10 |
| 同chunk Actor 100 | x1.17 |
| 密集 Actor 100 | **x0.72** |

プロセスCPU 0.2766s → 0.3469s。

## 不採用

まばらな中心では速いが密集では40%遅い。プレイヤーは基地に集まるので
密集側が現実的。revertした。

sortは候補数比例、hash dedupは集合サイズ比例。密集すると候補が集合を
大きく超えてsortが不利になる。

## 契約

- 次の候補は「全fixture形状でwin」してから着手する。
- 中途半端なwinで本番を遅くしない。
- `region_membership` の独立oracleは候補のバグを即座に特定できた。
  このゲートは維持する。

証拠は [region-build-rejected](../../../evidence/time-contract/region-build-rejected/README.md)。
