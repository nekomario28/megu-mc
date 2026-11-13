# Mineshaft payloads, restart and expanded native coverage

Working-tree extension of the [block/RNG checkpoint](mineshaft-production-20261113.md),
base HEAD `94f3ca6bc6a7d5260729908958888ccd72f5aea6`, branch
`perf/worldgen-parallel`. No commit/push or promotion is implied. The
[connection contract](../../../specs/mineshaft-dispatch/README.md) owns the current boundaries.
The earlier checkpoint's hashes and comparison counts remain historical, not current fixture hashes.

## One handoff, distinct runtime owners

`GenerationPayload` replaces the template-only type in the existing staging channel.
Mineshaft writers now stage a real chest-cart payload with the native signed loot
seed, and a cave-spider spawner payload when placing its block. Native placement
RNG consumption is unchanged. Gen owns the queue, source-chunk claims and one-shot
piece flags; the runtime owns typed NBT and persistent entity identity. No saved
pieces, captured terrain or fixture coordinates enter production generation.

Chest carts have their own vehicle collection, not Mob walking AI. The existing
interest tracker sends their spawn packets. Spawner NBT joins the existing chunk
block-entity packet path using native registry type 9. Generated spawner blocks
also enter the world overlay: a persisted one-shot flag alone cannot reconstruct
that block after feature-cache eviction. Existing user edits take precedence;
replacing a spawner removes its NBT, without clearing the source claim.

Saving drains pending payloads, then appends a typed `GEN1` snapshot before the
existing `TIK1` section. Checked length framing locates it without scanning for
marker bytes. It preserves UUID, position, loot data, mutable spawner NBT, source
claims and piece flags. Load validates generated state before applying it and
allocates fresh runtime entity IDs. Recomputing a claimed source cannot duplicate
a cart or reset NBT. Claims remain after object removal. This is the port's own
save format, not a claim of vanilla region/entity-file compatibility.

Block reads can hold mob/player locks and **must not acquire SAVE_LOCK or drain
payloads**. Tick/chunk boundaries transfer ownership under save → gen → runtime;
saving follows the same order. The raw snapshot builder retains the existing
caller-held save-lock contract. Generation metadata restoration requires an
initialized generator; changing world-generation settings is not a migration path.

## Native authority and its limits

The pinned 1.21.1 jar (SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) produced the fixtures through actual methods:

- `MineshaftDataOracle` constructs/configures a native spawner and writes its NBT,
  including typed short defaults and cave-spider SpawnData. Before/after Xoroshiro
  snapshots show the fresh spawner setter consumes no placement RNG.
- Chest loot NBT uses the actual delegated `ContainerEntity` save method with an
  explicit registry provider. Full cart saving with a null Level was rejected;
  the successful probe is **not full native entity serialization**. UUID/position
  ownership and persistence are verified separately in the Rust runtime.
- The pipeline corpus now has **19 authored scenes**. Actual native
  `addFreshEntity` captures the chest cart at `[-11.5,-28.5,-23.5]`, seed 0,
  child 32, loot seed `-1713839420441558611`. Rust compares staged position/loot,
  ordered block events and final placement RNG, not just rails.

The default oracle modes compare without rewriting fixtures. A corrupted spawner
NBT reference is rejected with `mineshaft entity data differ (no files changed)`;
restored references match. Native helpers serve explicit authored inputs and
reject unsupported calls; authored stone terrain is not a natural-world replay.

### Natural Mesa, without a fixed-biome substitution

| World seed | Start chunk | Native pieces |
|---|---|---:|
| 0 | `(31,-110)` | 180 |
| 3 | `(-71,-92)` | 113 |

These use the real Overworld multi-noise source in native admission. Rust admits
the same Mesa starts and compares **all 293 ordered piece bounds, depths,
orientations and Mesa tags**. The three earlier fixed-badlands interventions remain
labelled interventions. Searches produce fixtures only; runtime does not read them.
This is admission/geometry coverage, not complete natural Mesa chunk parity.

### Actual native reference-manager traversal

For seed 4242, target `(4,-2)`, the probe calls actual native
`createReferences → StructureManager`, then places the returned starts on authored
terrain with one caller stream. Packed reference order is
`["-4294967287","12884901892"]`; per-start successful writes are `[29,416]`.
Final RNG words are `[-616057497607443874,-3821003885875680158]`.
Rust compares reference order, both ordered write sequences, intermediate and final RNG.

A separate native-container regression exposed that NORMAL and Mesa must not
share hash-set capacity. The old union/filter order failed; production now groups
references into independent sets by structure type. The 25/100-member container
vectors test this boundary. The two-start manager fixture is NORMAL-only: it does
not establish general mixed-type runtime-world traversal.

## Executed checks

- **14 focused mineshaft tests passed**, including the natural Mesa geometry,
  native multiple-start traversal, typed payloads and real population path.
- **428 passed, 1 ignored** in the default-feature serial Rust suite, 109.32 s
  test execution. This includes the two typed NBT/snapshot unit tests.
- The save/restart test creates actual authored-writer cart/spawner payloads,
  saves a pending handoff, changes spawner Delay to short 137, then loads in a
  fresh process. It verifies UUID/position/loot, exact mutable NBT, the spawner
  block, restored claims/flags and no second payload on source recomputation.
  Removed objects leave durable claims. Repeated settled saves are byte-identical.
- The real interest tracker delivers a cart spawn frame over a loopback socket.
  The spawner chunk-entity producer preserves the mutated NBT and uses the native
  registry ID. These are packet/data checks, not rendered vanilla-client evidence.
- **16 compiling mutants** failed, each twice, with rebuilt restored controls:
  11 new payload/lifetime/reference/lock/packet guards and five existing
  writer/container guards. Missing sources or compile errors are not counted.
- All-target/all-feature Cargo check passed. The pinned source-bound layer gate
  observed **39 modules, 102 edges, zero named errors**. No compatibility alias
  for the old staging type remains.
- A freshly rebuilt normal executable passed the empty-placed-features dispatch
  regression. Complete `tests/verify.sh` (multi-round/wire/all-mutation promotion)
  was **not run**. Codex review was **not run**, as requested.

## Failures retained and guards that fired

The initial missing-spawner-payload assertion and shared-reference-container
assertion failed before their fixes. A later full suite stopped in the existing
falling-block save test: adding a lock inside a caller-locked snapshot builder
caused self-deadlock. The outer run timed out after 900 s. The implementation now
preserves the existing caller-held contract, and that exact test and the full
suite pass. Subsequent local verifier commands have bounded timeouts.

One test initially assumed exactly two claims even after neighbor updates; actual
updates populated eight additional sources. It now checks preservation of the
saved claims rather than rejecting legitimate new ones. A renamed drain field
also left one obsolete mutation anchor: the skip was rejected, the anchor was
repaired and both template mutants proved. A runner's omitted `--jar` failed on
an absent default cache path; the resumed checks used the authenticated jar
explicitly. None of these failed/omitted runs is reported as successful validation.

Local artifacts are retained under
`/home/yuu/mc-spine-local/scratch/current/mineshaft-entities-20261113/`.
`verification-2/current-verification.json` binds logs, the rebuilt binary and the
final non-document input manifest. It explicitly separates source-locked reuse
of earlier passed checks from resumed controls and the mutation-anchor repair.

| Local receipt | SHA-256 |
|---|---|
| `verification-2/current-verification.json` | `6c96a9e34228436fa2193fa783a42191a356773f26a6d5267bb29749b0f6454a` |
| `verification-2/final-inputs.json` | `6d12357eaedc74d1f8801e8e33186b9fff6e5b46398bf8dc8a1c5de58efd732c` |

Fresh resource GO checks preceded named local scopes, MemoryMax 1,800 MiB and
CPUQuota 100%; Java uses `-Xmx768m`. No GPU/cloud work or other-job termination
occurred. These tests make no performance claim.

## Not closed

Vehicle motion, loot-table unpacking/container interaction and spawner server
ticking remain unimplemented. Broader state-lifetime/mixed-type worlds, natural
Mesa whole-chunk parity, original A/B material history, asset redistribution,
P2-1, M2/M3 and full promotion remain separate work. The earlier fresh-native
98,304-cell match is not a new entity/save acceptance criterion.
