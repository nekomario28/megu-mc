# 忠実なゲーム時間と、結果不変の処理省略

Status: IMPLEMENTING — 全体の20TPS化・vanilla互換・軽量化は未達。

## Next Agent Prompt

目的は「超軽量だがゲームの結果は変えない」。前提の参照値は
[時間契約の一次証拠](../../../evidence/time-contract/README.md)、既存の影響表は
[時間契約の設計](../../../TIME-CONTRACT-DESIGN.md)を読む。
ユーザーは「忠実な20TPS＋必要な対象だけ実行」という推奨方針で進めることを承認した。
意図的なゲーム結果の近似は採用しない。小差の具体的な許可リストはまだない。

[トーテム効果集合](slices/01-totem-grant.md) の付与関数修正と回帰テストは完了。
[実際の発動・通知・吸収](slices/02-totem-activation.md) のRust/TCPゲートも通った。
[本番tick計測](slices/03-tick-baseline.md) を追加し、[切替前の限定fixture測定](../../evidence/time-contract/tick-baseline/README.md) を実行した。
[hot/warm集合構築](slices/04-region-expansion.md) の等価変形も適用・検証した。
[時間語の単一所有者](slices/05-cadence-owner.md) で `TICK_MS` から全境界を導出した。
[実時間境界](slices/06-wall-clock.md) のkeepalive/通知と、[ゲーム時間周期](slices/07-game-tick-periods.md)
のダメージ間隔、[保存tick語の値](slices/08-stored-durations.md) の炉/燃料/空気/天候、
[1tickあたりの割合](slices/09-per-tick-rates.md) の作物成長と周期スケジュール、
[導火線](slices/10-tnt-fuse.md)、[Drop/Xp寿命とpickup遅延](slices/11-entity-life.md)
のgame tick化、[永続化のround-trip検証](slices/12-persistence.md) も
同じ所有者から導出した。
[20TPS切替](slices/13-cutover-20tps.md) は**実行計画のみ**。実装はしていない。
雷の持続も雨と別の値に直した。溶岩ダメージの変換漏れも発見・修正した。
[限定fixtureのA/Bと限界](../../evidence/time-contract/region-expansion/README.md) を読む。
切替の前提は揃った。次は `TICK_MS = 50` の1コミット。
移動係数・TTLをvanillaへ置き換えるため**結果が変わる**。予告済み。
**移動係数の扱いはユーザーの判断が必要。** 現在は「gravity-lite」近似で、
vanillaのper-game-tick係数と実時間あたり一致しない。近似を実時間で保存するか、
vanilla係数に置き換えるかで結果が変わる。弓矢/パール/Mob弓矢のttlと
bobberの咬みもvanillaと大きく違う。この判断が出るまで20TPSは有効化しない。
独立レビューで共有fixtureの並行実行競合も修正した。世界fixtureを追加するテストは
setupからrestoreまでWORLD_TEST_LOCKを保持すること。
移動/衝突、保存の単位、遅延時の方針を未決定のまま時計を変えてはいけない。
終了時にこのpickupと検証結果を更新すること。

- [x] トーテム効果のID・持続・既存効果除去を修正し、実際の付与関数で回帰検証。
- [x] トーテムの通常被ダメ後/効果中の発動・TCP効果通知・吸収量の消費/期限切れをRust経路で検証。
- [ ] 実クライアント/vanilla実サーバーでのトーテム比較（TCP検証は描画や全戦闘経路の証明ではない）。
- [ ] ゲームtick語・実時間語を分離し、全ての生成/消費/保存/パケット境界を監査。
- [ ] 20TPSへ一括移行（時刻・効果・精錬・機械・移動・導火線等の整合が先）。
- [ ] 被ダメ・飛翔・天候・釣り等の状態条件を一次参照に合わせ、発生tickを比較。
- [x] 切替前の本番tick経路を限定fixtureで測定（4TPS時間語・sleepなし。全サーバー性能ではない）。
- [x] hot/warm集合を変えず重複挿入を削減。全負荷での速度改善は未証明。
- [x] tick周期を `TICK_MS` の単一所有者にし、世界時刻・効果wire・秒変換を導出化。周期は不変。
- [x] keepalive等の実時間境界を `every_ms` へ接続。keepaliveはvanillaの15000msに修正。
- [x] 環境ダメージ/回復の周期を `server_tick_period` から導出。値と実時間間隔は不変。
- [ ] 同条件20TPSでCPU/RSS/tick遅延を測り、費用の大きい箇所だけ最適化。

後続項目はまだ実装可能なsliceに確定していない。着手前に状態の所有者・入力・
動的ゲートを絞り、sliceとして具体化する。全体の終了をこの最初のパスと混同しない。

## 現在の検証

トーテムの4テストはそれぞれ対応する修正前に失敗。現在の全テストはserial/parallelとも30 passed・1 ignored。clockはまだ250ms。
ignoredは専用runnerで前後各3プロセス実行したrelease計測用fixture。
tickの炉stage欠落・ringの半径誤り・世界時刻の加算・効果wireの周期導出は
それぞれ負の対照で検出した。keepaliveのtick除算とダメージ周期・保存値の換算方向の誤りも検出した。共有fixture競合も
再現して修正し、対象テストの8thread実行を20回反復した。
通常発動の吸収0、救命後の旧空腹効果の実行、期限切れ後に残る吸収をそれぞれ再現した。
Fabric/NeoForge機能付きcargo checkも合格。**保存値と1tickあたり割合のsliceは独立レビューが
未実行**（`codex review` が外部の利用制限で中断）。それ以外のパスは指摘なし
（レビュー自体はテストを実行していない）。ログは
[実装の証拠](../../evidence/time-contract/)に保存。
clockは4TPSのまま。vanilla動的同等性・20TPS・全サーバーCPU/RSS・実クライアントは未検証。

## 守る契約

- ゲーム時間・乱数・状態はRustに一つだけ置く。Java側に別の世界を進めない。
- 計算の省略は結果と発生tickが不変と証明した対象だけ。定数の時間換算を状態再現と呼ばない。
- scheduled tickの遅延値を、全セルの永久巡回周期に読み替えない。
- 指数バックオフは全面撤去しない。意味のある処理を遅らせる用途は外し、
  条件が変わるまで結果不変と証明できる休止は残す。起床条件漏れは不合格。
- 互換性の基準と軽量性の基準は同じ20TPS/機能/seed/距離/負荷。
  旧4TPSとの比較は移行費用であって性能優位の証拠ではない。
- 既存のMET4方針は維持。ただし永続化の単位と旧形式の帰結をテストし、
  実データがないことを理由に検証を省かない。
- ネットワーク演出も勝手に間引かない。意図的な差には個別の合意と検証が必要。

## 一つの所有者

付与・tick更新・パケットは既存のEff経路を修正する。新しい平行の効果状態を作らない。
プレイヤー単位の効果更新は`effects::tick_effects`が所有し、主ループとテストが同じ関数を使う。
時間移行のためだけの二重モードや汎用schedulerは増設しない。
初期のトーテム修正では現行4TPSの保存語を維持し、その限界をsliceに明記する。

## 検証境界

純粋な付与集合テストは、効果が実際に回復/吸収として消費された証明ではない。
静的javap参照は動的なtick比較ではない。CPU測定前に「超軽量になった」と主張しない。
実装は小さいパスで前進させるが、切り替えが不可分な20TPS移行は分割して有効化しない。
