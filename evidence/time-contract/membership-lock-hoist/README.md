# 所属判定のmutexをtick1回にまとめる

## 何が高かったか

`is_warm(x, z)` / `is_hot(x, z)` は**呼ぶたびにmutexを取る**。
炉・ドロップ・xp・作物などのループは要素ごとに呼ぶので、
tickあたり要素数回のlock/unlockが発生していた。

測定では休止炉400個で炉stageが 5758µs。1炉あたり約14µs。
中身は数回の比較とハッシュ照合だけなので、lockが原因だと推断した。

## 変更

`furnace_tick` は `warm()` をtick1回だけlockし、クロージャで照合する。
所属テストの内容は不変（同じ`div_euclid(16)`と同じ集合）。

空のときはlockを取る前に早期returnする。これを入れないと
炉が無いfixtureでstageが 27µs -> 36µs に悪化した（x0.78）。
**後退を測定で見つけて修正した**例。

## 測定（HEADを基準・同一fixture・各3プロセス）

| fixture | tick 比 | furnaces stage 比 |
|---|---:|---:|
| 空 | x0.99 | x1.02 |
| idle Actor 1 | x0.99 | x1.01 |
| 休止炉400 | **x1.16** | **x1.60** |
| 稼働炉400 | **x1.17** | **x1.43** |
| 同chunk Actor 100 | x1.00 | x1.03 |
| 密集 Actor 100 | x1.00 | x1.01 |

プロセスCPU 0.2722–0.2733s -> 0.2660–0.2696s。

## 残り20ヶ所は不採用（測定済み）

drop・xp・作物・mobのループにも同じhoistを適用したが、全て後退した。
空 x0.96、idle Actor x0.96、**休止炉400 x0.86**。revertしてfurnace単体に戻し x1.17 を確認。

このfixtureにはdrop・xp・作物・mobが1つも無いため、hoistすると
「0回のlock」が「1回のlock」になる。要素数0では確実に損。

**hoistは要素が実際に多いことを測定で確認してから適用する。**
作物・drop・xp・mobを含むfixtureを作るまでは着手しない。

## xpのhoistも不採用（fixture拡張後に再測定）

fixture 19で600xp等を入れたあと測り直した。
entity_heavy では environment stage が x1.024 と改善したが、
xpが無いfixtureは x0.94 に後退。利得2.4%は管理コストに合わない。

`return` で早期脱出すると `keepalive_time_tick` の天候・雷・サイズログを
巻き添えにする。採用するなら `if !xl.is_empty() { ... }` で囲むこと。

## 作物のhoistも不採用（ノイズ内）

entity_heavy x0.994、dense_actors x0.985。
しかしrevert後の再測定も x0.974–1.002 と動く。
run-to-run ノイズが2–3%あり、差は判別できない。

**測定系の限界：x1.05未満の主張はできない。**
採用済み3件は x1.16・x1.60・x3.20 で十分ノイズを超えている。

## 同じ失敗を避けた経過

最初に作物・bobber・パール・xp・dropのループも一括でhoistしたが、
**同じmutexを保持したまま`is_hot`を呼ぶ形になりデッドロックの危険**があった。
即座に戻し、`furnace_tick`だけを確定させて検証した。

残るループ（`is_hot`/`is_warm`を要素ごとに呼ぶ箇所は20ヶ所）は
同じ最適化が適用できる。ただし1ブロックずつ、lock範囲内で
再入しないことを確認して進める。一括置換はしない。

## 境界

このfixtureのみ。実サーバー全体やvanilla比較の証明ではない。
独立レビューは未実行。
