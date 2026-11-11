# Fluid update parity trace (Minecraft Java 1.21.1)

Status: portable comparison tool only. **No native capture or mc-spine adapter is included yet**, so passing the tool's own tests does not assert gameplay parity. Tracking issue #39.

## Reference boundary and provenance

- Native truth: user's independently obtained, SHA1-pinned Minecraft 1.21.1 server JAR; only observable state-update events should leave that local test environment.
- Candidate: the mc-spine binary checked out at a specific commit. Use identical scenario, seed, initial blocks, dimension and tick-zero alignment.
- FerrumC's 1.21.8 fluid implementation is a research reference, not a copy source or oracle. This tool contains no FerrumC, Torch, Azalea or Mojang code.
- Do not check in copied mappings, generated registry tables, entire upstream worlds, or raw Mojang-derived assets. Keep local captures out of commits until a fixture's redistribution/provenance is reviewed.

## Trace TSV v1

Each trace is UTF-8, newline terminated, and consists of the following fields in exactly this order:

```text
# format=mc-spine-fluid-trace-v1
# version=1.21.1
# scenario=water_hole
tick	sequence	x	y	z	state
5	0	0	64	0	34
5	1	1	64	0	35
7	0	2	63	0	36
# end=count=3
```

- tick is relative to the agreed scenario start, not wall-clock elapsed time.
- sequence starts at zero and increments by one for each emitted update **within that tick**. Preserve duplicate updates even when the final state is the same.
- x, y and z are signed block coordinates; state is a 1.21.1 global block-state ID (0–26683).
- The end-count marker plus final newline guards against accidental empty/truncated traces; hash and event-stream source provenance remain external runner responsibilities.
- Identical event multisets in a different order are a failure. Differences are reported separately as missing/extra events and order-only differences.
- Input capture adapters must exclude unrelated block updates consistently and document the filtering rule before comparing real servers.

## CLI and acceptance

```sh
python3 tests/fluid_trace_diff_test.py
python3 tools/fluid_trace_diff.py --native /tmp/native.tsv --candidate /tmp/mc-spine.tsv
```

Exit code 0 = exact match, 1 = mismatch, 2 = invalid/uncomparable trace. The JSON report includes counts, first mismatched event, and examples of missing/extra updates. Empty, wrong-version, wrong-scenario, noncontiguous sequence, out-of-order, and truncated traces fail closed.

## Next slices

1. Add independent native 1.21.1 capture of deterministic water, lava, hole-search, two-source, waterlogged and cross-chunk fixtures.
2. Capture the corresponding mc-spine updates on the same tick alignment and record exact observed events.
3. Add mutation controls for wrong time, missing neighbor wake-up, wrong state, and duplicate publication.
4. Compare CPU time, p95/p99 tick duration, and RSS **only after** matching output; evaluate the FerrumC kernel design separately.
