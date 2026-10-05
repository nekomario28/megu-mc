# mc-spine Java mod compatibility

Status: slice 01 diagnostic acceptance passed; arbitrary Minecraft mod compatibility remains NOT RUN.
Target: Minecraft Java 1.21.1, Java 21, Fabric and NeoForge server mods. Client rendering mods are outside this server's execution boundary.

## Ownership and execution

Rust owns world state, ticks, persistence, inventories and networking. Java objects expose operations on those owners through a narrow JNI boundary; they do not maintain a second authoritative world. Execute synchronous game callbacks on the owning game thread. Before exposing asynchronous APIs, define thread affinity, reentrancy and shutdown behavior explicitly.

Reuse actual loader and Mixin machinery where possible. Loader-specific lifecycle, registries, events, mappings and access transformations belong in separate adapters. Share concrete world operations once both consumers require them; do not invent a generic loader abstraction before its contracts are known.

Minecraft class names alone do not provide compatibility. A supported mod must receive the expected methods, field behavior, registry identity, event order, exceptions, bytecode targets and persisted effects. Required but unsupported contracts reject loading before normal gameplay. Never silently ignore a required Mixin or dependency.

## Production operation path

When compatibility is enabled, route each supported Rust tick/player action through the corresponding interceptable Java game method, then invoke the native commit only after required callbacks and cancellation checks. A transformed method that normal gameplay never calls provides no compatibility. Java mod API calls must enter that same operation path. The foundation probe demonstrates this direction for one diagnostic method; normal-server integration is still future work.

Java world/entity/block-entity facades hold Rust handles and resolve current values through the owner. Registry IDs and object identity must remain stable across callbacks and persistence. Adding custom blocks/items also requires network registry/palette and save-format identity support before registration can be accepted. Direct Java field mutations and arbitrary internal bytecode edits need an explicit synchronization/execution contract; they are deferred rather than treated as equivalent to method-based API calls.

Fabric supplies its entrypoint/dependency/classloading lifecycle; the adapter adds actual Minecraft namespace/API behavior. NeoForge needs a separately pinned bootstrap plus its own mod/game event buses, listener ordering, registration phases and access transformations. It will reuse validated native world commits, not assume Fabric's lifecycle is identical. Exact NeoForge artifact/bootstrap selection is unresolved until slice 03 source inspection.

## Verifiable slices

| Slice | Deliverable | Acceptance |
|---|---|---|
| [01 foundation](slices/01-foundation.md) | Embedded JVM, actual Fabric Loader and Mixin, diagnostic mod | Rust mutation, cancellation, persistence across processes, dependency/invalid injection rejection |
| 02 real Fabric mod | Pinned existing server mod JAR, named/intermediary mapping strategy, smallest real Minecraft API facade | Original JAR hash retained; expected game operation and saved state, with required transformations |
| 03 real NeoForge mod | Pinned 1.21.1 loader bootstrap, lifecycle and registration adapter | Original JAR runs; registry/event behavior reaches the same Rust world |
| 04 stateful behavior | Inventories/block entities/tick callbacks required by selected mods | Vanilla reference scenarios, save/reload and exact tick ordering; canceled operations produce no effects |
| 05 compatibility expansion | Explicit per-version supported contract matrix | Each additional contract backed by a mod/scenario; missing contracts visibly rejected |

This is a feature ladder, not a promise that every mod works. API/event mods are the first target. HEAD/RETURN injection is the next bounded target. Redirects, field access, overwrite and bytecode-dependent internal mods require faithful Java execution paths and potentially much larger Minecraft implementation coverage.

## Planning review

Three lenses were applied: smallest end-to-end proof (loader → native state → reload), first failure risks (classloader identity, Mixin target and dependency rejection), and ownership seams (one Rust world, separate loader lifecycle). Independent planning-agent tools were unavailable in this session; these are local design checks, not three external agent reviews.

## Next implementation

Complete slice 01 evidence first. For slice 02 choose and pin one existing small server mod, inspect its actual bytecode/API calls, and build only the required facade. Compare a real Vanilla/Fabric reference run with the mc-spine run before calling it compatible. Resolve mappings and target bytecode before advertising `minecraft` as a provided dependency. For slice 03 inspect the pinned NeoForge bootstrap and lifecycle contracts; do not assume Fabric ordering applies.

## Sources

- [Fabric Loader architecture](https://wiki.fabricmc.net/documentation:fabric_loader) and pinned `0.19.5` JAR interfaces.
- [Pinned Fabric dependency metadata](https://meta.fabricmc.net/v2/versions/loader/1.21.1/0.19.5/server/json).
- [Mixin injection points](https://github.com/SpongePowered/Mixin/wiki/Injection-Point-Reference).
- [NeoForge 1.21.1 events](https://docs.neoforged.net/docs/1.21.1/concepts/events/).
