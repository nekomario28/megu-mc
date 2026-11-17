# ドロップの山 — バケットが密集に効くかの検証（陰性結果）

## 動機

セルバケットは O(n × 近傍数)。600ドロップが1か所に集中すると
近傍数が n に近づき、O(n²) に戻るのではないかと懸念した。

## fixture

- 600ドロップを 3×3 ブロック内に集中、全部マージ可能
- 600ドロップを 12ブロック四方に分散、別itemでマージしない

## 結果

| fixture | tick | drops stage |
|---|---:|---:|
| drop_pile | 10,272µs | 0.3ms（3%） |
| entity_heavy（参考） | 337,607µs | 148.3ms（44%） |

**集中は問題にならなかった。**

## 理由

集中したドロップは**最初の1tickで1スタックに畳まれる**。
以後バケットには1エントリしか無いので、近傍も1。
密集は自己消滅するため、バケットの悪いケースにならない。

分散側は 8×30 = 240セルに約2.5個ずつ。
近傍27セルで約67候補、600×67 = 約4万比較。安い。

**教訓：バケットの近傍が n に近づくには、
「近くて、かつマージできない」ドロップが大量に必要。**
item/comps/スタック上限が異なる密集ドロップがそのケース。
現fixtureはそこを踏んでいない。

## environment が未解明

entity_heavy の environment が 178.8ms（53%）で最大。
600xpで1個あたり約298µs。mutex lock としては高すぎる。

`keepalive_time_tick` の内部は作物成長も含む。
作物は `set_block`（ occlusion・batching・invalidation ）を呼ぶので
そこが原因の可能性がある。**内部の特定は未実施。**

次に調べるなら `keepalive_time_tick` をstage分割して測ること。
現状は environment が1ステージに固まっていて原因が絞れない。

## 訂正: 単位（1000倍）と、その後の測定

このページの「10,272µs」「337,607µs」は **µs/1000tick を µs/tick として読んだ**もの。
`results.json` の `total_ns` は1000 tick分の合計なので、1 tick あたりは

- drop_pile **10.27µs/tick**（drops 0.259µs, 3%）
- entity_heavy（参考）**337.61µs/tick**（drops 148.3µs, 44%）

である（`5bc49c1`）。「environment が 178.8ms（53%）」も同様に **178.8µs/tick**、
orb 1個あたり 0.298µs。

このページの「次に調べるなら `keepalive_time_tick` をstage分割して測ること」は
実行済みで、答えは [entity-merge-index](../entity-merge-index/README.md)：
environment の 81% は `env/xp_merge`（orbの全ペア走査, 105µs/tick）だった。
同ページで両方のマージをセル索引に置き換え、entity_heavy は
`5c6ccec` 345.18µs → **242.00µs**。集中した山（このページの主題）は
その変更後も drops 0.259µs → 0.098µs で、集中が悪いケースにならないという
このページの結論は変わらない。
