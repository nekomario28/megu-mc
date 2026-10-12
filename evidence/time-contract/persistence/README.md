# MET3/MET4永続化の round-trip 検証

## きっかけ

MET4の旧MET3残差について「実データがないことを理由に検証を省かない」制約があった。
実保存ワールドは無いので、**本番encoderを使った合成round-trip**で検証した。

## 発見した実際の不具合（2件）

1. **Drop/Xp age の単位がsaveとloadで食い違っていた。** saveはserver tick数を
   書くのに、loadはミリ秒として読んで `ms / (1000/TICKS_PER_SECOND)` で割っていた。
   age 3 tick が 0 tick になって戻る。round-tripテストが検出した。
   修正：loadはtick数をそのまま読む。

2. **MET3にmob airのfieldが無い。** 私は当初「MET3はairを持つ」と誤って仮定していた。
   encoderは `magic, count, etype, x, y, z, hp` までで、airは書かれない。
   loadは満タン（`server_ticks(300)`）を設定する。これは**忠実性の欠落**であり、
   vanillaはmob airを永続化する。テストは実態（満タン）を明示固定した。

さらにMET2エミュレーションのtruncate offsetを最初 3+8+3+8 と誤り、
z座標の途中で切っていた。正しくは 8+8+8。修正した。

## 何を証明したか

`met3_round_trip_preserves_drop_components_age_and_mob_air` が
chunk・forceload・world time・drop（item/count/components/age）・mob（etype/hp）の
round-tripを本番encoderで検証。
`met2_without_mob_air_falls_back_to_a_full_supply` がMET2 tailでもmobが読込まれ、
満タンの空気供給になることを検証。

観測はstructで返し、共有fixture復元後にassertする。assert中にpanicすると
WORLD_TEST_LOCKが毒されて全worldテストが連鎖失敗するため。

serial/parallelとも30 passed・1 ignored。Fabric/NeoForge checkも合格。
独立レビューは未実行（外部の利用制限）。

## 残る永続化の論点

- mob air が永続化されない（上記2）。追加するにはMET4化が必要。
- 旧MET3ファイルのdrop ageはミリ秒として書かれている。上記1の修正後、
  旧ファイルを読むとms値をtick数として解釈する。300s=300000msのファイルは
  6000 tickでちょうど寿命と同じだが、500msのような値は500 tick（125s）になる。
  **旧ファイルからの移行方針が未決定。** 実データが無いため影響は不明。
  MET4化してversionを明示するか、magicをMET4に変えて読み分けるかを決める必要がある。
