# 05 — 時間語の単一所有者と全境界監査

## なぜ

20TPS切替の前提は、tick周期が一箇所にあり、そこから全境界が導出されること。
散在する `250` / `* 5` / `secs * 4` は、切替時に一部だけ変わる事故を生む。

## 契約

- `TICK_MS` が唯一の周期所有者。`TICKS_PER_SECOND = 1000 / TICK_MS`。
- 世界時刻は実時間で毎秒20 game tick進む。`GAME_TICKS_PER_TICK = 20 / TICKS_PER_SECOND`。
- 効果の保存単位はserver tick、wireはgame tick。変換はこの定数からのみ行う。
- `/effect` の秒→保存値変換も同じ定数から導出する。
- TNTの導火線一時停止も `TICK_MS` を使う。周期と無関係に見える実時間値は変更しない。

このsliceは周期を変えない。値・順序・発生tickは不変。

## ゲート

- `cadence_owner_keeps_world_clock_at_vanilla_rate` が不変条件と1tick分の進行を検証。
- 世界時刻の加算をリテラルへ戻すと失敗する（負の対照）。
- 効果wireの変換を外すと既存のTCP効果テストが失敗する（負の対照）。
- 全テスト、Fabric/NeoForge check、独立レビューを通す。

## 未接続の境界（切替時に同じ所有者から導出する）

空腹の消耗係数もこの所有者から導出する。game tickあたり0.005なので
`GAME_TICKS_PER_TICK` を掛ける。`TICKS_PER_SECOND` を掛けると周期変更時に崩れる。

以下はこのsliceでは触らない。周期変更と同時に、それぞれの正しい変換を導出する。

- 実時間境界（keepalive 40 tick=10s、RSSログ 20 tick、時間通知 4 tick）。
  vanilla keepaliveは15000ms実時間。20TPSではこれらは別の周期へ再導出が必要。
- 効果の持続保存語。MET4の旧MET3残差は自動移行しない（既存方針）。
- M suffocation/air 60、投擲TTL、天候カウンタ、釣り待機、`/weather`の秒→tick変換。
- 移動・重力・摩擦など、1tickあたりの係数として書かれている量。
- 数値の等価変形ではない項目はvanilla一次参照で別途検証する。

## ゲートの実行境界

負の対照はrelease測定に使うrunnerとは別。対象テストの並行実行は専用バイナリで20回反復。
測定のA/Bは回帰検出の範囲であり、性能主張には使わない。
