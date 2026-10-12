# 04 — 結果不変のhot/warm集合構築

## Why

[切替前fixture](../../../evidence/time-contract/tick-baseline/README.md) で、ringの
重複挿入が局所的な費用の大きい対象だった。全サーバーで支配的という証拠ではない。
キャッシュや起床管理を増やさず、集合演算の等価変形だけで削減する。

## 不変条件

forced chunksはhotにそのまま入る。clients/actorsの中心からChebyshev半径5がhot。
warmはhotを半径3だけ拡張した集合。集合拡張はunionに分配でき、
プレイヤー由来の半径5＋3は元の中心から半径8となる。
forced由来は半径3のまま。同じ中心は重複排除。密集した中心では直接拡張の挿入回数が
増えるので、挿入回数の少ない等価なunion構築を選ぶ。これは休止や時計のmodeではない。
hot/warmの所属・処理tick・RNG・保存を変更しない。
中心のfloor/div_euclidとlock順を維持し、hot lockを保持してposを読まない。
集合のiteration orderは契約ではない（元々HashSet）。

## ゲート

既存refresh_hotに対して、空・forced・重複する中心・離れた中心・正負のchunk境界・
中心の移動/消失・forcedの追加/除去を実入力で検証する。
元の「hotを3拡張」参照集合と、実際のhot/warm全要素を比較する。
新実装の拡張半径を誤らせる負の対照で失敗を確認する。
全テスト、Fabric/NeoForge check、独立レビューを通す。
同じrunner・同じfixture・同じrelease設定で3プロセス再測定し、tick時間とstage費用を比較。
統計的な性能保証や20TPS完了を主張しない。

## 検証済みの範囲

[証拠と限界](../../../evidence/time-contract/region-expansion/README.md)を参照。
元実装でも新しい集合ゲートは合格し、誤った半径では失敗した。
全てのfixtureで高速化したわけではない。単一Actor・炉・同一chunkの多数Actorは改善、
密集する多数の中心では小さな測定上の退行を残す。
レビューで発見した共有fixture競合は実際に再現してtest-only lockで修正した。
