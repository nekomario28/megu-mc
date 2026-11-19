# mc-spine Java mod compatibility

Status: slice 01 passed; slice 02 pinned Redirect and normal player/adult-cow landing passed against a real reference; broader physics/crop coverage pending; arbitrary Minecraft mod compatibility remains NOT RUN.
Target: Minecraft Java 1.21.1, Java 21, Fabric and NeoForge server mods. Client rendering mods are outside this server's execution boundary.

## Ownership and execution

Rust owns world state, ticks, persistence, inventories and networking. Java objects expose operations on those owners through a narrow JNI boundary; they do not maintain a second authoritative world. Execute synchronous game callbacks on the dedicated owning operation thread. Before exposing asynchronous APIs, define thread affinity, reentrancy and shutdown behavior explicitly.

Reuse actual loader and Mixin machinery where possible. Loader-specific lifecycle, registries, events, mappings and access transformations belong in separate adapters. Share concrete world operations once both consumers require them; do not invent a generic loader abstraction before its contracts are known.

Minecraft class names alone do not provide compatibility. A supported mod must receive the expected methods, field behavior, registry identity, event order, exceptions, bytecode targets and persisted effects. Required but unsupported contracts reject loading before normal gameplay. Never silently ignore a required Mixin or dependency.

## Production operation path

When compatibility is enabled, route each supported Rust tick/player action through the corresponding interceptable Java game method, then invoke the native commit only after required callbacks and cancellation checks. A transformed method that normal gameplay never calls provides no compatibility. Java mod API calls must enter that same operation path. The normal player movement and adult-cow tick now use this path. A dedicated JVM owner executes callbacks synchronously; Rust callers wait without holding the world mutex, and the native commit compares the initial farmland state under that mutex. Other entity landing and full crop behavior remain outside the validated contract.

Java world/entity/block-entity facades hold Rust handles and resolve current values through the owner. Registry IDs and object identity must remain stable across callbacks and persistence. Adding custom blocks/items also requires network registry/palette and save-format identity support before registration can be accepted. Direct Java field mutations and arbitrary internal bytecode edits need an explicit synchronization/execution contract; they are deferred rather than treated as equivalent to method-based API calls.

Fabric supplies its entrypoint/dependency/classloading lifecycle; the adapter adds actual Minecraft namespace/API behavior. NeoForge needs a separately pinned bootstrap plus its own mod/game event buses, listener ordering, registration phases and access transformations. It will reuse validated native world commits, not assume Fabric's lifecycle is identical. The event adapter investigation pins NeoForge 21.1.172, FML 4.0.39 API classes and event bus 8.0.2. Full FML startup remains a separate gate.

## Verifiable slices

| Slice | Deliverable | Acceptance |
|---|---|---|
| [01 foundation](slices/01-foundation.md) | Embedded JVM, actual Fabric Loader and Mixin, diagnostic mod | Rust mutation, cancellation, persistence across processes, dependency/invalid injection rejection |
| [02 real Fabric mod](slices/02-seedprotect.md) | Pinned existing server mod JAR, named/intermediary mapping strategy, smallest real Minecraft API facade | Original JAR hash retained; expected game operation and saved state, with required transformations |
| [03 real NeoForge mod](slices/03-neoforge.md) | Pinned 1.21.1 loader bootstrap, lifecycle and registration adapter | Original JAR runs; declared lifecycle/event behavior reaches the same Rust world |
| 04 stateful behavior | Inventories/block entities/tick callbacks required by selected mods | Vanilla reference scenarios, save/reload and exact tick ordering; canceled operations produce no effects |
| 05 compatibility expansion | Explicit per-version supported contract matrix | Each additional contract backed by a mod/scenario; missing contracts visibly rejected |

This is a feature ladder, not a promise that every mod works. API/event mods are the first target. HEAD/RETURN injection is the next bounded target. Redirects, field access, overwrite and bytecode-dependent internal mods require faithful Java execution paths and potentially much larger Minecraft implementation coverage.

## Planning review

Three lenses were applied: smallest end-to-end proof (loader → native state → reload), first failure risks (classloader identity, Mixin target and dependency rejection), and ownership seams (one Rust world, separate loader lifecycle). Independent planning-agent tools were unavailable in this session; these are local design checks, not three external agent reviews.

## Next implementation

Normal Fabric player/adult-cow landing is verified with the original Seed Protect WAKX5fBR JAR, normal TCP/tick entry points, and saved Rust-world reload. Real Vanilla/Fabric reference servers confirm adult-cow dirt conversion versus cancellation and saved MCA state. 18 diagnostic regression arms and 15 native tests pass. Broader collision, crop/loot and all-entity parity are still open.

The bounded NeoForge event adapter now uses actual upstream game-bus and FarmlandTrampleEvent classes with the original Seed Protect HetyfPep JAR. One guarded native commit and JVM owner serve both loaders (with `DimId` scoping). Five normal-server modes and four startup refusals pass, as do real event-bus contract checks. Full FML discovery/bootstrap and a full NeoForge reference remain NOT RUN. Next, close broader collision/crop coverage and proceed along the staged NeoForge S0–S3+ architecture in [neoforge-architecture.md](neoforge-architecture.md) (starting with S0 `shimcheck`).

## Sources

- [Fabric Loader architecture](https://wiki.fabricmc.net/documentation:fabric_loader) and pinned `0.19.5` JAR interfaces.
- [Pinned Fabric dependency metadata](https://meta.fabricmc.net/v2/versions/loader/1.21.1/0.19.5/server/json).
- [Mixin injection points](https://github.com/SpongePowered/Mixin/wiki/Injection-Point-Reference).
- [NeoForge 1.21.1 events](https://docs.neoforged.net/docs/1.21.1/concepts/events/).
