# Pinned weird scaled sampler / native noise seam

Target authority is Minecraft 1.21.1 / protocol 767, server SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. Horizon/Valence versions are not
specifications for this seam. No dependency, server/world startup, GPU or donor
execution is introduced.

## What changed and why

The installed pack has three reachable `weird_scaled_sampler` nodes. They were
parsed by the generic unknown-type arm as `DF::Zero`: the installed-node test
reproduced `[0.0, 0.0, 0.0]` at nontrivial coordinates. The existing DF parser,
evaluator and dependency walk now own a typed sampler. Missing required inputs,
unresolved names and unsupported mappers fail construction; a direct retry of a
failed named parse does not become a cached placeholder success. The input keeps the caller's interpolation context.

The actual sampler is `rarity * abs(noise(x/rarity,y/rarity,z/rarity))` in f64.
Its strict type_1/type_2 mapper boundaries include signed zero, adjacent binary64
values, infinities and NaN fallthrough. Native mapper functions return scales,
not ordinal bucket numbers.

A separate raw-noise comparison exposed a pre-existing native-constructor defect:
six initial spaghetti samples were approximately 4.67% too large. The native
`firstOctave/amplitudes` path had reused converted noise's variance estimate.
Native construction now uses the span-derived final factor and two separately
accumulated Perlin stacks. A multi-octave case additionally exposed a three-ULP
error from precombining amplitude and octave weight; native multiplication order
is retained instead. Converted and legacy paths retain their existing behavior;
converted regression pins are not native-parity evidence. The native constructor
is shared, so non-sampler climate/terrain noises change too; this is why broader
regressions were rerun, and why whole-world acceptance remains outside this seam.

## Reference and scope

`vanilla.json` contains 254 actual-class cases: 38 mapper and 216 sample cases.
Samples cover both mapper types, all rarity branches, two seeds, the three
spaghetti noises and native multi-octave/sparse constructor guards using
`temperature`, `continentalness`, and `offset`. Raw noise and sampler outputs are
separate assertions, so a branch cannot conceal a wrong noise field.

The Java probe uses actual constructors and methods:

- `dys$z`, `dys$z$a`, `dzf$a`: sampler and actual mapper functions.
- `dzz`, `dzl`, `eoh`: name-derived Xoroshiro positional source and NormalNoise.
- `dyr$c`, `dyr$e`: NoiseHolder and block-position context.

NoiseHolder's registry holder is null; its real NormalNoise is non-null.
Pinned bytecode confirms `getValue` reads only that noise field. This is normal
construction, not allocation bypass, registry-codec validation, RandomState's full
wiring, or world execution. `scaled_bits` are probe-derived coordinates from the
actual rarity value; raw noise is sampled separately at those coordinates and
checked against the independently invoked actual sampler result. The input noise
JSON comes from the authenticated jar and must agree with the installed pack.

Rust tests exercise production parsing/evaluation and the installed dependency
graph. They do not establish all seeds, arbitrary noise configurations, the full
router, aquifer or surface context, generated chunks, or performance parity.
No old worldgen capture is re-recorded.

## Verification

| Gate | Result |
| --- | --- |
| Rust `weird_sampler_` | 10 passed; 254 mapper/raw-noise/sampler rows match raw f64 bits |
| Actual-class repeat | Both complete runs match the committed corpus, requests and provenance |
| Offline oracle guards | 5 passed, including wrong jar preserving an existing output |
| Selected canonical mutations | 13 proved (11 original + 2 added); no not-a-gate, parked, skipped or surviving cases |
| fmt / all-target warnings-denied clippy / shell syntax | Passed |
| Gate/layer self-tests, graph, debt | 15 / 17 passed; 19 modules, 44 edges, 0 named errors; 0 over limit |
| Restored production binary | Rebuilt after mutations; container and gameplay wire PASS |
| Independent static review | No actionable new correctness regression; no independent build/Java/runtime execution |
| Full serial release | **205 passed / 23 failed / 1 ignored**, exit 101; 15 failures contain `PoisonError` |

The baseline was 194/24/1. Ten tests were added, with no new failed names; the
existing `context_overrides_heights_bounds_and_biomes_keep_existing_results` now
passes, accounting for the extra pass and removed failure. It previously failed
comparing two ocean-floor views at (-1,-1); changed density/noise changes that
terrain input. **This is not a general heightmap repair or native heightmap proof.**
The generic unchanged-failure comparator correctly rejects this changed set;
`unchanged_failure_baseline` remains false. The scoped run explicitly checks the
single named removal instead of weakening that comparator or silently accepting
any count change. No test/capture was re-recorded.

Fresh-process `biome_vectors`, both `material_rules` tests, and the removed context
test pass. Fresh `aquifer_vectors` still fails with 97 mismatches. Its failure
poisons the shared lock before later worldgen tests in the full suite; the 23
failed names are not 23 independent root causes. Existing biome vectors passing
is bounded regression evidence, not fresh whole-world native acceptance.

Results and exact source/corpus/binary identities are in `run.json`; resource
accounting and completion markers are invocation-bound in `unit.jsonl`. Logs are
only normalized for trailing whitespace/extra EOF blanks, with raw and saved
hashes in `log-normalization.json`. Native Guava's deprecated Unsafe warning is
retained; the probe itself uses no allocation bypass.

The pre-existing full migration acceptance remains red. Full `tests/verify.sh`,
the whole mutation campaign, hosted CI, publication, main integration and release
were not performed for this slice.

Implementation was split between bounded commandcode DeepSeek v4.1 Flash workers
(high for Rust, medium for Python) and parent-authored actual-class Java calls.
Workers did not build/run Java or Cargo; parent executions are the runtime
verification. The separate rationale audit clarified the shared constructor's
blast radius and the `eval0_c` pointer; its follow-up verified the final done-spec
path. The earlier audit's stale path refers to its old snapshot, not a live spec.
Parent review corrected ordinal-bucket, NaN and sign expectations
in the worker's initial Python tests using the real corpus, not a synthetic one.

## Remaining boundaries and failed attempts

- Pointer-keyed NCACHE across dropped/recreated generators is a pre-existing
  lifetime hazard identified by static review, not repaired or runtime-proven
  here. Corpus generators for both seeds stay alive simultaneously; this is a
  construction-lifetime precondition, not evidence of cache-lifecycle parity.
- Direct failed-reference retry is covered. Cyclic graphs can still retain an
  abandoned placeholder through another cached name. The general unknown-node
  fallback and missing `firstOctave` default are also not a full native codec.
  Pinned installed inputs in this slice resolve without those malformed cases.
- The first worker launch disabled extensions and therefore could not register
  CommandCode. It failed before implementation. Subsequent workers retained
  provider extensions; the receipt requires successful final responses from the
  requested provider/model and rejects the first attempt as evidence.
- The first broad-check run stopped when the unchanged-failure comparator rejected
  the legitimately changed failed-name set. Its verdict was preserved, not made
  green by changing the generic comparator.
- An additional-mutation attempt used case labels where `MUTATION_FILTER` selects
  test names. Zero selected gates correctly failed the run. The corrected selection
  uses the actual fully-qualified Rust test; two distinct new mutations then prove
  per-octave multiplication order and final rarity multiplication.
- A completed check run's final journal lines lost live-PID invocation metadata.
  The receipt rejected them even though the tests had run. The final run kept
  marker-emitting processes alive briefly and was repeated; its markers carry
  the expected invocation directly. No unbound line or default reaped-unit
  property was accepted as the completion proof.
- Original red captures are incremental authoring logs, not complete snapshots of
  every intermediate test harness. Final-source baseline/mutant/restored controls
  are the authoritative recurring-failure proofs. The static review preceded the
  two extra mutation registrations; production source identities are unchanged.

## Reproduce

With the authenticated jar already installed and fresh resource admission:

```sh
python3 tools/weird_sampler_oracle.py
python3 tests/weird_sampler_oracle_test.py
cargo test --release --offline --locked -j 1 weird_sampler_ -- --test-threads=1
MUTATION_FILTER=weird_sampler_ MUTATION_RELEASE=1 CARGO_BUILD_JOBS=1 \
  bash tools/mutation_check.sh --offline
```

See [the contract](../../../specs/done/weird-sampler/README.md) for the boundary between
this repair and the remaining worldgen work.
