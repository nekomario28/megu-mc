# tick周期の単一所有者

## したこと

`TICK_MS` を唯一の周期所有者にした。`TICKS_PER_SECOND = 1000 / TICK_MS` と
`GAME_TICKS_PER_TICK = 20 / TICKS_PER_SECOND` を導出し、世界時刻の加算・効果の
wire変換・秒→tick変換・導火線の一時停止・主loopのsleepがこれを使う。
散在していた `250`・`* 5`・`secs * 4` のリテラルを置き換えた。

**周期は250msのまま。** 世界時刻は実時間で毎秒20進む。効果の保存単位はserver tick、
wireはgame tickという二重単位は残る。数値・順序・発生tickは変えていない。

## 何を証明したか

- `cadence_owner_keeps_world_clock_at_vanilla_rate` が不変条件と1tick分の進行を検証。
- 世界時刻の加算をリテラル `4` へ戻すと失敗（[clock-negative](time-contract-cadence-clock-negative.log)）。
- 効果wireの周期導出を外すとTCP効果テストが900/100/800ではなく720/80/640で失敗（[wire-negative](time-contract-cadence-wire-negative.log)）。
- 独立レビューは空腹の消耗係数を `TICKS_PER_SECOND` で書いて0.025→0.020に変えていた
  誤りを指摘した。`GAME_TICKS_PER_TICK` へ修正し、修正後に再レビューで指摘なし。
  レビューはテストを実行していない。計測再実行で平均tick時間は最大2%以内の変動。
  CPU合計0.2862s→0.2827s。これは回帰検出の範囲で、性能主張の証拠ではない。

serial/parallelとも22 passed・1 ignored。対象テストの8thread実行を20回反復。
Fabric/NeoForge checkも合格。[suite](time-contract-cadence-suite.log)、
[parallel](time-contract-cadence-parallel.log)、[features](time-contract-cadence-features.log)。
独立レビューの生ログは公開準備で削除した（[REDACTION.md](../REDACTION.md)）。結論は `receipt.json` の `review` に残す。

## 未接続の境界

[slice 05](../../../specs/time-contract/slices/05-cadence-owner.md) に列挙した。
keepalive/timeout/天候/酸素/投擲TTL/釣り/移動係数/MET4保存はまだ同じ所有者に
接続していない。これらを接続するまで20TPSへ切り替えてはならない。
実時間境界（keepalive 15000ms等）はvanillaの単位がgame tickではなく実時間である点に注意。
