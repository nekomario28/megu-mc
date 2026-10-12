# 12 — MET3/MET4永続化の round-trip

## なぜ

旧MET3残差の帰結を「実データがない」ことを理由に省かない。
実保存ワールドは無いので、本番encoderを使った合成round-tripで検証する。

## 契約

- round-tripは `save_world_into` / `load_world_from` を使う。手書きbytesを使わない。
  手書きだとchunk index符号化を誤る。
- 観測はstructで返し、共有fixture復元後にassertする。毒lockで連鎖失敗しない。
- MET2はair fieldを持たない。満タンの空気供給になる。
- MET3もair fieldを持たない（encoderが書いていない）。これは忠実性の欠落。

## ゲート

chunk・forceload・world time・drop（item/count/components/age）・mob（etype/hp）の
round-tripを検証。MET2 tailでもmobが読込まれることを検証。
serial/parallel全テスト、Fabric/NeoForge checkを通す。

## 発見した不具合

1. drop/xp age の単位がsave/loadで食い違っていた（tick vs ms）。
2. mob air が永続化されない。
3. MET2エミュレーションのtruncate offsetを誤っていた。

## 未決定

旧MET3ファイルのdrop ageはミリ秒として書かれている。修正後のloadは
tick数として解釈する。旧ファイルの移行方針が未決定。
MET4化してversionを明示し読み分けるか、magicを変えるかを決める必要がある。
実データが無いため影響は不明。
