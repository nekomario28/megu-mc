# 03: Seed Protect NeoForge event boundary

Status: designed from original artifact bytecode; execution NOT RUN. Start implementation after slice 02 reference and normal-server integration gates pass.

## Original artifact

Publisher version `HetyfPep`, downloaded original SHA-256 `90d3318ce049d2e684b08d12ab6e35410a9d8e159afc43757102ab127c028acf`. The version title says 1.0.1; its TOML and Java VERSION field both say 1.0.0. Pin bytes and actual metadata rather than interpreting the release title as the runtime version. TOML requests javafml loaderVersion `[1,)`, NeoForge `[21.1,)`, Minecraft `[1.21.1,1.22)`; classfile version 65 requires Java 21. The original JAR is not vendored.

## Actual calls / smallest adapter

- Discover `@net.neoforged.fml.common.Mod("seedprotect")` with the selected actual loader/bootstrap contract; parse the original TOML and reject unsupported dependencies.
- Invoke its no-argument constructor, which calls actual `NeoForge.EVENT_BUS.register(this)`.
- Register the `@SubscribeEvent` handler accepting `BlockEvent.FarmlandTrampleEvent`.
- Post the game-bus event on the native operation's owning thread before the Rust dirt conversion; its handler calls `setCanceled(true)`.
- Commit the existing Rust operation only when the event allows it. Share the Rust owner, JNI storage and persistence operation with Fabric; keep Java event dispatch and Fabric Mixin dispatch distinct.

Prefer the pinned upstream event-bus implementation over writing our own listener/priority/cancellation engine. Select exact NeoForge/FML/bus artifacts by inspecting their 1.21.1 bootstrap interfaces and source. Exact loader versions are unresolved here. Do not call reflective construction alone proof of real NeoForge loader integration.

The selected mod registers no items or blocks and uses no custom registry. Implement a registry adapter only when a subsequent selected artifact requires it. Full game/mod-bus lifecycle compatibility remains a separate gate.

## Acceptance

- [ ] Real selected loader discovers metadata and constructs the original mod.
- [ ] No-mod eligible conversion creates dirt, saved and reloaded through Rust.
- [ ] With the original JAR, FarmlandTrampleEvent cancellation preserves farmland across restart.
- [ ] Suppressing registration makes the protected-result check fail; unknown artifact/dependency is rejected with a specific reason.
- [ ] Event order, cancellation and delivery to canceled-event listeners follow the pinned event-bus contract.
- [ ] Equivalent real NeoForge reference scenario agrees, including the separation of fall damage and dirt conversion.
- [ ] Original JAR hash remains unchanged.

## References

[Publisher metadata](https://api.modrinth.com/v2/version/HetyfPep), inspected `com.seedprotect.SeedProtectMod` bytecode and `META-INF/neoforge.mods.toml`, and [NeoForge 1.21.1 event registration/cancellation contract](https://docs.neoforged.net/docs/1.21.1/concepts/events/).
