# 監査が引用する修正前ソース

2026年9月の wire/security 監査（[AUDIT_WIRE.md](../../../AUDIT_WIRE.md)、
[SECURITY_NOTES.md](../../../SECURITY_NOTES.md)）は、修正前の `src/main.rs` を
根拠に指摘を書いています。その原本は長いあいだリポジトリ外
（`/data/lane-scratch/mc-spine*`）にだけあり、引用がホストの残骸に依存していました。
ここに置き直したのは、引用がリポジトリ内で解決するようにするためです。

| ファイル | 何か | sha256 |
|---|---|---|
| `main.pre-cap.rs` | `read_packet` の事前チェック（cap）前のソース。当時の `src/main.rs.bak-precap` | `f5a28e8c7a01814cd653227dc17e966c24e928a7bb96d004dfdb334798c83207` |
| `main.2026-09-25T1741.rs` | 監査2巡目が見た 17:41 断面 | `cbc30aae6fd9fef15453b93d313f60b757324831ac2cfbdd6a1810bb969feef2` |
| `main.2026-09-25T1809.rs` | 同じ日の 18:09 断面（`AUDIT_WIRE.md` の「New surface since」） | `f0cdc493abbc7a3138bf9390dd2c935d533c1cdbef793a5e3ee92b3d0171e5de` |
| `main.2026-09-25T1641.rs` | 同じ日の 16:41 に配布物へ当てた版（`AUDIT_WIRE.md` が「incident-era build」として引用）。バグのある `write_varint` を実際に積んだビルドの断面 | `3c834269c9ce4cbc3bbb851b73ea2af8e14d21fa658881e7683de0cc26907343` |

これらは**ビルド入力ではありません**。どのrevisionのビルドにも使われず、
ゲートも読みません。当時の指摘が指している行を含むかどうかを確かめるための
記録です。現在の挙動や安全性の証拠ではありません。
