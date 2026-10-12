# saveを観測純粋にする（レビュー [P1] の修正）

## 指摘

issue #7：
> save_world_into serializes drops first, **then removes all tridents** from live
> arrows and creates in-memory drops. Autosave therefore mutates the running
> world; the newly generated drops are not included in that save.

**診断で確認した**（修正前）：

    DIAG live arrows [7001, 7002] -> [7002]     ← trident が消える
    DIAG live drops 0 -> 1                       ← live に drop が増える
    DIAG loaded drops=[]                         ← payload に drop が無い

つまり **resting trident は save から失われ、live 状態だけが変わる**。
データ損失であり、指摘より重い。

## 修正

変換をpayloadへ**書き込む**だけにし、live世界に触れない。

- trident を収集する（`kind == 1 && ret_item > 0`）
- drop数を `live + tridents` と書く
- live drop と同じ書式で trident record を追記（eid 0 は load 時に再割当、age 0 で pickup 遅延満タン）
- ages も同順で追記
- 矢の書き出しから trident を**除外**する（drop 側にあるので二重保存を防ぐ）

## 何を証明したか

`saving_neither_mutates_live_state_nor_loses_resting_tridents`：
- live arrows と live drops が save 前後で**不変**
- **2回目の save が同一バイト列**（saveが冪等。世界を消費しない）
- load 後 trident が **drop として1回だけ**復元（`[(811, 1)]`）
- 空中の矢だけが矢のまま（`kind = [0]`）

負の対照：矢の除外をやめると `left: [0, 0]`（trident が矢としても保存され二重化）。

## 実装中の失敗

最初の試作は**2回失敗した**。
1. payload符号化でロードが `(0,0)` になった（原因を切り分けずrevert）
2. 外側のbraceを巻き込んで削除し `unexpected closing delimiter`

今回は「trident変換を削除 → 収集に置換 → dropループに追記 → 矢の除外」を
**1箇所ずつ**適用し、各段階で診断を回して確認した。

43テスト pass（serial／8thread）、wire smoke pass、Java adapter × 2 pass。
