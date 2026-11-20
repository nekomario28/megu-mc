# NeoForge 1.21.1 compatibility bootstrap

This adapter supports the original Seed Protect `HetyfPep` JAR's game-bus registration and farmland cancellation. It uses the NeoForge 21.1.172 `NeoForge` and `BlockEvent.FarmlandTrampleEvent` classes, event bus 8.0.2, and FML 4.0.39 annotation and marker APIs. The adapter supplies a bounded bootstrap and a small set of Minecraft and NeoForge facades.

Rust checks supported player/adult-cow landing eligibility before posting the event, consistent with the pinned `CommonHooks.onFarmlandTrample` source's canTrample-before-post ordering. Java posts the actual event on the shared JVM owner. Cancellation suppresses the guarded native dirt conversion; existing fall damage remains separate. The actual upstream event bus owns priority, cancellation filtering and listener registration. No replacement event engine is implemented.

At startup, `Bootstrap.initialize(directory)` scans `directory/mods/` in filename order and invokes `ShimCheck` on **all** JARs before constructing a class loader or loading any mod class. `game.jar` carries the canonical `net/mcspine/neoforge/shim-surface.json` resource, so startup does not depend on the current working directory. Metadata, supported NeoForge/Minecraft symbols, Mixin restrictions, duplicate mod IDs and required inter-mod dependencies are checked together. Invalid JARs, directories, non-JAR files and unsupported mods fail startup with `ShimCheck rejected NeoForge mod` and specific rejection codes. An empty `mods/` directory starts the event bus in baseline mode.

After the entire mod set passes, a shared `URLClassLoader` loads eligible `@Mod` entrypoints. Each mod ID receives its own `IEventBus`. Supported constructor parameters are `IEventBus` and `Dist`, including either two-parameter order and the no-argument form. `Dist.DEDICATED_SERVER` is injected, and `@Mod(dist = Dist.CLIENT)` entrypoints are excluded. ASM-discovered server-eligible `@EventBusSubscriber` classes are registered on `NeoForge.EVENT_BUS` for the default `Bus.GAME`, or on their respective mod bus for `Bus.MOD`. Client-only subscriber classes are excluded before class loading. The mod buses start for registry events, then the game bus starts after the registry freezes. `Bootstrap.instances()`, `Bootstrap.modBus(modId)` and `Bootstrap.close()` support introspection and isolated testing.

The original pinned Seed Protect `HetyfPep` JAR remains a tested input. Its TOML declares javafml `[1,)`, NeoForge `[21.1,)` and Minecraft `[1.21.1,1.22)`. `ShimCheck` checks the exact symbol surface defined in `shim-surface.json`.

Build with Java 21:

```sh
python3 compat/neoforge-events/build.py --java-home /usr/lib/jvm/java-21-openjdk
cargo build --features fabric-probe,neoforge-events -j 1
```

For a normal server, put statically supported NeoForge mod JARs in its isolated `mods/` directory and set `MC_SPINE_NEOFORGE_CLASSPATH` to the absolute path of `.build/classpath.txt`. Leave `MC_SPINE_FABRIC_CLASSPATH` unset. This opt-in does not replace the server's normal world, networking or save path. Use a separate working directory and world for verification.

`EventChecks` exercises original registration/cancellation/unregister, priority, canceled-event delivery, shutdown/start and mod-bus listener rejection. `tests/neoforge_bootstrap_test.py` verifies multi-mod startup, subscriber routing, constructor injection, dedicated-server filtering and fail-closed rejection. `compat/fabric-probe/server_verify.py --neoforge` verifies normal player packets, adult-cow ticks, damage, saved-world reload and startup rejection across native, Fabric and NeoForge modes. Runtime JAR hashes are retained in the receipt.

Full NeoForge reference-server comparison, FML lifecycle and arbitrary mod compatibility remain NOT RUN. Crop/loot and other entities remain outside the validated landing contract.

Sources: [NeoForge 21.1.172 POM](https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.172/neoforge-21.1.172.pom), pinned upstream source/runtime JARs in dependencies.json, [publisher artifact metadata](https://api.modrinth.com/v2/version/HetyfPep), and [NeoForge 1.21.1 events](https://docs.neoforged.net/docs/1.21.1/concepts/events/).

## NF-0 static compatibility check

Before loading a NeoForge 1.21.1 mod, audit its metadata and bytecode with Java 21
and the pinned ASM 9.10.1 dependency. The first invocation builds the pinned
adapter if needed. The checked-in `shim-surface.json` enumerates supported
classes and exact method and field descriptors.

```sh
python3 tools/shimcheck.py --json path/to/mod.jar
python3 tools/shimcheck.py --surface compat/neoforge-events/shim-surface.json path/to/mod.jar
```

Use `--java-home PATH` to override Java autodetection. The report includes
coverage, client-side exclusions, annotations, rejected symbols and precise
reason codes. Exit 0 means `COMPATIBLE`, exit 2 means `REJECTED`, and exit 1
means command or I/O failure. The check is static and does not start a server.
Mixin transformations and access transformers are unsupported in S0 and fail
closed. `COMPATIBLE` describes the pinned shim surface, not arbitrary Mod
runtime behavior. For tight `/tmp` environments the wrapper uses `target/tmp`.

## NF-3 block and item registration

`DeferredRegister` and direct `RegisterEvent` listeners submit entries for
`Registries.BLOCK` and `Registries.ITEM`. The bootstrap posts the block event to
every mod bus before posting the item event. Duplicate names in the same
registry, invalid locations, and registrations in the reserved `minecraft`
namespace abort startup.

`Bootstrap.frozenRegistryEncoded()` exposes the S2 table to the Rust JVM
owner. Records sort by registry, namespace, then path. The first dynamic block
state ID is 26,684 and the first dynamic item ID is 1,333. Each custom block
has one state. The Rust palette codec saves its canonical `namespace:path`
and resolves that name against the current table on load. Chunk and overlay
save integration, protocol registry synchronization, and reference-server
save compatibility require further integration and verification.

`tests/neoforge_s2_registry_test.py` builds synthetic JARs against the pinned
NeoForge API and tests both load orders, palette remapping after a mod addition,
and rejection of invalid registrations.

## NF-4 direct tick batches

The Rust JVM owner binds a reusable, fixed-capacity request and response pair.
Rust writes directly into the request, Java invokes all matching callbacks in
one JNI dispatch, and Rust validates the entire response before yielding
mutations. Messages borrow UTF-8 from the response, so hold the decoded
iterator only until the caller is ready to reuse the buffers.

S3 v1 uses little-endian frames. The header carries the `S3` magic, version,
direction, record count and nonzero batch sequence. Each request supplies an
activation kind, dimension, handle identity and cell snapshot. Every mutation
identifies its source by request index and generation. The decoder checks both
against the original request and requires an exact frame length. See the
[codec](../../src/landing_types.rs) for field layouts. Tick delays are
nonnegative game ticks and priorities use the signed range −3 through 3.
Container mutations replace slot contents, with item ID and count both zero
for an empty slot.

Mods use `DirectBatchBridge.register` during bootstrap. Each callback receives
a context scoped to its activation on that thread. Handles expire as soon as
the activation returns, even if a later activation has the same identity.
`redispatch` shares the enclosing response and allows up to four callback
frames. Stale handles, excess depth or invalid output invalidate the entire
response, even if a mod catches the exception. Rollback covers response
mutations. A mod can still change its own Java state.

The bridge exchanges snapshots and mutations. The server tick owner needs to
supply activations and apply the validated operations to world state. The
[S3 test suite](../../tests/neoforge_s3_batch_test.py) exercises the actual JNI
entry point with a mod loaded through the NeoForge bootstrap.
