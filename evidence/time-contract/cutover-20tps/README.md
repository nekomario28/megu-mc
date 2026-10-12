# 20TPS切替

## したこと

`TICK_MS` を 250 → 50 にした。`TICKS_PER_SECOND` は20、`GAME_TICKS_PER_TICK` は1になり、
slice 05–12で導出した保存値は**全て自動的にvanilla game tick値と一致**する。

追加で必要だったのは効果の保存持続と周期、移動係数。

- 効果の保存持続をvanilla game tick値に（トーテム 900/100/800、食料 2400/100/400/6000 等）
- 効果周期を `server_ticks(50/25/40) >> amp`（4TPSでは半分にしていた）
- **wire変換が消えた**。`e.left * GAME_TICKS_PER_TICK` が `* 1` なので `e.left` を直接送る。
  二重単位が解消された
- 移動係数を一次証拠のvanilla値へ。弓矢 `0.99`/`-0.05`、投擲 `0.99`/`-0.03`
  （`AbstractArrow.javap.txt:747,665`、`ThrowableProjectile.javap.txt:163,aZ()`）

TTLは**現在の実時間を保つ**側を選んだ。vanilla忠実な撤去はエンティティ蓄納の
リスクがあり結果も変わるため、別sliceとして分離する。

## 結果が変わるもの（予告済み）

- 弓矢・パール・釣針・Mobの軌道。重力-lite近似からvanilla係数へ
- 効果の周期。4TPSではvanillaの半分にして実時間を合わせていたが、
  今はvanilla game tickを直接使う（実時間は不変）
- 雷の持続（slice 08で600s→180sに修正済み）

実時間が不変なもの：炉の精錬・燃料・空気・天候の長さ・autosave・導火線・
Drop/Xp寿命・pickup遅延・環境ダメージ間隔・作物成長率・keepalive・時刻通知。

## 更新したテスト（壊れていたのではなく4TPS派生の期待値）

| テスト | 変更 |
|---|---|
| furnace_cache | schedule index を `server_ticks(vanilla_gt)` へ。反復 160→800tick |
| furnace_cache 参照 | frozen v2.37 が `progress >= 40` をハードコード。`server_ticks(200)` へ |
| furnace produced | 49/89/129 → 249/449/649（連続精錬は1クック時間=200tick間隔） |
| effect_tests | 保存持続 180/20/160 → 900/100/800 |
| tick_tests 各 | `server_ticks(gt)` の期待値を4TPS射影からvanilla game tickへ |
| TNT導火線 | 16tick目 → 80tick目 |
| pickup遅延 | 2tick → 10tick |
| 作物成長分母 | 120 → 600 |
| profiler fixture | 産出数をcadence依存の値から「動いたか」のbooleanへ |

## 同一fixtureでの前後測定

同じrunner・release設定・同一fixture、各3プロセス。1tickあたりの中央値：

| fixture | 4TPS | 20TPS | 比 |
|---|---:|---:|---:|
| 空 | 1.27µs | 1.17µs | x1.09 |
| idle Actor 1 | 10.17µs | 10.04µs | x1.01 |
| 休止炉400 | 19.21µs | 19.04µs | x1.01 |
| 稼働炉400 | 22.74µs | 20.28µs | x1.12 |
| 同chunk Actor 100 | 16.65µs | 16.51µs | x1.01 |
| 密集 Actor 100 | 183.11µs | 182.35µs | x1.00 |

全fixtureでCPU 0.2811s → 0.2766s、peak RSS 14,768–14,860KiB → 14,764–14,860KiB。

**これは1tickあたりの費用であり、1秒あたりではない。** 20TPSは5倍の頻度で回るので、
同一負荷での每秒CPUは約5倍になる。vanillaも20TPSで回るので同等条件だが、
**「軽量になった」とは言えない。** 旧4TPSとの比較は移行費用であり性能優位の証拠ではない。

serial/parallelとも30 passed・1 ignored。Fabric/NeoForge checkも合格。

## 独立レビューは未実行

`codex review` は7slice連続で外部の利用制限により不通。
この切替は大きな変更であり、**レビュー未実行のままコミットした**。
後続で必ず実行すること。

## 残る論点

- 旧MET3ファイルのdrop ageはミリ秒。1tick=50msなので `300000` は300000tick（15000s）に
  なり即座に消滅する。MET4化してversion明示が必要。実データが無く影響は不明。
- TTL（弓矢240・パール50・Mob弓矢160・釣針咬み）は実時間を保つ側を選んだ。
  vanillaは飛翔ttlを持たず、1200はinGround消滅カウンタ。
- 被ダメ後無敵500ms。vanillaは `invulnerableTime` 20と `>10` の別モデル。
- 天候の範囲。現在は下限固定。vanillaは12000..24000 / 3600..15600。
- mob air が永続化されない。
- 移動係数の水中分岐（vanilla 0.8f）が未実装。
