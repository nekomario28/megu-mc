# Local full-gate checkpoint — 2026-11-12

This is a record of an existing local run, not a fresh execution for the
2026-11-13 planning/documentation update.

| Item | Recorded observation |
|---|---|
| Source SHA | `561dd6581da16d97ff3af06ab8e8e331c16476c1` |
| Start → finish | 2026-11-12 16:19:30 → 17:07:27 JST (47m57s) |
| Driver | `tests/verify.sh`, mutation stability enabled |
| Rust rounds | 4 × 412 passed / 0 failed / 1 ignored |
| Mutations | 274 proved / 0 not a gate / 0 parked / 0 skipped |
| End markers | `verify: PASS`, exit 0 |
| Unit / invocation | `mc-final-gate-main.service` / `bf1da4bcfa2a4c3eb218f997264b5bf7` |
| Log SHA-256 | `5dff187dd5e5e5fba4b43655e55fccb26fdb66394f92fe10d2a17c2fec7eebc0` |

The host retains `verify3.log`, `verify3.exit`, timestamps and
`final-receipt-4.json` under
`~/mc-spine-local/audit/historical/save-booking-20261110/`.
On 2026-11-13 these files were read back: all four unfiltered 412-pass result
lines, the mutation summary, PASS marker and exit file agreed with the receipt.
The log digest above was computed from the retained bytes. Logs and native
artifacts were not uploaded by the planning update.

The source-to-`531e369ce2641d5b617f5522a8d288269b799638` diff consists only of
`specs/template-finalization/slices/04-chunks.md`; it is not a new gate run.
Subsequent behavioral edits require new results on their actual source SHA.
The backoff test at this checkpoint admits a one-sided, four-tick late drift
while still rejecting an early visit; this record does not undo or strengthen
that test's contract.

Not established here: green hosted CI, independent review, broad native parity,
all seeds/chunks, actual template entity creation, M2, or M3.
A prior Actions billing failure is historical evidence, not a current CI status check.
