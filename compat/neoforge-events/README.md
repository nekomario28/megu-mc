# Pinned NeoForge farmland event adapter

This adapter supports the original Seed Protect `HetyfPep` JAR's game-bus registration and farmland cancellation. It uses the actual NeoForge 21.1.172 `NeoForge` and `BlockEvent.FarmlandTrampleEvent` classes, actual event bus 8.0.2, and actual FML 4.0.39 annotation/marker APIs. It does not run FML's full loader, ModLauncher, registry lifecycle or access transformations. The small Minecraft argument classes are opaque shapes required by this mod; they are not general Minecraft implementations.

Rust checks supported player/adult-cow landing eligibility before posting the event, consistent with the pinned `CommonHooks.onFarmlandTrample` source's canTrample-before-post ordering. Java posts the actual event on the shared JVM owner. Cancellation suppresses the guarded native dirt conversion; existing fall damage remains separate. The actual upstream event bus owns priority, cancellation filtering and listener registration. No replacement event engine is implemented.

Only the exact original JAR hash in dependencies.json is accepted. Its known TOML declares javafml `[1,)`, NeoForge `[21.1,)` and Minecraft `[1.21.1,1.22)`. Adapter discovery verifies that pinned artifact and its actual `@Mod("seedprotect")`, then invokes its constructor. This is restricted adapter discovery, not proof of FML loader compatibility. Unknown artifacts and multiple mod files reject startup before networking. One process selects one loader adapter.

Build with Java 21:

```sh
python3 compat/neoforge-events/build.py --java-home /usr/lib/jvm/java-21-openjdk
cargo build --features fabric-probe,neoforge-events -j 1
```

For a normal server, put the pinned original JAR alone in its isolated `mods/` directory and set `MC_SPINE_NEOFORGE_CLASSPATH` to the absolute path of `.build/classpath.txt`. Leave `MC_SPINE_FABRIC_CLASSPATH` unset. This opt-in does not replace the server's normal world, networking or save path. Use a separate working directory and world for verification.

`EventChecks` exercises original registration/cancellation/unregister, priority, canceled-event delivery, shutdown/start and mod-bus listener rejection. `compat/fabric-probe/server_verify.py --neoforge` verifies normal player packets, adult-cow ticks, damage, saved-world reload and startup rejection across native, Fabric and NeoForge modes. Runtime JAR hashes are retained in the receipt.

Full NeoForge reference-server comparison, lifecycle/bootstrap and arbitrary mod compatibility remain NOT RUN. Crop/loot and other entities remain outside the validated landing contract.

Sources: [NeoForge 21.1.172 POM](https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.172/neoforge-21.1.172.pom), pinned upstream source/runtime JARs in dependencies.json, [publisher artifact metadata](https://api.modrinth.com/v2/version/HetyfPep), and [NeoForge 1.21.1 events](https://docs.neoforged.net/docs/1.21.1/concepts/events/).

## NF-0 static compatibility check

Before loading a NeoForge 1.21.1 mod, audit its metadata and bytecode with Java 21
and the pinned ASM 9.10.1 dependency. The first invocation builds the pinned
adapter if needed. The checked-in `shim-surface.json` enumerates currently
supported S0 classes and exact method/field descriptors.

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
