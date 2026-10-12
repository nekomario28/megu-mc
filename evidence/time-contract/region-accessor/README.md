# Region accessor（slice 01 の実装）

## したこと

`is_hot`/`is_warm` が呼ぶたびにmutexを取っていたのを、**1つの `Region` が所有**する形にした。

- `Region { hot, warm }` を1つのmutexが持つ
- ループは `region_guard()` で**1回だけ**借りる
- 集合の中身・`div_euclid(16)` の意味は不変
- 空の集合では借りない（`(!cells.is_empty()).then(region_guard)`）。
  空ならループに入らないので、借りるだけ無駄。これは最初の測定で
  後退として出た

## 測定（バックトゥバック、各3プロセス）

| fixture | before | after | 比 |
|---|---:|---:|---:|
| entity_heavy | 379,739µs | 330,674µs | **x1.148** |
| entity_heavy_furnaces | 414,915µs | 350,574µs | **x1.184** |
| 空 | 1,273µs | 1,209µs | x1.053 |
| idle Actor 1 | 10,244µs | 10,401µs | x0.985 |
| 休止炉400 | 13,688µs | 13,859µs | x0.988 |
| 稼働炉400 | 17,392µs | 17,202µs | x1.011 |
| 同chunk Actor 100 | 16,779µs | 16,656µs | x1.007 |
| 密集 Actor 100 | 185,073µs | 183,571µs | x1.008 |
| drop_pile | 10,873µs | 10,409µs | x1.045 |

CPU合計 1.0923–1.2732s -> 1.0186–1.0344s。

**効いたのは drop/xp/作物が600個あるfixture**（`is_hot` が項目ごとに
呼ばれていた箇所）。他はノイズ内。

## 測定方法の失敗（重要）

最初の測定は **x0.60〜0.79 と全fixtureで後退**した。マシンが別の負荷を
抱えていた時間帯だった。**同一コードの再測定でも壁時計で最大8.5%、
CPU時間で19%変動した。**

教訓：**before/afterは連続して測る。** 数分離すと環境の負荷変動を
変更の効果と誤読する。測定値だけでなく `cpu_user_seconds` も見て、
同じ仕事量でCPU時間が増えていれば実装が重くなったと判断する。

## ゲート

52テスト（serial／8thread）、wire smoke、Java adapter×2。
`region_membership_matches_hot_plus_skirt_across_input_changes` を維持。

## 残り

per-itemで `is_hot`/`is_warm` を呼ぶ箇所は**18残っている**
（飛翔体4経路・オブザーバー・Mob・plugin cell・interest・作物の確率判定）。
今回変換したのは炉（既にslice 18）・水・溶岩・ホッパー・作物・drop・xp。
残りは同じ形なので順次移す。
