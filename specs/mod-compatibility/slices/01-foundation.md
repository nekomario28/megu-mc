# 01: Loader, transformation and Rust world boundary

## Contract

Explicit `--fabric-probe` startup, behind optional Cargo feature `fabric-probe`, runs in a new empty scratch directory. It must not start network listeners, initialize a full server or overwrite an existing `world_overlay.bin`. The Java heap is limited to 128 MiB. Execution is additionally scoped to 512 MiB, zero swap, half a CPU during local verification.

Actual Fabric Loader `0.19.5` discovers a diagnostic game through `GameProvider`, resolves mod dependencies and invokes a compiled mod's initializer. JNI natives are registered on the `Host` class returned by the actual Knot target classloader, not a parent duplicate. Calls from worker threads throw before any world read or mutation. The diagnostic ABI accepts only position `(8,70,8)` and block states `1` or `2`. Invalid values throw Java exceptions; Rust panics must not unwind across FFI.

The initializer writes state `1` to the existing Rust world. Without a guard, the game method changes it to `2`. A required actual Mixin HEAD injection cancels that game call, returning false and retaining state `1`. The existing production `save_world` writes the result; a second Rust process uses the existing `load_world` and verifies it. No Java shadow world exists.

## Acceptance

- [x] API fixture: state 2, changed=true, fresh-process reload=2.
- [x] Guard fixture: state 1, changed=false, fresh-process reload=1.
- [x] Required injection targeting a missing method: nonzero exit, no persisted world.
- [x] Missing Minecraft dependency: nonzero exit, no persisted world.
- [x] Wrong expected state: rejection, no persisted world (test discriminator).
- [x] Existing temporary saves and symlink targets: reject without changing bytes.
- [x] Existing world: reject without changing bytes.
- [x] Fixture and dependency JAR hashes unchanged after execution.
- [x] Default Rust tests and feature-enabled build pass.

## Evidence boundary

Fixtures are authored here against a diagnostic `net.megu.probe.Host` API. They exercise the actual loader and transformation mechanism but are not pre-existing Minecraft mods. No Minecraft classes, Fabric API, intermediary mapping, NeoForge runtime, real client, tick timing or broad Mixin compatibility is proven. `megu_probe` is the only game dependency advertised; pretending that `minecraft` is provided would hide missing contracts.

## Files / handoff

`compat/fabric-probe/build.py` builds hash-verified dependencies and deterministic fixture JARs. `src/java_compat.rs` owns diagnostic startup, the normal JVM owner and the shared guarded native bindings. `src/main.rs` intercepts explicit probe arguments before ordinary startup; world logic stays in existing functions. The regression harness records both positive and failure arms, resource configuration and artifact hashes. Normal integration retains the restricted diagnostic ABI and permits only the active, supported landing operation to commit a matching farmland state. Fabric and NeoForge share native registration and that world commit.
