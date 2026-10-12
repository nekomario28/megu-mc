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
