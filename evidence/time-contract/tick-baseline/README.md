# 本番tickの切替前ベースライン

## 測ったこと / 測っていないこと

[`results.json`](results.json) は同じ本番 `game_tick` をsleepなしで進めたreleaseビルドの
headless/flat fixture。tickの意味は現行4TPSのまま。プレイヤー、Mob、液体、worldgen、
Java Modの費用は含まない。通常サーバーのCPU/RSSや20TPS互換性の証明ではない。
計測callback・単調時計・集計の費用は含む。通常サーバー経路にはこれらの時計読み出しを追加していない。
fixtureごとのstage時間と、全fixtureを実行した子プロセス全体のCPU/RSSを区別する。

## 結果と判断

3プロセスの平均tick時間の中央値：空1.58µs、idle Actor37.17µs、休止炉400個46.23µs、
稼働炉400個49.96µs。Actorはhot/warm ringを作る入力であり、実プレイヤーの代理ではない。
Actor付きfixtureではhot/warm集合の再構築が約35.65µs/tick。
実サーバー全体で同じ比率になるとは言えないし、絶対時間は小さい。

各プロセスは4fixtureそれぞれ100回warmup・1000回測定。
全プロセスCPUは約0.151s、peak RSSは約14.4MiB。
稼働炉fixtureのp99は170.74–171.30µs。炉の出力・進捗・燃料残量も検証し、
稼働すべき機械を省略した速いだけの測定を拒否する。

次の小さな候補はringの重複挿入の除去。結果集合は一切変えず、
「hotの各chunkをさらに3拡張」を「元の中心から合計半径へ直接拡張」に置換できる。
キャッシュや起床条件を増やす前に、この局所的な等価変形を検証する。

## 再実行と証拠

Linuxで native rootから：

```sh
python evidence/time-contract/profile_tick.py --runs 3 --output /tmp/mc-spine-tick-profile
```

runnerはreleaseテストをビルドし、各実行を専用のtemporary working directoryへ隔離する。
autosaveはそのディレクトリだけに書かれ、終了時に削除する。listenerは開かない。
fixtureは [`tick_tests.rs`](../../../src/tick_tests.rs) に置き、production経路を利用する。
時計・炉の回帰テストはstageを欠落させる負の対照で失敗した。
[`negative-control.log`](negative-control.log) は意図的なmutation失敗であり、修正前の不具合とは呼ばない。
[`suite.log`](suite.log) は20 passed・1 explicitly ignored、[`features.log`](features.log) は
Fabric/NeoForge付きcheck合格。[`review.log`](review.log) は独立レビューで指摘なし
（レビュー自体は実行検証していない）。既存の `Trample.distance` warningは残る。

source/binary hashとbuild commandはresultsに収録。receiptはログ類のhashを保持する。
README/specのみの追記は測定対象のコードを変えない。
