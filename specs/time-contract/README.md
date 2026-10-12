# 忠実なゲーム時間と、結果不変の処理省略

Status: 20TPS化 完了・CI green・独立レビュー1回受領（REQUEST CHANGES、P1は対応済み）。
未達: vanilla動的同等性・実クライアント検証・負荷計測・統合headの再レビュー。

## Next Agent Prompt

目的は「超軽量だがゲームの結果は変えない」。一次参照は
[時間契約の一次証拠](../../../evidence/time-contract/README.md)、影響表は
[時間契約の設計](../../../TIME-CONTRACT-DESIGN.md)。
ユーザーは「忠実な20TPS＋必要な対象だけ実行」を承認し、移動係数はvanilla値（案B）を選んだ。

branchは `fix/20tps-integrated`。20TPS時間契約とPR #2のhardeningを統合したheadで、
これが正系。`fix/20tps-time-contract` は時間契約のみ、`fix/audit-1-on-20tps` は削除済み。

**まず [矛盾の棚卸し](slices/26-contradictions.md) を読む。** 計画文書が実装から
どれだけ乖離していたかと、その是正を記録してある。

[トーテム効果集合](slices/01-totem-grant.md) の付与関数修正と回帰テストは完了。
[実際の発動・通知・吸収](slices/02-totem-activation.md) のRust/TCPゲートも通った。
[本番tick計測](slices/03-tick-baseline.md)、[hot/warm集合構築](slices/04-region-expansion.md)、
[時間語の単一所有者](slices/05-cadence-owner.md)、[実時間境界](slices/06-wall-clock.md)、
[ゲーム時間周期](slices/07-game-tick-periods.md)、[保存tick語の値](slices/08-stored-durations.md)、
[1tickあたりの割合](slices/09-per-tick-rates.md)、[導火線](slices/10-tnt-fuse.md)、
[Drop/Xp寿命とpickup遅延](slices/11-entity-life.md)、[永続化のround-trip検証](slices/12-persistence.md)、
[20TPS切替](slices/13-cutover-20tps.md)、[MET4マーカー](slices/14-met4-marker.md)、
[飛翔体TTLの実時間復元](slices/22-projectile-ttl.md)、[tick周期と確率の網羅監査](slices/23-tick-periods-audit.md)、
[tick pacing のdrift補正](slices/24-tick-pacing.md) を適用した。

軽量化は [炉の空input skip](slices/16-furnace-idle-skip.md)、
[所属判定lockの集約](slices/18-membership-lock-hoist.md)、
[dropマージのセルバケット化](slices/20-drop-merge-bucket.md) を採用。
[region構築案](slices/15-region-build-rejected.md) は密集で後退したため不採用。

[被ダメ無敵](slices/25-invulnerability.md) は仕様のみで未実装。

- [x] トーテム効果のID・持続・既存効果除去を修正し、実際の付与関数で回帰検証。
- [x] トーテムの通常被ダメ後/効果中の発動・TCP効果通知・吸収量の消費/期限切れをRust経路で検証。
- [ ] 実クライアント/vanilla実サーバーでのトーテム比較（TCP検証は描画や全戦闘経路の証明ではない）。
- [x] ゲームtick語・実時間語を分離し、生成/消費/保存/パケット境界を監査（slice 05–12, 23）。
- [x] 20TPSへ一括移行（`bf2c595`。時刻・効果・精錬・機械・移動・導火線を同時に）。
- [ ] 被ダメ・飛翔・天候・釣り等の状態条件を一次参照に合わせ、発生tickを比較。
      [被ダメ無敵](slices/25-invulnerability.md) は未実装。
- [x] 切替前の本番tick経路を限定fixtureで測定。
- [x] tick周期を `TICK_MS` の単一所有者にした。周期は50ms。
- [x] keepalive等の実時間境界を `every_ms` へ接続。keepaliveはvanillaの15000ms。
- [x] 環境ダメージ/回復の周期を `server_ticks` から導出。
- [x] 同条件20TPSでfixtureのCPU/RSSを測り、費用の大きい箇所を最適化（slice 16/18/20）。
- [ ] 実クライアント・vanilla動的比較・代表負荷での計測。
- [ ] 統合headの独立再レビュー。

## 現在の検証

全テストはserial/parallelとも52 passed・1 ignored。clockは50ms。CI green
（locked build・serialized tests・localhost wire smoke・Java adapter × 2）。
ignoredは専用runnerで実行するrelease計測用fixture。

独立レビューは [issue #7](https://github.com/nekomario28/mc-spine/issues/7) で1回受領し
REQUEST CHANGES。P1は全て対応し、各修正に負の対照を付けた
（spawn回転・負のdrop age・旧counter単位・save副作用・飛翔体drag）。
P0は統合headで解消（[統合](slices/26-contradictions.md)）。
[P2] `read_comps` も修正。残るは統合headの再レビュー。

**自己レビューは代替にならない。** 統合の衝突解決10箇所で私が作った欠陥を2件、
自己レビューで見つけた。計画と実装の乖離は [slice 26](slices/26-contradictions.md) を参照。

Fabric/NeoForge機能付きcargo checkも合格。ログは
[実装の証拠](../../evidence/time-contract/)に保存。
vanilla動的同等性・全サーバーCPU/RSS・実クライアントは未検証。
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
トーテム修正を先に行った理由は、切替が不可分で他を巻き込むためだった。

## 検証境界

純粋な付与集合テストは、効果が実際に回復/吸収として消費された証明ではない。
静的javap参照は動的なtick比較ではない。CPU測定前に「超軽量になった」と主張しない。
実装は小さいパスで前進させるが、切り替えが不可分な20TPS移行は分割して有効化しない。
