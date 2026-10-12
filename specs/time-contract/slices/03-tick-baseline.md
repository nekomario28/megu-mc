# 03 — 切替前の本番tick計測

## 契約

本番tickの順序と呼出し条件を一つのgame_tickで所有し、mainのthreadと計測が
同じ関数を呼ぶ。既存の250ms周期・時間語・処理順・lavaの6回に1回という条件は維持。
新しいschedulerや設定、通常tickの時間計測は追加しない。
実行callbackは本番では即実行、計測では同じstageを時間計測して実行する。

## ゲート

温かい炉が同じtickで進行し、coldの炉は変わらず、両時計が1更新ずつ進むことを
production game_tick経路で検証する。炉stageを欠落させる負の対照で失敗を確認する。
既存全テストを維持。mainの元のstage順序・lava条件をdiffで確認する。

## 計測

明示実行するignoredテストで、空世界、idle Actorによるhot/warm ring、休止炉、
稼働炉を測る。ウォームアップ後のtick wall time分位とstage総時間/呼出し回数を出力。
最適化ビルドを使い、別々のプロセスで繰り返す。fixtureのCPU/RSSは外側で取得する。

ignoredテストは専用の一時working directoryから実行する。通常cargo testに混ぜない。
本番のautosaveも実行するため、WORLD_FILEをユーザーのworking directoryへ書かせない。
既存世界を読込まない、listenerを開かない、終わったら一時dirとプロセスを残さない。
全stageを測るが、fixtureにはプレイヤー・Mob・液体・Java Mod・worldgenを含めない。
Actorはhot/warm更新の負荷を作るだけで、プレイヤーの代理とは呼ばない。
この数値をサーバー全体の性能や20TPS後の結果として主張しない。

## 委譲と次の判断

fixture規模・反復回数・出力形式は実装者が選んで記録する。恒常的な新依存を増やさない。
費用の順位から次の小さな最適化を選ぶ。動作契約が未確定なまま20TPSを有効化しない。
