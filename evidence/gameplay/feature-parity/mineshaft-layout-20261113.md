# Seed-derived NORMAL mineshaft layout checkpoint

Starting commit: `a205e2d56405edd99316616285e2bbd7e0b595a7`, with uncommitted
changes bound by the local receipts' source hashes. This is a verified layout
component, **not production mineshaft dispatch or zero-diff acceptance**.
The earlier [piece-writer replay](mineshaft-writer-replay-20261113.md) remains a
separate, explicitly scripted placement experiment.

## The isolated contract

[The layout kernel](../../../src/mineshaft_layout.rs) constructs ordered NORMAL
mineshaft pieces from world seed, start chunk and height inputs. It does not read
saved piece NBT. Room entrances, corridor flags/sections, crossings, stairs,
inclusive collision bounds, recursion order and vertical adjustment belong to
this owner. It makes no block writes.

[The native oracle](../../../tools/structure-oracle/MineshaftLayoutOracle.java)
runs the authenticated 1.21.1 `findGenerationPoint` body, then independently
partitions the actual native calls into context seeding, a discarded double,
room construction, recursive children and height adjustment. The complete piece
NBT and final RNG position must equal the unpartitioned native method. Inputs
are NORMAL, sea level 63, minimum Y -64 and native adjustment margin 10. Candidate
admission, biome eligibility and Mesa's terrain-dependent adjustment are excluded.
The scripted context is not a generated world.

The discarded `nextDouble()` precedes the initial room. Skipping it changes the
entire piece tree. `Lcg::for_large_feature` now owns the existing raw-nextLong
seeding formula shared with carvers and legacy structure frequency reduction;
it does not use decoration seeding's odd multipliers. The carver's trace-arm
boundary is retained.

Snapshots read the native LegacyRandomSource's 48-bit state without consuming a
sentinel draw. Native draw counts are also retained in local JSON; Rust compares
state48 at the five boundaries. A compiling Java mutant that consumes a double
inside each snapshot is rejected by the whole-method/partition consistency
assertion, and the restored oracle reproduces the original vectors.

## Results and stage distinction

The fixed six oracle inputs produce 904 pieces. Rust matches every ordered piece
record and all 30 RNG boundary states, including negative seeds/coordinates and
seeds above JSON f64's exact-integer range. The fixture uses integer TSV parsing,
not the project's floating-point JSON number representation.

For seed 4242, start `(4,3)`, native layout produces 200 pieces and shift -41.
Against both retained A/B starts, the only full-NBT difference is child 130's
`hps`: layout has 0, saved state has 1. All other fields and ordered pieces match,
including the target's children 85, 86 and 185. `hps` is the spider-placement flag,
not a layout choice. It is reported, not normalized out of the fixture or copied
back into the generator. This does not reconstruct placement order, decoration
RNG or the saved world's writer history.

## Checks and their limits

- An isolated `rustc --test` harness compiles the exact changed math, placement
  and layout modules: 13 tests pass. This includes the pre-existing native RNG,
  structure-candidate and frequency fixtures; it is not the whole Cargo suite.
- Six Rust mutants compile and fail the named layout test: omitted double,
  shallower recursion limit, unconditional spider roll, omitted recursive
  expansion, unshifted entrances, and ignored start-chunk X in shared seeding.
  Restored controls pass. They are registered in
  [the existing mutation gate](../../../tools/mutation_check.sh), but the complete
  Cargo mutation runner was not executed in this checkpoint.
- `cargo check --offline --locked --all-targets --all-features -j 1` passes on the
  changed tree. Existing warnings and the new kernel's lack of a production
  caller remain visible; no dead-code suppression was added.
- The source-bound layer gate passes: 36 modules, 83 edges, no named errors.
  The module/edge policy adds only this classified owner and its dependencies.
  The missing PATH inspector was replaced by the exact CI-pinned gxkit revision
  `d6b9ad8db4774919c8b4fb032342e53b2548b660`, extracted from an existing local Git
  object. Its failure probe was preserved, including rejection of an early
  wrapper that masked the inspector's nonzero exit status.
- The CLI compares the generated vectors without writing expectations by
  default. A wrong-reference control rejects with exit 1 without changing either
  fixture; the canonical reference passes again.

Manual shape/diff review kept one procedural-layout owner and shared seeding,
without a second world/placement state. The independent read-only Codex review
produced no review: connection failures persisted until its 90-second/1-GiB bound
expired with exit 124. It was not retried or reported as passed.

These are component checks. The pre-change runtime has no mineshaft dispatcher;
a missing API/compile failure would not be a pre-change behavioral proof. This
checkpoint does not supply that proof, a fresh block dump, a full regression
pass, hosted CI or production acceptance. This verification record predates the
checkpoint's commit/publication.

## Reproduction and retained evidence

With the authenticated jar available locally:

```sh
python3 tools/mineshaft_layout_oracle.py --jar /path/to/server.jar
```

The runner authenticates SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`
before extraction/execution. The numerical fixture is
[`mineshaft_layout_1211.tsv`](../../../tests/data/mineshaft_layout_1211.tsv);
replacement requires an explicit `--output`. Empty payloads are spelled `-`,
not a trailing tab. Publication preflight rejected the old spelling in the named
Rust test, regenerated the fixture from native, and verified that the complete
native piece/RNG JSON was unchanged. Runtime jars, bytecode listings,
worlds and captured NBT remain outside the repository.

Local scripts, stamped receipts, controls and logs live under
`~/mc-spine-local/scratch/current/mineshaft-layout-20261113/`. Native and focused
Rust runs use 1 GiB scopes; the all-targets check and source-graph expansion use
1.5 GiB scopes, all at CPUQuota 100%. `run_probe` passes `-Xmx768m`; an earlier
`JAVA_TOOL_OPTIONS=-Xmx384m` was not an effective heap override and is not used as
an execution-footprint claim. Mutations use copied sources and isolated outputs,
not the shared Cargo target or restored old mtimes. The format-only publication
refresh and repeated focused controls are recorded separately under
`~/mc-spine-local/scratch/current/mineshaft-publish-20261113/`.

Next: recover admission/biome selection and decoration step/index/RNG ordering,
then connect a seed/config-derived layout to the real dispatcher and implement
piece placement with its native clipping and out-of-BB support writes. Acceptance
must compare the full volume against A and B separately, without fitting RNG
seeds or capture injection. M2/M3 and P2-1 remain open.
