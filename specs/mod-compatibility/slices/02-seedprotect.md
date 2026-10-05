# 02: Existing Seed Protect JAR and namespace boundary

Status: isolated Redirect contract passed; complete Minecraft/Fabric compatibility NOT PROVEN.

## Selected artifact

Seed Protect, Fabric Minecraft 1.21.1, Modrinth version `WAKX5fBR`, unmodified SHA-256 `ccd48d96239f485846ced57968e73c6040df4a66ca6ea02cb180a29a5cff7a3f`. Pinned download and Yarn/SLF4J inputs: `compat/fabric-probe/seedprotect.json`. No downloaded mod, Minecraft binary or dependency is committed.

Bytecode inspection identifies a required Redirect on `net.minecraft.class_2344.method_9554`, replacing the static invocation `method_10125(Entity, BlockState, World, BlockPos)` with a no-op. The initializer uses actual Fabric's ModInitializer and actual SLF4J; no Fabric API or Architectury implementation is needed by this artifact. Two earlier candidates were not selected: Always Eat requires Architectury; the earlier Seed Protect `2hykIXnG` has mixed target namespaces and a Fabric API dependency. Do not patch either candidate to manufacture a successful original-JAR result.

## Implementation

Five narrowly scoped Java argument/facade classes are authored with Yarn named names. The official pinned Yarn `1.21.1+build.3` Tiny v2 table supplies class mappings and the exact descriptors for `onLandedUpon` (declared by Block) and `setToDirt` (declared by FarmlandBlock). ASM remaps the host classes and their call sites to intermediary at build time. The downloaded mod JAR is never rewritten.

In isolated `--fabric-trample` mode, Rust initializes farmland state 4286 at `(8,70,8)`. The already-eligible conversion branch calls the mapped Java method, which calls the mapped static dirt-conversion method and existing Rust set_block via JNI. The actual mod's required Redirect can replace that invocation. Java owns no block storage. State IDs are checked against native name_to_state before the scenario runs. Owner-thread and persistence protection from slice 01 remain active.

Only this mode advertises Minecraft 1.21.1 to satisfy the selected mod's loader dependency. Scoped startup rejects Fabric system-property overrides, including alternate mod paths and development discovery. An absent mods directory is created for the no-mod baseline. Before loader resolution, every supplied mod file must match the selected original-JAR SHA-256; unknown mod hashes are rejected. This is a scoped dependency contract, not a claim that all Minecraft APIs exist. Ordinary foundation mode continues to reject missing Minecraft dependencies; normal server startup enables this pinned provider only through explicit MC_SPINE_FABRIC_CLASSPATH configuration.

## Acceptance / evidence

- [x] No-mod baseline: farmland → dirt (10), changed=true; fresh process reload=10.
- [x] Original selected JAR: farmland remains 4286, changed=false; fresh process reload=4286.
- [x] Expecting dirt with the mod present fails before persistence.
- [x] Missing method_10125 mapping: no-mod baseline still succeeds; with the mod, required Redirect fails before persistence.
- [x] Unknown mod hash rejected with the specific refusal reason.
- [x] Empty scratch directory starts; additional mod paths and development discovery are specifically rejected.
- [x] Actual loaded mod JAR and original inputs retain their hashes.
- [x] Existing slice 01 scenarios still pass.
- [x] Real Vanilla/Fabric reference run of the equivalent adult-cow landing; saved MCA block palettes confirm the result.
- [x] Normal player movement packets and adult-cow tick landing routed synchronously through a dedicated JVM owner.
- [ ] Full landing eligibility, mobGriefing, entity conditions, damage and crop behavior match the pinned reference.

World, Entity and BlockState arguments are opaque shapes for this Redirect, not general Minecraft implementations. FarmlandBlock implements only the eligible dirt-conversion branch; it does not reproduce Vanilla fall eligibility or Block inheritance. Do not expose this operation as complete onLandedUpon compatibility. No real-client visual result is claimed.

## Next gate and NeoForge handoff

First validate the eligible branch against a real isolated Vanilla/Fabric reference, then bind normal mc-spine landing behavior through the same Java/native commit boundary and compare cancellation and persisted state. The current host resource gate reports two heavy Minecraft occupants; the user explicitly authorized sequential isolated reference servers capped at 1GiB, zero swap, half CPU and 180 seconds per run. The successful reference removes NoAI, forces the test chunk loaded and verifies both live state and saved MCA state. Earlier stationary NoAI attempts are excluded from passing evidence.

Normal startup now routes player movement (position, look and onGround-only packets) and adult-cow landing through src/landing.rs. Fabric callbacks run on a dedicated synchronous owner; native commits compare the initial state under the world mutex. Existing fall damage continues after cancellation. Passive mobs now descend while stationary, and farmland landing height uses 15/16 of a block. The native cow starts at 20 HP versus the reference 10 HP: both lose 3 HP in the measured fall, so absolute health parity is not claimed. Crop/loot, other entities and seed-identical physics remain open.

NeoForge starts after those Fabric gates. A same-feature candidate exists: Modrinth Seed Protect `HetyfPep`, titled 1.0.1 but filename `seedprotect-1.0.0.jar`; the inspected TOML and VERSION field both say 1.0.0. Its constructor registers this on NeoForge.EVENT_BUS, and its subscribed FarmlandTrampleEvent handler calls setCanceled(true). See 03-neoforge.md; exact loader/bootstrap selection still requires source inspection. Keep the native operation owner. NeoForge execution is NOT RUN.

## Primary sources

- [Selected publisher version metadata](https://api.modrinth.com/v2/version/WAKX5fBR).
- [Fabric Yarn FarmlandBlock signatures and namespace table](https://maven.fabricmc.net/docs/yarn-1.21.1+build.3/net/minecraft/block/FarmlandBlock.html).
- [Pinned Yarn artifact](https://maven.fabricmc.net/net/fabricmc/yarn/1.21.1+build.3/yarn-1.21.1+build.3-v2.jar).
- [NeoForge candidate metadata](https://api.modrinth.com/v2/version/HetyfPep).
