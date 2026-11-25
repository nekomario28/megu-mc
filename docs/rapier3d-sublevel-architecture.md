// SPDX-License-Identifier: LGPL-3.0-or-later

# Rapier/Parry/nalgebra SubLevel rigid-body physics architecture

Status: architectural evaluation and implementation plan; no production physics
backend is enabled by this document alone.

## 1. Executive evaluation

`rapier3d-f64` is the primary candidate for an optional Megu SubLevel rigid-body
backend because Minecraft legal coordinates span approximately ±30,000,000
blocks. At that magnitude a 32-bit float has an ulp on the order of two blocks,
so naïvely storing absolute world positions in f32 is categorically unsuitable.
A 64-bit float still has only a few millimetres of absolute-position
granularity there (about 0.004 blocks at the world border), which is
sufficient for block-scale rigid-body contact and transforms. The f32
`rapier3d` crate remains useful only as a smaller comparison benchmark and,
if ever used, would require explicit local-origin rebasing; it is not the
compatibility candidate.

The intended dependency shape is deliberately narrow:

* depend on `rapier3d-f64` (current upstream line observed during evaluation:
  0.36.1, Apache-2.0, MSRV 1.86);
* use Rapier's own re-exports (`rapier3d_f64::na` for `nalgebra` and
  `rapier3d_f64::parry` for `parry3d-f64`) instead of independently versioned
  direct dependencies wherever practical;
* enable `enhanced-determinism`; never enable `simd8` in a deterministic
  compatibility profile;
* evaluate `parallel` separately. Rapier documents its parallel solver as
  producing the same result independent of worker count, but Megu must verify
  that claim in its own differential harness before enabling it.

Apache-2.0 is license-compatible with Megu's LGPL-3.0-or-later project, but
compatibility is not permission to omit notices. The adoption gate includes a
direct and transitive license inventory and binary/source attribution review.

**Recommendation:** proceed only behind a compile-time feature and a strict
`PhysicsBackend` boundary. The built-in deterministic pure-Rust solver remains
the default until every gate in section 11 passes in the same commit that
enables Rapier for a compatibility profile.

## 2. Non-negotiable semantic boundaries

### 2.1 Physics-engine parity is not gameplay parity

Rapier can provide rigid-body integration, collision detection, contacts,
sleeping, CCD, and joints. It does not encode Create: Aeronautics behaviour.
Therefore Aeronautics-specific gameplay remains Megu-owned code above the
backend boundary:

* propeller/RPM thrust and addon propulsion parameters;
* balloon/buoyancy-block counting, altitude taper, vacuum behaviour, and
  environment gravity scaling;
* helm authority, rudder torque, control clamping, disassembly semantics;
* machine/network continuation inside a flying sub-world;
* passenger carry semantics and anti-cheat carried-motion evidence;
* block breaking/placing, assembly/disassembly, and save/load meaning.

A differential test must compare externally observable gameplay contracts, not
assume that equal solver versions imply equal gameplay. A Rapier body can be
perfectly stable while still failing Aeronautics parity because buoyancy or
helm semantics were applied incorrectly.

### 2.2 Vanilla entity physics is never replaced

Rapier applies strictly to dynamic rigid-body structures: SubLevels, airships,
and moving contraptions. It must never replace Minecraft entity physics.
Megu's exact Vanilla 1.21.1 solver continues to own, without exception:

* step-up/auto-step behaviour;
* entity cramming and pushing;
* ground snapping;
* water/lava drag and buoyancy;
* block friction, slippery blocks, honey/slime edge cases;
* vanilla collision clipping and pose transitions.

Passengers or entities standing on a moving structure may receive additional
carried-motion corrections from the SubLevel system, but their native entity
integration remains the vanilla solver.

### 2.3 Clean-room legal boundary

This design is clean-room. No code, comments, constant tables, solver patches,
test vectors, or implementation details are copied from Sable or from any
PolyForm Shield 1.0.0-licensed project. Compatibility is defined only through
public gameplay behaviour, file/protocol formats Megu already owns, documented
physical inputs/outputs, and black-box differential observations.

Rules for implementers:

1. Do not read or copy Sable source while implementing this backend.
2. Do not paste decompiled or source-derived constants into Megu.
3. Derive expectations from public behaviour and Megu's compatibility tests.
4. Record differential cases as input/state/action/output contracts, not as
   copied implementation snapshots.
5. If a behaviour cannot be established black-box, document it as an explicit
   approximation rather than laundering copied logic into the tree.

## 3. Layered architecture

```text
Minecraft/Vanilla entity physics       unchanged, authoritative for entities
---------------------------------------------------------------------------
Aeronautics gameplay semantics          propulsion, buoyancy, helm, assembly,
                                        machine ticking, passenger carry
---------------------------------------------------------------------------
SubLevel orchestrator                   fixed tick schedule, canonical input
                                        ordering, chunk lifecycle, save codec
---------------------------------------------------------------------------
PhysicsBackend trait                    narrow rigid-body/collider/joint API
                +-----------------------+-----------------------+
                | BuiltInBackend        | RapierBackend (opt-in)|
                | pure Rust, current    | rapier3d-f64/parry    |
                | compatibility solver  | nalgebra re-exports   |
                +-----------------------+-----------------------+
```

The orchestrator owns gameplay and world lifecycle. The backend owns only the
numerical rigid-body world. It must not mutate blocks, load chunks, fire game
events, access random thread-local state, or callback re-entrantly into world
mutation while stepping.

## 4. `PhysicsBackend` interface

The trait is intentionally expressed in plain arrays and integer handles so
`create-adapter` and the orchestrator do not expose `nalgebra` types in their
public contract. A Rapier implementation may convert internally through
`rapier3d_f64::na` and `rapier3d_f64::parry`.

```rust
pub type BackendBody = u64;
pub type BackendCollider = u64;
pub type BackendJoint = u64;

pub enum BackendMotion {
    Dynamic,
    Kinematic,
    Static,
}

pub struct BodyDesc {
    pub motion: BackendMotion,
    pub position: [f64; 3],
    pub rotation_xyzw: [f64; 4],
    pub linear_velocity: [f64; 3],
    pub angular_velocity: [f64; 3],
    pub linear_damping: f64,
    pub angular_damping: f64,
    pub gravity_scale: f64,
    pub ccd_enabled: bool,
    pub can_sleep: bool,
}

pub enum JointKind {
    Fixed,
    Revolute { axis_local: [f64; 3], limits: Option<(f64, f64)> },
    Prismatic { axis_local: [f64; 3], limits: Option<(f64, f64)> },
}

pub enum ForceCommand {
    Force { body: BackendBody, force_world: [f64; 3], point_world: Option<[f64; 3]> },
    Torque { body: BackendBody, torque_world: [f64; 3] },
    Impulse { body: BackendBody, impulse_world: [f64; 3], point_world: Option<[f64; 3]> },
    SetVelocities { body: BackendBody, linear: [f64; 3], angular: [f64; 3] },
}

pub struct StepInput {
    pub tick: u64,
    pub substeps: u32,          // fixed by compatibility profile, normally 4
    pub substep_dt: f64,        // fixed by compatibility profile, normally 1/80
    pub gravity_world: [f64; 3],
    pub force_commands: Vec<ForceCommand>, // canonical sorted order
}

pub struct BodyState {
    pub body: BackendBody,
    pub position: [f64; 3],
    pub rotation_xyzw: [f64; 4],
    pub linear_velocity: [f64; 3],
    pub angular_velocity: [f64; 3],
    pub asleep: bool,
}

pub enum PhysicsEvent {
    ContactStarted { a: BackendBody, b: BackendBody, manifold: ContactManifoldSummary },
    ContactStopped { a: BackendBody, b: BackendBody },
    JointLimitReached { joint: BackendJoint },
    CcdHit { body: BackendBody, time_of_impact: f64 },
}

pub struct StepOutput {
    pub states: Vec<BodyState>,       // sorted by stable body handle
    pub events: Vec<PhysicsEvent>,    // deterministic event order
    pub solver_digest: u64,           // canonical Merkle leaf input
}

pub trait PhysicsBackend {
    fn capabilities(&self) -> BackendCapabilities;
    fn insert_body(&mut self, stable_id: u64, desc: BodyDesc) -> BackendBody;
    fn remove_body(&mut self, body: BackendBody);
    fn teleport_body(&mut self, body: BackendBody, pos: [f64; 3], rot_xyzw: [f64; 4]);
    fn replace_compound_collider(&mut self, body: BackendBody, boxes: &[VoxelBox]) -> BackendCollider;
    fn patch_compound_collider(&mut self, collider: BackendCollider, patch: ColliderPatch);
    fn insert_joint(&mut self, stable_id: u64, kind: JointKind, a: BackendBody, b: BackendBody) -> BackendJoint;
    fn remove_joint(&mut self, joint: BackendJoint);
    fn wake_body(&mut self, body: BackendBody);
    fn force_sleep_body(&mut self, body: BackendBody);
    fn step(&mut self, input: StepInput) -> StepOutput;
    fn body_state(&self, body: BackendBody) -> Option<BodyState>;
    fn raycast_world(&self, from: [f64; 3], dir: [f64; 3], max_toi: f64) -> Option<RayHit>;
}
```

Exact final names may differ during implementation, but these properties are
binding:

* body/collider/joint handles map one-to-one to deterministic Megu stable ids;
* all vectors entering `step` are sorted by stable id and command kind;
* no `HashMap` iteration order leaks into insertion, removal, or event order;
* output contains canonical sorted body states and deterministic event order;
* Rapier sleeping state is observed, but gameplay sleep/wake policy is owned
  by the orchestrator;
* the backend never serializes gameplay state. Megu's save format stores the
  canonical body pose/velocity/collider/joint inputs and reconstructs backend
  internals deterministically in stable insertion order.

## 5. Determinism and numerical contract

### 5.1 Fixed-step scheduling

Minecraft advances at 20 ticks per second. The compatibility profile uses four
fixed physics substeps per tick (`80 Hz`, `dt = 1/80`). There is no wall-clock
accumulator and no variable substep count. If a server stalls, simulation time
still advances by integer ticks, not by elapsed real time.

`substeps` is part of the profile, not a performance knob. Changing it changes
gameplay numerics and therefore requires a new compatibility profile and a new
differential baseline.

### 5.2 Rapier determinism requirements

Rapier is only locally deterministic by default. For cross-platform
reproducibility the Rapier documentation requires all of the following:
`enhanced-determinism`, identical initial conditions and insertion order, strict
IEEE-754 platforms, and use of `nalgebra`'s `ComplexField`/`RealField` math
functions for any transcendental input construction rather than host-native
`f64::sin`/`cos`/`sqrt`-style calls whose implementations may differ.

Megu additionally requires:

* stable sorted insertion/removal of bodies, colliders, and joints;
* stable sorted external force/torque command order;
* no iteration over hash-based containers in simulation input preparation;
* canonical normalization of quaternions before insertion and hashing;
* rejection or deterministic sanitization of NaN/infinite commands before the
  backend sees them;
* one locked dependency version and one Rust toolchain per comparison run;
* no `simd8` in the cross-platform determinism domain;
* `parallel` treated as an experimental acceleration feature until Megu's own
  cross-core-count digest tests prove identical output.

### 5.3 `dst_oracle` Merkle verification

Every physics substep contributes a canonical leaf containing raw IEEE bits
for body id, pose, velocity, sleep state, active contact count, and material
properties that affect the step. Per-tick leaves form the SubLevel physics
Merkle stream consumed by `dst_oracle`.

The harness runs identical scenarios:

1. built-in backend vs Rapier backend for gameplay-equivalence envelopes;
2. Rapier vs Rapier repeated on one machine;
3. Rapier with 1, 2, 4, and maximum worker threads when `parallel` is tested;
4. Rapier across at least x86_64 Linux and one additional supported
   architecture/OS before any claim of cross-platform bit reproducibility.

If a digest diverges, the first divergent substep and body id are emitted. A
digest match is evidence, not proof of correctness; it must be paired with
black-box gameplay assertions.

## 6. Collider construction and incremental maintenance

### 6.1 Structure colliders

A dynamic structure is represented as a compound collider attached to its
rigid body. Individual voxel shapes are not inserted one-for-one unless the
voxel count is tiny. Adjacent identical full-cube voxels are greedily merged
into the smallest practical set of axis-aligned boxes in local body space.

The merge is deterministic:

1. scan local coordinates in x/y/z lexicographic order;
2. merge only voxels with identical collision material and shape class;
3. prefer maximal x runs, then z planes, then y slabs using a fixed rule;
4. retain non-full cubes as their own exact shapes;
5. emit boxes sorted by min-corner then max-corner.

Greedy merging is not required to solve the mathematically optimal rectangular
cover. It is required to be deterministic, incremental, and small enough to
keep broadphase/narrowphase cost bounded.

### 6.2 Static-world collider tiles

Static terrain colliders are only generated around active structures and only
inside loaded chunks. They are keyed by `(dimension, chunk, section)` and by
the canonical `SharedSectionRevTable` revision. A revision hit reuses the
cached collider; a revision miss regenerates only the affected section tile.
The whole chunk is never rebuilt every tick.

Cross-section border blocks are included in the generation footprint so a
block update at a section edge invalidates the correct neighbouring collider,
matching the border-wake semantics of `SharedSectionRevTable`.

### 6.3 Mutation and re-entrant block-break safety

Physics stepping is read-only with respect to Minecraft world state. During a
step, contact events are collected in a queue; no block may be broken,
placed, or replaced inside the narrowphase callback or solver loop.

Canonical per-tick order:

1. freeze the physics input snapshot for the tick;
2. drain already-committed block mutations and mark affected collider tiles;
3. patch dirty structure/static colliders in stable key order;
4. apply gameplay force/torque commands in stable order;
5. run the fixed number of substeps;
6. collect contacts/events into immutable output;
7. after the backend step completes, validate and apply gameplay consequences
   (damage, disassembly requests, block-break requests) through the normal
   world mutation path;
8. process removals/unloads in reverse dependency order.

This makes re-entrant block breaks impossible by construction. A contact that
should break a block schedules a request for step 7; it cannot invalidate the
collider currently being traversed in step 5.

## 7. Chunk unload and rigid-body lifecycle

The lifecycle is ownership-ordered and exception-safe:

### Assembly

1. validate the structure voxel set and gameplay constraints;
2. build deterministic merged local colliders;
3. allocate a stable Megu body id;
4. insert body, collider, and joints into the backend in sorted order;
5. activate gameplay machine ticking only after insertion succeeds.

### Normal tick

Active bodies are partitioned into awake, sleeping-but-loaded, and dormant
sets. Gameplay may still tick machines at the Minecraft rate even when the
rigid body sleeps, but force computation for a sleeping body is zero-cost
unless a wake condition exists.

### Chunk unload crossing

If a dynamic structure remains live because its body spans loaded chunks, it
continues. If no required chunks remain loaded, the orchestrator performs an
ordered freeze:

1. stop accepting new control input for the body;
2. capture canonical pose, velocity, sleep state, collider revision, and joint
   graph before removing anything;
3. remove joints, then dynamic body colliders, then bodies in reverse stable
   insertion order;
4. unload static collider tiles only after no remaining active body references
   that tile;
5. persist the canonical state in the owning chunk/SubLevel save record.

### Reload

Reload reconstructs the canonical inputs in stable id order. Rapier-internal
handles are not persisted. The first post-load step must produce the same
digest as if the body had been continuously present, assuming no external
world difference.

### Failure paths

Partial insertion rolls back in reverse order. Partial removal never leaves a
joint pointing at a missing body. If saving fails after backend removal, the
in-memory canonical state remains authoritative until either persistence
succeeds or the server shutdown protocol serializes it.

## 8. Mandatory lightweight / zero-overhead conditions

These seven conditions are acceptance requirements, not aspirations:

1. **Zero CPU cost with zero dynamic structures.** If no dynamic structures
   exist, no Rapier pipeline is constructed or advanced; no broadphase,
   narrowphase, island manager, collider scan, or per-tick allocation occurs.
2. **Rigid-body sleeping.** Stationary structures enter solver sleep/island
   sleep and are excluded from integration and contact solving until a
   deterministic wake condition occurs.
3. **No whole-chunk collider rebuild every tick.** Static colliders update
   incrementally and are keyed by canonical `SharedSectionRevTable` revisions.
4. **Greedy collider merging.** Adjacent identical voxel colliders are merged
   into a minimal practical compound set of boxes before insertion.
5. **Strictly limited CCD.** CCD is enabled only for bodies flagged fast or
   gameplay-critical; it is never enabled globally.
6. **Fixed substep count.** Each compatibility profile has a fixed substep
   count and `dt`; performance problems are handled by sleeping, culling, or
   budgets, never by silently changing integration time.
7. **Measured deterministic execution.** `enhanced-determinism`, parallel
   execution reproducibility, Megu-side input ordering, and floating-point
   hygiene are benchmarked and digest-verified before enablement.

## 9. Feature mapping: built-in solver versus Rapier

| Capability | Built-in compatibility solver | Rapier backend |
|---|---|---|
| 6-DOF integration | Existing deterministic Euler model | `rapier3d-f64` rigid bodies |
| Compound voxel colliders | Explicit local shape tests | Parry compound shape, greedy-merged |
| Contact manifolds | Limited/custom | Parry/Rapier manifolds with stable summaries |
| CCD | None or narrowly custom | Rapier nonlinear CCD, opt-in per body |
| Fixed joints | Gameplay-level grouping | Rapier fixed joint or single compound body |
| Revolute joints | Not initially represented | Rapier revolute/impulse joint |
| Prismatic joints | Not initially represented | Rapier prismatic joint |
| Sleeping | Explicit dormant logic | Rapier islands plus Megu policy |
| Vanilla entities | Exact vanilla solver, unchanged | Exact vanilla solver, unchanged |
| Gameplay propulsion/buoyancy | Megu-owned | Megu-owned, external forces |

Joint use is conservative. A permanently connected machine assembly should
normally be one compound rigid body rather than a chain of fixed joints.
Revolute/prismatic joints are for genuinely articulated Create-style mechanisms
and must carry deterministic joint stable ids, limits, and break behaviour.

## 10. Binary-size, compile-time, and MSPT budget analysis

Rapier, Parry, and nalgebra are not lightweight dependencies in source size.
The evaluation expectation is that release binary size increases by roughly a
small single-digit number of MiB and clean compile time increases materially
(tens of seconds to minutes depending on machine and feature set). Those are
planning ranges, not acceptance measurements. The implementation must measure
the actual target triples and record them in the enabling commit.

Cost-control design:

* the dependency is optional and absent from default builds until gates pass;
* default-features are disabled wherever possible;
* no debug-renderer/testbed/serialization feature is enabled unless required;
* `parallel` is a separate feature because it adds rayon and scheduling cost;
* f32 `rapier3d` is benchmark-only and never part of the production default;
* static collider caches are memory-bounded and LRU/evicted deterministically.

Suggested initial server budgets for a 20 TPS/50 ms tick, to be revised only
with measurements:

| Scenario | Budget |
|---|---:|
| zero dynamic structures | 0 additional CPU instructions beyond one empty-set check |
| one sleeping structure | < 0.01 ms median physics MSPT |
| 32 sleeping structures | < 0.05 ms median physics MSPT |
| 8 awake small structures | < 1.5 ms p95 physics MSPT |
| 8 awake small structures with terrain contacts | < 4.0 ms p95 physics MSPT |
| CCD body | individually budgeted; global CCD forbidden |

Compile-time acceptance is measured as clean and incremental wall time with
`CARGO_BUILD_JOBS=1` and with the normal parallel setting. Binary-size
acceptance compares stripped release artifacts for the same target and feature
matrix. MSPT acceptance uses deterministic scenarios, reports median/p95/max,
and proves the changed path executed with counters.

## 11. Unified single-commit verification gates

Any commit that enables or changes the Rapier backend for a compatibility
profile must pass all eleven gates in that same commit. Splitting implementation
work into steps does not weaken this rule: intermediate commits may keep the
backend disabled, but the enablement commit is atomic with the evidence.

### Correctness gates

* **V1 — trait semantic correctness:** unit and property tests cover insertion,
  removal, teleport, forces, collider replacement/patching, joints, sleep/wake,
  contact output ordering, and backend handle lifetime.
* **V2 — deterministic replay:** repeated same-machine runs are bit-identical;
  cross-platform claims additionally pass the supported-platform matrix. The
  first-divergence report names substep, body, and digest.
* **V3 — lifecycle/re-entrancy:** chunk unload/reload, partial assembly failure,
  re-entrant block-break attempts, contact-triggered mutation, and joint/body
  removal ordering all fail closed without dangling handles or world mutation
  during stepping.
* **V4 — gameplay black-box parity:** public Aeronautics/Create behaviour cases
  (propulsion, buoyancy, helm, docking/disassembly, passenger carry, machine
  continuation) match the compatibility oracle within the explicitly recorded
  numeric envelope. Physics-engine parity alone does not satisfy V4.

### Compatibility/build gates

* **C1 — feature matrix:** default build without Rapier, Rapier deterministic
  build, and benchmark-only comparison configurations compile and run. Invalid
  combinations such as `enhanced-determinism` plus `simd8` are compile-time or
  CI-time failures.
* **C2 — dependency/toolchain reproducibility:** locked versions, MSRV,
  vendoring/hash checks, cargo metadata, and the pinned Rust toolchain are
  verified. No floating or unpinned dependency resolution is accepted.

### Performance/zero-overhead gates

* **A1 — dormancy:** zero-dynamic-structure worlds perform no physics pipeline
  work; this is proven by counters/traces, not inferred from equal timings.
* **A2 — sleeping and cache reuse:** stationary structures sleep, stable
  terrain collider revisions hit the cache, and block updates regenerate only
  the affected section tiles. Whole-chunk periodic rebuilds fail the gate.
* **A3 — MSPT and scaling:** the section-10 scenarios meet their budgets and
  show no unbounded allocation growth, event amplification, or no-progress
  retry loop.

### Precision/license gates

* **P1 — far-origin precision:** scenarios at ±1,000,000 and ±30,000,000 blocks
  preserve stable contact, docking, and transform behaviour. f64 absolute world
  positions or an audited equivalent rebasing scheme are mandatory; un-rebased
  f32 fails.
* **L1 — license and notice audit:** direct and transitive dependencies are
  inventoried; licenses are compatible with LGPL-3.0-or-later distribution;
  Apache-2.0 notices and any required attribution are present; no PolyForm
  Shield or copied clean-room-prohibited material enters the tree.

## 12. Seven-step migration order

The migration order is fixed. A later step may not start unless the prior step
is independently inspectable and does not regress the default build.

1. **Extract the backend boundary.** Put the existing pure-Rust solver behind
   `PhysicsBackend` with no gameplay behaviour change and keep it default.
2. **Build the differential oracle.** Add deterministic scenario replay,
   canonical digests, first-divergence reporting, and black-box gameplay cases
   using only Megu/public contracts.
3. **Add measurement infrastructure.** Add feature-matrix CI, binary-size,
   compile-time, allocation, cache-hit, sleeping, and MSPT counters before any
   Rapier dependency is enabled.
4. **Prototype f32 comparison only if useful.** Benchmark `rapier3d` in a
   throwaway/test-only target to measure dependency and solver cost. Do not use
   it for far-world production semantics.
5. **Implement dormant `rapier3d-f64` backend.** Add the optional dependency,
   body/collider/joint adapter, fixed-substep driver, canonical input ordering,
   sleeping, and selective CCD while compiled off by default.
6. **Implement world integration.** Add greedy merged compound colliders,
   revision-keyed incremental static tiles, chunk unload/reload persistence,
   and non-re-entrant mutation draining.
7. **Enable behind an explicit compatibility profile.** Turn it on only for an
   opt-in profile after V1–V4, C1–C2, A1–A3, P1, and L1 all pass in the same
   commit; retain the built-in backend as fallback and rollback path.

## 13. Risks and decision points

| Risk | Mitigation |
|---|---|
| Cross-platform FP drift | `enhanced-determinism`, nalgebra math functions, digest matrix, no `simd8` |
| Parallel nondeterminism | keep sequential initially; verify worker-count invariance before enabling |
| Compile/binary cost | optional feature, minimal features, measured C2 evidence |
| Collider explosion | greedy merge, exact non-full shapes only, bounded section caches |
| Contact event amplification | manifolds summarized deterministically; wake/break thresholds |
| Re-entrant world mutation | read-only step plus post-step command queue |
| False gameplay equivalence | V4 black-box gameplay cases independent of solver internals |
| Clean-room contamination | documented provenance rules and L1 audit |

## 14. Decision

Adoption is technically plausible and architecturally desirable for complex
contacts, articulated mechanisms, sleeping islands, and selective CCD. It is
acceptable only as an optional `rapier3d-f64` backend behind the boundary in
this document. It is not acceptable as a replacement for vanilla entity
physics, as an unfeature-gated dependency, or as a shortcut around
Aeronautics gameplay compatibility work.
