# `read_comps` の境界確認（レビュー [P2] の修正）

## 指摘

issue #7：
> read_comps checks the remaining 2-byte length field *before* consuming a
> variable-width type VarInt, and then indexes b[*p..*p+2] without rechecking.

そのとおりだった。

    if *p + 2 > b.len() { break }          // VarInt の前
    let ty = rv_varint(b, p);              // 1..5 バイト消費する
    let l = u16::from_be_bytes(b[*p..*p + 2]...).unwrap();   // 再確認なし → panic

`rv_varint` は可変長なので、消費後の位置は事前チェックでは保証できない。

## 修正

VarInt 消費の**後**に長さフィールドを確認する。長さ本体も
`saturating_sub` で比較し、加算のオーバーフローも避ける。

## ゲート

`truncated_component_blobs_do_not_panic_the_loader`：
- 正常なcomponentレコードの**全truncation点**で panic せず、cursorがbufferを越えない
- 残量より大きい長さを主張するレコードは**棄却される**（空を返す）
- 長さフィールド自体が切れた場合も同様

負の対照：修正前の形に戻すと `src/main.rs:3884` で panic。

42テスト pass（serial／8thread）、wire smoke pass、Java adapter × 2 pass。
