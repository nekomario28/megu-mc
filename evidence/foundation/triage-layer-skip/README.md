# Exact-match layer fast path

## Contract

`tools/chunk_triage.py::_compare_chunk` bulk-compares each 16×16 horizontal
layer before entering its Python per-cell loop. It skips that loop only when
A and port are exactly equal and, when B exists, A and B are exactly equal too.
Air kinds and all state properties remain distinct; no normalization is added.
A B-only difference must still enter the original loop and update native A/B
variation counts. Non-matching layers retain the original iteration/sample order.

This is less Python-loop work, not fewer logical state comparisons: slice copying
and list equality still inspect the cells. Temporary layer slices are bounded;
there is no persistent cache. Array lengths are checked before the fast path so
empty/short slices cannot turn missing data into an apparent match. Heightmap
and classification-top computation remain outside the skipped loop.

## Measured comparison — 2026-11-13

The before module came from HEAD `033d324ae0114b7f56af84fa147ac58f13ed83b5`.
Source SHA-256 before:
`92eb424234101fdd1db5d26539ac1e9ffc7e95ff9a3dd9ccaee9cb0a05de5e71`;
after: `d2529cc47b731d062095d0365f45d37d3394c051fbcb4ed803c1526f75dca0e8`.

Inputs were prepared once from retained seed-4242 A/B worlds, chunk `(3,-2)`,
and the retained binary dump. Both arms used the same loaded objects, current
region reader, arguments, structure pieces and comparison bounds. Three timed
runs per arm were interleaved. No build or native server was launched.

| Prepared comparison case | Before median | After median | Python port single-cell reads, before → after |
|---|---:|---:|---:|
| Captured port, 101 exact-state differences | 7.522 ms | 4.542 ms | 98,304 → 5,376 |
| Synthetic port=A, retaining 23 native A/B differences | 7.164 ms | 3.815 ms | 98,304 → 1,280 |
| Synthetic dense port differences, all 98,304 cells | 159.448 ms | 159.751 ms | 98,304 → 98,304 |

Single-cell read counts were collected in separate, untimed instrumented runs.
The after arm additionally performs 384 port layer slice reads per call. Dense
inputs gain no loop skipping and pay for those slices; timings overlapped
(before 159.177–160.874 ms, after 158.677–160.113 ms), so no dense speedup is claimed.

Every before/after text report and sorted JSON summary was byte-identical in all
three cases, including the instrumented runs. The real case retained 101 exact
state / 82 air-collapsed differences and 23 native A/B differences. Synthetic
controls are labeled as synthetic with derived input hashes; they are not new
captures or parity acceptance. Timing excludes decoding, I/O, report formatting,
builds and generation; no full-triage, grid or M3 speedup is claimed.

## Firing guard and verification boundary

`tests/chunk_triage_test.py` now bounds Python single-cell reads for exact-match
layers, with and without a B-only air-kind change. It permits bulk reads rather
than enforcing a particular timer or caching configuration. The pre-change module
failed both subtests; the restored module passed. A second test rejects truncated
A/B/port arrays. The suite was already in `tests/verify.sh`; this change also
wires triage and port-triage tests into the existing CI workflow before builds.

Nine triage tests and the port-triage, region-NBT, carver-diff and saved-tick suites
passed. Manual diff review covered B-only differences, length validation, exact
state comparison and unchanged loop/sample order. Independent review was not run:
the immediately preceding local CLI attempt failed to connect, and was not
retried for this small change. Rust/native/full gate and hosted CI were not run.

Full local scripts, stamped receipts and negative-control logs:
`~/mc-spine-local/scratch/current/triage-layer-skip-20261113/`
(`measure.py`, `measurement.json`, `verify.py`, `verification.json`).
No capture, jar or native world was added to the repository.
