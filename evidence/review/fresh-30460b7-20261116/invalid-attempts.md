# Invalid attempts while repairing the receipt producer (kept, not deleted)

同じ条件の再試行を避けるため、producer 自身の失敗を残す。いずれも**測定値ではない**。

1. `2026-11-16T01:14Z` — `FAIL: no unique Cargo executable: Expecting value: line 1 column 4 (char 3)`。
   Cargo の JSON (stdout) と `Compiling…` (stderr) を1本にまとめて解析していた。以後 stdout と
   stderr を分離。build 自体は成功しており、この run の test 数は使わない。
2. `01:15Z` — `FAIL: no unique Cargo executable: expected one mc-spine build executable, received 0`。
   up-to-date の unit には `compiler-artifact` が出ないため binary を解決できなかった。anchor を
   touch して1回 relink する方式に変更（bytes は変えない）。
3. `01:15Z` — `FAIL: filter … ran no test: test result: ok. 1 passed; … 0 measured; 532 filtered out`。
   Rust の summary は常に `0 measured`（bench 数）を含むため、それを「0 tests」と誤判定していた。
   `ok. N passed; 0 failed` (N>=1) の正規表現に変更。
4. 補足: shell の `… | tail` で producer の exit を `$?` で読むと 0 になる（`tail` の status）。
   `verify.sh` が過去に踏んだのと同じ罠で、以後 producer の exit は直接読む。

これらの shape は現在 `tests/evidence_receipt_test.py` が固定しており、同じ欠陥が戻れば
`status=PASS` ではなく FAIL になる。
