# Ristretto / embedded JVM evaluation (2026-11-20)

## Decision

**No-go for replacing the embedded HotSpot 21 runtime with Ristretto in the current mc-spine compatibility path.** Keep HotSpot JNI for the bounded NeoForge/Fabric profile and retain the S3 direct-buffer bridge behind a production tick-owner integration gate. Ristretto is a plausible isolated prototype candidate: its current project advertises Java 17/21/25 test targets, invokedynamic interpretation, class loading, ModuleLayer, JIT and VarHandle/Unsafe implementations. Those project-level features do not yet establish that the pinned unmodified NeoForge + EventBus + FML + Fabric Loader/Mixin stack runs correctly on it, or that mc-spine's host callbacks can be ported with equivalent semantics.

The embedded HotSpot baseline meets the existing S1 resource budgets on this host for an empty-mod startup: about **+55.1 MiB RSS** and **+70.8 ms to TCP ready** versus the pure-Rust build, below the design thresholds of +60 MiB and +250 ms. This is a minimal server with no loaded mod and a flat fallback world; it does not establish the budget with a real mod set or full NeoForge/FML bootstrap.

## Reproduction and measured results

Repository revision: `01badc14f630b66b50315801e5ff2dde5a7164ee`. Host JDK: OpenJDK `21.0.13.1` (build `2026-11-17`), `/usr/lib/jvm/java-21-openjdk`. The runner uses `-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData`; Rust-launched JVMs receive the same flags through `_JAVA_OPTIONS`. Build outputs, downloaded hash-pinned JARs and trial directories are staged under `target/tmp`.

```sh
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
TMPDIR="$PWD/target/tmp" python3 tools/jvm_eval/bench_jvm_baseline.py
```

The script runs `cargo build --offline --locked --features fabric-probe,neoforge-events -j 1` and `cargo build --offline --locked --no-default-features -j 1`. It builds the bounded NeoForge Java adapter in `target/tmp` from the checked-in Java sources and downloads only manifest-pinned runtime artifacts, verifying each SHA-256 before use. No Cargo network access is needed. It launches three fresh server processes per mode, records process-start to successful loopback TCP connect, samples `/proc/<pid>/status` after 12 seconds in the first trial, and runs a Java harness with three independent JVM processes. The Java harness times `Bootstrap.initialize(empty mods)` and 50,000 warmed S1 event posts per process.

| Measurement | Embedded HotSpot 21 + JNI | Pure Rust | Delta |
|---|---:|---:|---:|
| TCP-ready startup p50 (3 trials) | 81.16 ms | 10.31 ms | +70.84 ms |
| TCP-ready startup min–max | 71.07–81.28 ms | 1.12–10.39 ms | — |
| RSS at 12 s (`VmRSS`) | 70,800 KiB (69.14 MiB) | 14,364 KiB (14.03 MiB) | +56,436 KiB (+55.11 MiB) |
| Anonymous RSS at 12 s | 31,772 KiB | 900 KiB | +30,872 KiB |
| File RSS at 12 s | 39,028 KiB | 13,464 KiB | +25,564 KiB |
| `VmHWM` at 12 s | 70,800 KiB | 14,364 KiB | +56,436 KiB |

`Bootstrap.initialize` measured **10.22 ms** in the first independent JVM process and **9.83 ms median** across the next two independent processes (9.17 and 10.49 ms). “Warm” here means later JVM processes with a likely warm host file cache; it is not a second invocation inside the same JVM. The bootstrap has a one-shot guard, so same-process repeat is intentionally not timed.

The isolated `FarmlandTrampleEvent` dispatch on actual EventBus 8.0.2, with one warmed no-op listener and no JNI boundary or mod handler, measured median-of-three-process **p50 40 ns / p95 60 ns / p99 70 ns** (50,000 samples/process). This is only the Java event-dispatch component. **Production S1 Rust↔Java event round-trip latency: NOT MEASURED. S3 batch latency: NOT MEASURED.** The current tree has a DirectByteBuffer S3 bridge and test fixture; the bridge is still marked dead code until the production tick owner supplies activations.

Current S3 evidence is correctness-only. The NF-4 fixture in [`tests/neoforge_s3_batch_test.py`](../../tests/neoforge_s3_batch_test.py) exercises 1,024 activations through one direct-buffer JNI crossing and checks entity/block-entity handle lifetime, reentrancy rollback and bounds. The current [`S3BatchBridge`](../../src/java_compat.rs) remains `allow(dead_code)` pending production tick-owner activations. ROADMAP records NF-4's prior acceptance result as 62 Python + 10 Rust checks; this lane did not rerun that fixture because it builds into `compat/neoforge-events/.build`, outside this task's allowed write scope. No latency or throughput figure is inferred from that correctness check.

The run logged the normal “worldgen datapack missing” message and continued using the flat fallback. Both server builds used the same directory-isolated startup path and no connected player. TCP-ready means the loopback listener accepted a connection; it does not prove gameplay, persistence, JIT steady-state throughput, FML startup, or arbitrary-mod compatibility. The full structured measurements and artifact digests are emitted to `target/tmp/bench_jvm_baseline.json` by the script and are intentionally not checked in.

## Pinned artifact and bytecode inspection

The runner verified the artifacts below against `compat/neoforge-events/dependencies.json` and `compat/fabric-probe/dependencies.json`; the versioned manifests remain the complete URL/hash source of record.

| Runtime artifact | Pinned version | Verified SHA-256 |
|---|---|---|
| NeoForge universal | 21.1.172 | `02d2bc5bc56a9a60405adbbdeb3b996ecdae82d38f0ff1cec5c9715117c0af47` |
| FML loader | 4.0.39 | `2c5b1386bba0e77c769e95d53eb2edb00773ef3bded5196a44277ade8f112534` |
| EventBus | 8.0.2 | `7ae71dcb9225f3593dca618a668add22b3a1b3f010fdcd746fcb31c3f6d57969` |
| ASM | 9.10.1 | `ed825d10ab1399c8c0cb669e688cf0c8c82629b4c8399b58352b68e92ca10fcb` |
| Fabric Loader | 0.19.5 | `93044e4dd46de5d8136701292f05e868da096d2c9fddb4793e4fdbcc63efc695` |
| Sponge Mixin | 0.17.4 + Mixin 0.8.7 | `1f0ae44db7295f8626f33b1dc0ad7f043d8954a8d6847247875fcc5dfcecc934` |

`javap -p -c -verbose` on EventBus 8.0.2 `net.neoforged.bus.EventBus` shows `invokedynamic` sites linked through `LambdaMetafactory.metafactory` (also `StringConcatFactory`). `EventListenerFactory` contains ASM `ClassWriter`, emits listener bytecode including `ConstantDynamic`, calls `MethodHandles.lookup()`, and defines each generated listener with `MethodHandles.Lookup.defineHiddenClassWithClassData`. This directly exercises invokedynamic, `MethodHandles.Lookup`, hidden runtime class definition and ASM-generated dispatchers. The bytecode scan also finds `ConcurrentHashMap` in `EventBus`.

FML 4.0.39 bytecode references `ModuleLayer` and `ServiceLoader` in loader/discovery classes, including `FMLLoader`, `FMLServiceProvider`, `LaunchContext` and `ModDiscoverer`. Fabric Loader 0.19.5 contains invokedynamic, `ServiceLoader`, and `ClassLoader.defineClass` use in Knot; Sponge Mixin 0.17.4 contains ASM generation and runtime class-definition paths. The NeoForge universal JAR has hundreds of invokedynamic/LambdaMetafactory-using classes. This confirms the required JVM surface is exercised in the pinned libraries, independent of the narrower compatibility shim.

`tools/shimcheck.py` was **not run**: its documented first use builds into `compat/neoforge-events/.build` and `.cache`, both outside the task's permitted write paths. Manifest verification and `javap` inspection were performed from artifacts staged only in `target/tmp`.

## Capability matrix

“Ristretto evidence” below is from its upstream source at commit [`7b6e1379556d2e92d4639cc4198c845fd5e0b469`](https://github.com/theseus-rs/ristretto/commit/7b6e1379556d2e92d4639cc4198c845fd5e0b469), not a local build or a mc-spine runtime test. Its README advertises interpretation for all Java bytecodes including invokedynamic, a parallel JIT, and LTS Corretto runtime classes. The repository contains relevant tests; their presence is not proof that this exact Minecraft stack passes them.

| Required surface | Evidence in pinned dependencies | Ristretto evidence / evaluation |
|---|---|---|
| Java 21 class files and `invokedynamic` | EventBus, NeoForge, FML, Fabric Loader and Mixin contain invokedynamic sites; EventBus links lambdas through `LambdaMetafactory`. | Upstream README claims all bytecodes including invokedynamic; lambda tests exist under `tests/lambda/`. The Ristretto repo declares Java 17/21/25 for `generated_name_lookup`. **Promising; actual EventBus execution unverified here.** |
| `MethodHandles.Lookup`, hidden and ordinary class definition | EventBus ASM dispatcher calls `Lookup.defineHiddenClassWithClassData`; Fabric Knot reaches `ClassLoader.defineClass`; Mixin transforms classes at load time. | Upstream tests cover ordinary `Lookup.defineClass`, hidden class identity and custom class loaders (`tests/class/generated_name_lookup`, `hidden_identity`, `classloader`). **Feature coverage exists; EventBus's exact hidden-class-data path and Mixin transforms remain unverified.** |
| `ModuleLayer` | FML loading/discovery classes reference ModuleLayer and use module-aware launch structures. | Module tests cover boot and child layers (`tests/module/basic`, `layer_hierarchy`, `dynamic_access`). **Basic tests present; ModLauncher/FML graph equivalence not established.** |
| `ServiceLoader` | FML uses service providers in launch/discovery; Fabric Knot and Mixin service classes use it. | Service tests exist (`tests/module/services`); README advertises standard runtime classes. **Common API coverage present; exact launcher provider graph unverified.** |
| `ConcurrentHashMap` | Directly used by EventBus, FML and Fabric Loader bytecode. | Runtime classes are supplied from a Corretto-derived JDK runtime, but no targeted `ConcurrentHashMap` test was found in the inspected test tree. **Expected API availability; Java 21 contention/performance behavior not demonstrated here.** |
| `sun.misc.Unsafe` / `VarHandle` | These pinned JAR scans did not show direct Unsafe/VarHandle references in the listed core classes; modern mods and JDK implementation paths may still require them. | Rust intrinsic sources include `sun.misc.Unsafe` and `VarHandle` operations; tests exist for Unsafe class loading/runtime memory and VarHandle views/indirect access. **Surface appears intentionally implemented; exact Java 21 native-method coverage and mod workloads need execution.** |
| Runtime native methods | Fabric/Mixin and mod ecosystems may call JDK and platform natives beyond these artifacts. | Upstream README explicitly states unimplemented runtime natives fail with `UnsatisfiedLinkError`. **A compatibility blocker unless each required native is implemented or statically rejected before mod load.** |
| JIT and warm-up | HotSpot 21 is the measured reference; its default tiered compilation is enabled. This baseline does not isolate steady-state JIT cost. | Upstream README advertises a parallel JIT, but no comparable Minecraft/event benchmark was found or run. **Performance and warm-up are unknown; do not infer HotSpot parity.** |

Ristretto's current check-run state was queried for the same upstream commit: 174 checks succeeded and 3 were skipped. That is project-level evidence only; it does not identify an executed NeoForge/Fabric compatibility fixture.

## Architecture comparison

| Choice | Compatibility and fail-closed behavior | Startup / RSS | FFI and operations | Decision |
|---|---|---|---|---|
| Embedded HotSpot 21 + existing JNI/shim | Uses the reference JVM behavior for bytecode, method handles, dynamic class definition, modules and services. Current shim scans all JARs before classloading and rejects unsupported surface. Compatibility remains the explicitly supported profile, not arbitrary mods. | Measured empty-mod overhead +70.8 ms / +55.11 MiB; under current S1 budgets. | Existing JNI host and owner-thread callback are wired. S3 has a correctness-tested direct-buffer bridge, but production tick activation and latency evidence remain open. | **Keep for current Phase E.** |
| Pure-Rust Ristretto VM | More of the Java VM/runtime can be implemented in Rust, but the upstream runtime-native limitation and lack of exact loader/Mixin proof leave coverage and fail-closed guarantees unknown. Its Rust embedding API is not the current JNI `RegisterNatives` boundary, so the host bridge needs a concrete port. | No mc-spine startup/RSS numbers; no equivalent build was attempted. | Could avoid the JNI C ABI only after native host functions, owner thread, callbacks, classloader and async/sync semantics are mapped. Boundary cost is unknown. | **No-go as a HotSpot replacement; prototype only after a real compatibility fixture is selected.** |
| GraalVM Native Image / AOT | Closed-world reachability and metadata requirements conflict with arbitrary runtime-loaded mods, Mixin transformations and EventBus generated hidden classes. A constrained image profile might be possible, but would need generated metadata or a different execution contract and explicit rejection. | Not measured for this code or workload. | Could reduce VM startup/runtime footprint in a fixed app; dynamic loader semantics are the main blocker. | **No-go for the current unmodified-mod goal.** |
| Static shim / staged adapter | Narrowest compatibility; exact method/field profile can fail closed before loading. Percentage of arbitrary mods is not a meaningful measured statistic, and full FML is not implemented in mc-spine. | Existing empty-mod HotSpot/JNI cost is within the current S1 resource budget. | Lowest integration risk because it preserves Rust world ownership and current callback shape; direct-buffer S3 transport exists but remains unwired to production ticks. | **Continue, while making each expansion evidence-gated.** |

No compatibility percentage is claimed: there is no representative mod corpus or full FML/Mixin runtime gate in this lane. The EventBus timing is not JNI cost, and the HotSpot startup/RSS result is not a comparison against Ristretto or GraalVM.

## Verification and next discriminator

Completed:

- `TMPDIR="$PWD/target/tmp" python3 tools/jvm_eval/bench_jvm_baseline.py` — passed, emitted the measurements above.
- `cargo build --offline --locked --features fabric-probe,neoforge-events -j 1` — passed inside the benchmark script.
- `cargo build --offline --locked --no-default-features -j 1` — passed inside the benchmark script.
- JDK 21 `javap` inspection and SHA-256 checks for the pinned NeoForge/FML/EventBus/Fabric Loader/Mixin/ASM artifacts — completed.

Repository gates passed on this source:

- `bash tools/layers_check.sh` — PASS (44 modules, 111 edges; current debt named, not resolved).
- `bash tools/debt_ratchet.sh` — PASS (0 over limit).
- `python3 tools/provenance_check.py` — PASS (no tracked/referenced generated provenance assets).

Before commit, the task's required `rm -rf target/tmp` cleanup must remove the generated staging and trial data.

Next decision-changing experiment: run an isolated actual EventBus 8.0.2 + FML 4.0.39 + Fabric Loader 0.19.5/Mixin 0.8.7 fixture on Ristretto Java 21, including the EventListenerFactory hidden-class path, the pinned unmodified Seed Protect JAR, representative `Unsafe`/VarHandle use, and startup fail-closed checks. Only if that passes should a second experiment compare end-to-end S1 callback and S3 batched snapshot/update latency, RSS and startup against this baseline. The S3 wire/handle correctness fixture exists, but production activation and latency evidence are still **NOT RUN**.
