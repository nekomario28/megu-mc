# Current implementation choices

## Bounded and still provisional

- Run the selected NeoForge mod through a restricted event adapter using its actual annotation, bus and event classes. Full FML discovery/bootstrap stays separate because this mod only requires constructor registration and event cancellation. Expanding to another mod requires an explicit lifecycle/API contract.
- Keep Minecraft World/Entity/BlockState arguments opaque for these exact artifacts. Their handlers do not inspect those objects. The restricted artifact hash prevents advertising these shapes as a general API to arbitrary mods.
- Validate player/adult-cow farmland conversion first. Preserve the existing native damage model and report the cow's initial-health difference rather than interpreting a matching damage delta as complete entity parity. Crop/loot and all-entity physics still need separate evidence.

## Settled for this operation

- Rust remains the sole world owner. Both loaders use the same JNI registration, state guard and existing persistence path; Java stores no second world.
- A dedicated JVM thread executes synchronous callbacks. Network/tick callers wait without holding the world mutex, and the final commit compares the original farmland state while holding that mutex. This protects an intervening block replacement without exposing unrestricted Java block mutation.
- Use actual upstream Fabric/Mixin and NeoForge event-bus behavior. Loader adapters choose the appropriate Java operation; they do not implement another listener-priority or cancellation engine.
- Reject unsupported artifacts and required transformation failures before opening normal listeners. Run exactly one Java adapter per process.
