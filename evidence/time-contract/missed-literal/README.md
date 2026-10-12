# 変換漏れていたliteral

## 発見

溶岩ダメージだけ `if in_lava && t % 2 == 0` のままだった。他の環境ダメージは
slice 07で `server_ticks(10)` へ変換済み。4TPSでは `server_ticks(10) == 2` なので
**完全に等価**で、全テストが通っていた。このため行動ゲートでは検出できなかった。

`git log -S` で確認すると、この行は導入時（`f9924b0`）から `t % 2` のままで、
slice 07の変換も実際には適用されていなかった。報告した「applied 12」は
他の11件が真で、この1件は当初から対象外だった。

これはslice 09がreceiptに書いた「呼び出し側のliteral revertは4TPSで
行動ゲートで検出できない」という限界の**実例**であり、仮定の警告ではなく
実際に起きた。検索で0件にした。

## 修正後の検索

`rg -n 't % [0-9]+ == 0|TICK\.load\(Ordering::Relaxed\) % [0-9]+|% \(if fast' src/main.rs`
の結果は0行（`plate_powered` のstate bit判定を除く）。

serial/parallelとも26 passed・1 ignored、Fabric/NeoForge checkも合格。
