# 18 — 所属判定のmutexをtick1回にまとめる

## なぜ

`is_warm` / `is_hot` は呼ぶたびにmutexを取る。要素ごとに呼ぶループでは
tickあたり要素数回のlockが発生する。炉stageが支配的だった原因。

## 契約

- ループの外で1回lockし、クロージャで照合する。
- 所属テストの内容は不変（同じ`div_euclid(16)`、同じ集合）。
- 要素が空ならlock前に早期returnする。
- **lock範囲内で`is_hot`/`is_warm`を呼ばない**。同じ非再入mutexなのでデッドロック。

## ゲート

32テスト（serial/parallel）、Fabric/NeoForge check。
全fixtureで後退がないことを測定してから採用する。

## 注意

一括置換は禁止。1ブロックずつ、再入しないことを確認して進める。
`is_hot`/`is_warm`を要素ごとに呼ぶ箇所は20ヶ所残っている。

## 残り20ヶ所は不採用（測定済み）

drop・xp・作物・mobのループにも同じhoistを適用してみたが**全て後退した**。

| fixture | furnace単体 | +drop/xp/作物/mob |
|---|---:|---:|
| 空 | x1.00 | x0.96 |
| idle Actor 1 | x1.00 | x0.96 |
| 休止炉400 | x1.16 | **x0.86** |
| 稼働炉400 | x1.17 | x0.99 |

revertしてfurnace単体に戻した（x1.17で回復を確認）。

理由：このfixtureにはdrop・xp・作物・mobが**1つも無い**。
hoistすると「0回のlock」が「1回のlock」になり、要素数0では確実に損。
さらに lock を要素ループの外に出すと、範囲が長くなって
他のlockとの競合が増えている可能性がある。

**教訓：hoistは「要素が実際に多い」ことが測定で確認できてから適用する。**
fixtureにその要素が無い最適化は、fixtureを先に拡張して測ること。
作物・drop・xp・mobを含むfixtureを作るまでは着手しない。

## xpのhoistも不採用（fixture拡張後に再測定）

slice 19でfixtureに600xp・600ドロップ・600作物・600Mobを入れたあと、
xpループのhoistを測り直した。

| fixture | environment stage 比 |
|---|---:|
| entity_heavy | **x1.024** |
| entity_heavy_furnaces | **x1.024** |
| 空 | x0.994 |
| idle Actor 1 | x0.950 |
| 休止炉400 | x0.939 |
| 稼働炉400 | x0.942 |

xpが無いfixtureではlockを無駄に取るので x0.94 に後退。
利得は entity_heavy でわずか2.4%しかない。

**判断：不採用。** 2.4%の利得に後退の管理コストと
早期returnの落とし穴（`keepalive_time_tick` の後続処理を飛ばす）を
持ち込む価値がない。

もし採用するなら `return` ではなく `if !xl.is_empty() { ... }` で囲むこと。
`return` は天候・雷・サイズログを巻き添えにする。

## 作物のhoistも不採用（ノイズ内）

fixture 19で600作物を入れたあと測った。
entity_heavy x0.994、dense_actors x0.985。
しかし**revert後の再測定でも x0.974–1.002 と動く**ので、
run-to-run ノイズが2–3%ある。作物hoistの差はノイズ内で判別できない。

**不採用。** 2–3%未満の最適化はこの測定系では判別できない。
次の候補は「2–3%より大きい効果」が見込めるものに限る。

## 測定系の限界

同一コードを測り直しても fixture によって 2–3% 動く。
したがって x1.05 未満の主張はできない。
採用済み3件は x1.16・x1.60・x3.20 と十分ノイズを超えている。
