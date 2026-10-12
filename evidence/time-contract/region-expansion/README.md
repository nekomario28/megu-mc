# 結果集合を変えないring構築のA/B

## 契約と証拠の範囲

hot/warm所属は同じ。中心のfloor/div_euclid、tick順、時間語、RNG、保存は変更していない。
集合の膨張をunionの各元へ分配し、半径5＋3を半径8として直接構築できる。
同じchunkの中心は重複排除。密集した中心では直接拡張の方が挿入回数が多くなるので、
集合が等しい二つの計算法から挿入回数が少ない方を選ぶ。これは休止やschedulerではない。
既存のforceloaded以外に長命の状態・キャッシュ・起床条件は追加しない。

source検索でhot/warmの本番consumerはis_hot/is_warmのcontainsのみと確認。
集合のiteration順はゲームRNGや保存へ渡らない。[`membership-consumers.log`](membership-consumers.log)。
回帰テストは実際のrefresh_hotに対し、正負の境界、移動、重複、離れた中心、密集、
forced追加/除去、入力消失を検証。全要素を元のhot＋skirt参照と比較する。
半径8を7にした負の対照は失敗。[`negative-control.log`](negative-control.log)。
これは既存のregion方針を保持した証拠で、vanilla simulation distanceへの一致の証明ではない。

## 限定fixtureの測定結果

同じrunner・release設定で前後各3プロセス、fixtureごと100warmup＋1000tick。
sleepなしで実行する現行4TPS時間語。各runの平均tick時間の中央値（µs）：

| fixture | 前 | 後 |
|---|---:|---:|
| 空 | 1.58 | 1.54 |
| idle Actor 1 | 37.37 | 10.45 |
| 休止炉400＋Actor 1 | 46.32 | 19.46 |
| 稼働炉400＋Actor 1 | 49.53 | 22.91 |
| 同じchunkのActor 100 | 102.26 | 16.84 |
| 10×10chunkにActor 100 | 178.28 | 184.86 |

最後の密集fixtureは約3.7%遅くなった。全負荷で改善したとは主張しない。
選択条件は挿入回数を抑えるものであり、CPU時間の最小値を保証しない。
Actorは中心を供給するだけ。実プレイヤー、Mob、液体、worldgen、Java Modは含まない。

全6fixtureを実行する子プロセスのCPU合計の中央値は0.4608s→0.2846s。
peak RSSは前14,724–14,872KiB、後14,784–14,928KiB。RSS削減の証拠ではない。
全サーバーの性能、vanilla同条件比較、20TPS互換の証明ではない。
[前の生データ](before/results.json)と[後の生データ](after/results.json)はsource/binary hash、
stage時間、分位、CPU/RSSの境界を保持する。fixtureとrunnerはコードが所有する。

## ゲート

serial/parallel全テストは21 passed・1 ignored。ignored profilerは専用runnerで実行済み。
Fabric/NeoForge checkも合格。独立レビューで共有fixtureの競合を指摘され、
2thread実行で実際に再現した。setupからrestoreまでの共通test-only lockを追加し、
既存の炉fixtureも同じlockを使用。8threadの対象テストを20回反復して全て合格。
最終独立レビューは指摘なし（レビュー自体は実行検証していない）。ログとhashはreceipt参照。

A/B前の本番mainは `ceb79b2` と同一。後のsource hashは現在のRustソースに一致。
前後のテスト差は共有lockだけで、tick計測区間の外にある。後の測定はレビュー修正後に再実行した。
時計は4TPSのまま。次はこの局所最適化を広げず、時間単位・動き・保存・遅延方針の監査へ戻る。
