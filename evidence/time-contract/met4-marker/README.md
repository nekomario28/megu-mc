# MET4マーカー — 切替で私が劣化させたものの修正

## 何が起きていたか

20TPS切替（`897fcac`）の直前に、Drop/Xp age の単位をmsからtickへ直した。
そのとき**旧MET3ファイルのageはミリ秒で書かれている**ことを受け入れず、
tickとして読むようにした。切替で1tick=50msになったため：

    旧ファイルの 300000 ms → 300000 tick
    寿命は server_ticks(6000) = 6000 tick
    → 最初のtickで全ドロップが消滅する

これは切替が原因で**私が作った後退**である。受入条件の「保存状態を壊さない」に反する。

## 修正

magicを `MET3` → `MET4` に上げ、ageの単位をmagicで読み分ける。

- `MET4`: age は server tick（そのまま）
- `MET2` / `MET3`: age はミリ秒（`raw / TICK_MS` でtickへ変換）

MET2は従来通りair fieldを持たないため、mobは満タンの空気供給で読込まれる。

## 何を証明したか

3つのround-tripテストが本番encoderで検証する。

- `met4_round_trip_...` — chunk・forceload・world time・drop（item/count/components/age）・
  mob（etype/hp）の保存と復元
- `legacy_met3_millisecond_ages_convert_to_ticks_...` — 旧MET3の3000msが
  50ms/tickで60tickになり、寿命6000tickを超えずに生き残る
- `met2_without_mob_air_falls_back_to_a_full_supply`

負の対照3種すべて検出（[negative](met4-negative.log)）。

## 実装中の誤り

期待値を `150` と書いた。3000ms / 20 = 150 は4TPSの計算で、
50ms/tickでは60が正しい。テストが検出した。
式も最初 `raw / (1000 / TICKS_PER_SECOND)` = `raw / 50` と書いていて
TICK_MSと同じだが回りくどかった。`raw / TICK_MS` に直した。

serial/parallelとも31 passed・1 ignored。Fabric/NeoForge checkも合格。
独立レビューは未実行（外部の利用制限、8slice連続）。
