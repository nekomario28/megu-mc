# S4 network, capabilities, and Fabric audit bridge

Status **proposed**, Phase E Wave 3, Lane E3-6. Target Minecraft 1.21.1 protocol
767, Java 21, NeoForge 21.1.172 with FML 4.0.39 and Bus 8.0.2, and explicitly
verified Fabric Loader releases in the 0.15+ line. This specification follows the
[NeoForge architecture](neoforge-architecture.md) and uses the
[Wave 3 audit](../../evidence/mod-compatibility/top-mods-surface-20261120.md) as the
pre-implementation scanner baseline. The [shim plan](shim-plan.md) uses S4 for
the Fabric API subset. Here S4 also names the NeoForge payload and item
capability work needed before claiming the next compatibility profile.

The base checkout replays captured Configuration packets from
`config_packets.bin` in `src/main.rs`. S2 supplies a Java registration snapshot
via `fetch_registry_snapshot` and `ModRegistrySnapshot`. The JVM owner and loader
selection already live in `src/java_compat.rs`. S4 requires a merged, verified S3
callback snapshot/batch ABI before exposing inventory or world mutations through
payload handlers. Treat S3 lane work as a dependency until that verification is
recorded. Read API declarations and compare wire captures from user-provided
reference installs. Write native code and facades locally without copying game
implementations, shipping upstream JARs or projecting their internal state into
the Rust world.

## File and layer ownership

All paths marked **new** are proposed implementation paths.

| Layer | File | Contract |
|---|---|---|
| Rust composition and transport | `src/main.rs` | Own TCP, phases, deadlines and live player lookup. Drive the Configuration task sequence from `handle`, dispatch Play ingress from its existing packet match and send via the current connection writer. Retain the native inventory and BE commit path here while it owns `ContainerKey` and storage. |
| Rust protocol | `src/compat_network.rs` **new** | Own the protocol-767 payload envelope and Configuration plan, negotiated table, loader control frames and bounded channel queues. Read a freeze-time view of registry IDs. Consume and return typed values without sockets or JVM dependencies. |
| Rust registry and tick owners | `src/block_registry.rs`, `src/landing_types.rs`, `src/scheduled_ticks.rs` | Keep the frozen S2 identities and S3 tick schedule authoritative. Derive wire views once per freeze. Project supported inventory reads and writes through the existing S3 types and native commit seam. |
| Rust value and JVM boundary | `src/compat_types.rs` **new**, `src/java_compat.rs` | Value-only types for channel registration, connection epochs, bytes and request/reply. The owner thread marshals registered Java codecs and callbacks, with S3 snapshot and mutation results. Protocol code uses the value types without importing `java_compat`. |
| NeoForge runtime | `compat/neoforge-events/java/net/megu/neoforge/{Bootstrap,ServerHooks}.java`, `compat/neoforge-events/java/net/neoforged/neoforge/{network,capabilities,items}/` | Post registration events on the mod bus, freeze providers/codecs before server readiness and expose the selected API signatures. Use the matching `net/minecraft/network/` and `net/minecraft/world/` facades for bounded buffers, level handles and container views. |
| Fabric runtime | `compat/fabric-probe/java/net/megu/probe/{Bootstrap,SpineGameProvider,RemapFacade}.java`, `compat/fabric-probe/dependencies.json` and `build.py` | Keep the Knot/Loader entrypoint and classloading path. Generate the intermediary facade projection into ignored build output from a verified 1.21.1 mapping pair. Reuse the JVM owner and validated native operations. |
| Static gate | `tools/shimcheck.py`, `compat/neoforge-events/java/net/megu/neoforge/ShimCheck.java`, `compat/neoforge-events/shim-surface.json`, `compat/fabric-probe/shim-surface.json` **new** | One ASM front end and JSON schema with two loader profiles. Share symbol extraction and report serialization. Each profile carries its namespace, version pins, exact descriptors and executable transformation claims. |

Rust dependency direction is `main → compat_network → compat_types` and
`main → java_compat → compat_types`. `compat_types` depends on no higher layer.
Keep `java-compat` opt-in and keep loader and JNI edges out of Vanilla worldgen
and lower layers. Model new nodes in the layer policy in the implementation lane
and verify against the source graph. The default Rust build must keep its
existing packet replay path and runtime behavior.

## 1. Configuration registry data and NeoForge payloads

Use these packet IDs only in the listed protocol-767 state and direction. Each
Custom Payload body is a ResourceLocation string followed by the selected
codec's bytes to the frame boundary. Registry Data uses a dedicated packet body.

| State | Clientbound | Serverbound |
|---|---|---|
| Configuration custom payload | `0x01` | `0x02` |
| Configuration Registry Data | `0x07` | — |
| Configuration Select Known Packs | `0x0e` | `0x07` |
| Configuration Finish | `0x03` | `0x03` |
| Play custom payload | `0x19` | `0x12` |

Freeze S2 registry data and the Java payload registration table before starting
the listener. Key registrations by `(phase, direction, ResourceLocation)`. The
Java facade posts `RegisterPayloadHandlersEvent` exactly once per startup and
implements the audited `registrar(String)`, `configurationToClient/Server` and
`playToClient/Server` signatures, with bidirectional registrations expanded
to two entries. Freeze version, optionality, codec and handler per entry.
Reject duplicate keys, reserved channel collisions and registration after
freeze. Pass `FriendlyByteBuf` to Configuration codecs and bind the frozen
registry view to `RegistryFriendlyByteBuf` for Play codecs. Treat
`ResourceKey<Level>` as a namespaced key in a specified registry. Resolve world
access separately against the native dimension map. Gate this slice on the
three existing dimensions and reject keys for worlds the runtime cannot serve.

The connection's Configuration state follows this ordering.

1. After Login Acknowledged, the NeoForge profile sends the upstream reset and
   initial channel advertisement, the `neoforge:register` query and Configuration
   Ping with ID 0. Accept negotiation traffic while awaiting Pong 0 under the
   existing login deadline. On a NeoForge query response, match phases, flows,
   versions and required/optional channels against the frozen table. Reply on
   `neoforge:network` with the accepted setup. On negotiation failure, send the
   profile's setup failure as applicable and Configuration Disconnect (`0x02`
   clientbound). A client that replies to the ping without a NeoForge query may
   take the other-client path only if the server has no required NeoForge
   channels or content. Check the channel and registration transition against
   the 21.1.172 server patch and capture, including Play registration changes.
2. For negotiated NeoForge clients, run the early frozen registry task before
   Vanilla registry and tag synchronization. Encode
   `neoforge:frozen_registry_sync_start` with its ordered registry-name list,
   the `neoforge:frozen_registry` snapshots and
   `neoforge:frozen_registry_sync_completed` as opaque custom payload bodies
   on clientbound `0x01`. Advance after its matching serverbound completion
   on `0x02`. Take raw IDs from the frozen view, including the S2 block/item IDs
   used by chunk palettes and item stacks. Reject missing required registry
   channels, mismatched keys and stale or duplicate task completion.
3. Send Select Known Packs and decode the serverbound selection against the
   offered set. Emit a `0x07` Registry Data frame for each synchronized
   Vanilla/datapack registry. Its body is a registry identifier, a VarInt count
   and ordered entries of identifier, presence boolean and anonymous NBT when
   present. Omit the NBT only for entries supplied by a mutually selected pack.
   Keep numeric order aligned with native biome, dimension-type and other wire
   tables. Emit tags against the same frozen view. Vanilla's `0x07` stream and
   loader-specific static registry snapshots have separate codecs and share
   their source registry data.
4. Run supported Configuration tasks in order and wait for their declared
   acknowledgments. Initial S4 supports registered payloads and the audited
   sync tasks. A mod that requires arbitrary custom configuration tasks is
   rejected until that API is implemented. Send Finish only when task and
   registry synchronization are complete. The empty serverbound Finish advances
   the connection to Play and permits Join Game (`0x2b`).
5. Route Play custom payloads through the connection's negotiated table. Keep
   `minecraft:brand` on the protocol path and ordinary gameplay packets on
   their existing handlers. Dispatch only the valid phase, direction and
   channel to Java. A well-formed unregistered Vanilla/other-client payload
   may be discarded within the protocol bound. A disallowed modded channel,
   invalid codec body or premature task response closes the connection with a
   phase-appropriate reason.

Keep the current 64 KiB pre-Play frame cap and the Play VarInt21 frame cap.
For the initial profile, cap raw custom payload data at 32,767 bytes
serverbound and 1,048,576 bytes clientbound, subject to the enclosing frame
limit and any tighter codec limits. Those are the 1.21.1 fallback bounds and an
explicit scope limit for registered S4 channels. Reject an unsplittable
registration or registry snapshot that exceeds the profile before serving
worlds. Check frame size before allocation, codec reads against their bounded
slice and decoding through the last byte. Apply the existing login deadline to
Configuration and bound queued payloads by frame count and bytes. Queue
overflow disconnects the affected connection. The connection thread submits an
owned byte slice and a connection token, with no world or socket-write lock
held. The JVM owner invokes the handler once. `IPayloadContext.enqueueWork`
schedules bounded work on that same owner, including inline on-owner execution
where needed to preserve ordering. Stage replies and permitted S3 mutations
until the callback succeeds. Validate the connection epoch and world
preconditions before applying them. Discard outstanding results after
disconnect.

Sources for the framing and reference test inputs are the
[protocol-767 packet table](https://github.com/PrismarineJS/minecraft-data/blob/23303d2350177f2084f94cd38e5d7c8e54dfc8d3/data/pc/1.21.1/proto.yml),
[NeoForge 21.1.172 userdev patches](https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.172/neoforge-21.1.172-userdev.jar)
and [NeoForge 21.1.172 sources](https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.172/neoforge-21.1.172-sources.jar)
(`NetworkRegistry`, `ConfigurationInitialization`, `SyncRegistries`,
`FrozenRegistryPayload` and `RegisterPayloadHandlersEvent`).

## 2. Rust-backed item capability

Start with `Capabilities.ItemHandler.BLOCK` (`neoforge:item_handler`) and the
sided `BlockCapability<IItemHandler, Direction>` contract. The mod-bus
`RegisterCapabilitiesEvent` collects `registerBlockEntity` providers before
freeze. Preserve registration order for a BE type, call matching providers
until one supplies a value and return `null` for an absent capability. Also
expose the selected Rust-owned container providers. The first acceptance case
uses a sided container BE and a mod registration that exposes its guarded
native view. Identify a native view by dimension, block position, BE lifetime
and nullable side. Query only a loaded position or one already in the
callback's snapshot. A replaced BE invalidates previous handles even when the
block and coordinates match. Keep `BlockCapabilityCache`, arbitrary capability
types and item/entity capabilities out of the first surface.

The Java `IItemHandler` and vanilla `Container` facades share the same
callback-local slot view and write intents. Rust's container owner
(`ContainerKey::Block(DimBlockPos)` and existing slot storage) holds the
authoritative stacks. The view includes item ID, count and components,
slot/item limits, sided insert/extract rules and the BE generation plus
inventory revision. Java receives at most one bounded snapshot per callback
batch. Repeated Java reads and simulated operations use that view. Never
expose mutable backing stacks through `getStackInSlot`.

- `insertItem` returns the unconsumed remainder. Respect validity, sided
  access, item/component equality and the smaller of slot and stack limits.
  `extractItem` returns up to the requested amount, or `ItemStack.EMPTY`. With
  `simulate=true`, compute the same answer without changing the working view,
  adding an intent or marking anything dirty.
- Stage real operations in call order so later reads see earlier staged
  changes. On callback completion the native owner validates all touched BE
  generations, revisions, slots, stack limits and side policies under its
  mutation serialization. Commit the batch atomically or discard the entire
  batch and report a conflict. A callback that has already run is never
  reissued automatically. Mark changed storage for save and emit the existing
  viewer/neighbor updates only after successful commit. Exception, stale
  handle and canceled operation paths leave native state untouched.
- Keep the existing BE ticker and `scheduleTick` entry points on the Rust
  scheduler. Release native world locks before running Java. World effects
  enter the same guarded commit path as tick changes. Capability invalidation
  and chunk unload advance the corresponding lifetime or revision. Reject
  queries from an unregistered thread or beyond the scope of the active
  snapshot.

Use NeoForge 21.1.172 source contracts `BlockCapability`,
`RegisterCapabilitiesEvent`, `IItemHandler` and
`ILevelExtension.getCapability` as the differential oracle for ordering,
return values and simulation. [Pinned NeoForge sources](https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.172/neoforge-21.1.172-sources.jar).

## 3. Shared audit front end and Fabric Loader path

Run discovery over the exact JAR bytes that startup will use. Keep one ASM
class visitor and exact JVM-symbol format `(owner, name, descriptor)` with a
separate class-only record. Select the profile via `--loader neoforge|fabric`
or a single unambiguous metadata family. Fail closed for mixed
loader sets in one server, conflicting metadata, duplicate classes/entries,
unverified nested JARs, invalid dependency graphs and unsupported metadata
semantics. CLI and runtime take the same verifier result, surface hash and
sorted JAR digests. Bind the approved bytes to the loader before it starts.
Keep the existing CLI exit contract (0 admitted, 2 rejected, 1 tool/input
failure). Include originating JAR, class or resource, code and exact symbol in
deterministic JSON.

- **NeoForge.** Read `META-INF/neoforge.mods.toml` in Mojang names against
  `shim-surface.json`. Keep the pinned FML annotations and game/mod buses.
  Discover AT declarations and the conventional
  `META-INF/accesstransformer.cfg` regardless of an explicit TOML declaration.
- **Fabric.** Read `fabric.mod.json` schema 1 with loader version predicates,
  dependency closure, server `main`/`server` entrypoints, language adapters,
  `jars`, `mixins`, `accessWidener` and mod environment.
  Reject unsupported adapters and entrypoint types required on server.
  Respect class-level `@Environment(SERVER/CLIENT)` and the environment on
  Mixin configs. For member-level side annotations, reject retained server
  references to stripped members. Retain transitive server-side references in
  descriptors, annotations and bootstrap handles and reject references crossing
  to removed classes. Resolve production symbols in intermediary using the
  exact 1.21.1 mapping projection of the facade. Reject unmapped, wrong
  namespace or ambiguous overloads. In the S4 provider, expose the
  `minecraft` built-in at `1.21.1` so mod version predicates resolve against
  the audited projection. Keep the diagnostic provider's `megu_probe`
  identity for probe runs. Use the real Loader dependency solver and Knot when
  launching the approved original JAR. Validate the first 0.15
  candidate separately from the existing 0.19.5 pin. Candidate pins are
  0.15.11 and 0.19.5, with compatible fixtures obeying their own loader
  predicates. Admit only releases with an explicit pin and passing launch
  evidence. Profile support for Fabric Loader alone grants no Fabric API
  modules. [Fabric metadata format](https://wiki.fabricmc.net/documentation:fabric_mod_json_spec)
  and [0.15.11 versioned Loader source](https://github.com/FabricMC/fabric-loader/tree/0.15.11).
- **Transformations.** Visit nested annotation arrays so `@Inject(at=…)`,
  `@Redirect(at=…)` and `@ModifyVariable(at=…)` report both their annotation
  and injection point. Resolve class/string targets, descriptors and refmaps
  using the matching namespace. Reject unsupported config plugins and dynamic
  target selection. Deny unresolved or unsupported
  Mixin/AT/AW requests before class definition. HEAD/RETURN/TAIL admission
  requires an exact target descriptor and positive evidence the required hook
  runs in a native operation. Preserve the existing Fabric Seed Protect
  Redirect only as the exact known JAR/target/call-site profile, exercised
  through its existing real runtime gate. Default-reject other Redirect,
  ModifyVariable, Overwrite and internal transforms. `COMPATIBLE` is limited
  to the admitted surface and runtime profile.
- **Fabric API networking.** Treat networking and registry sync as a separate
  profile acceptance step after Loader starts. Map the audited Fabric API
  registrations onto the same transport and registry snapshot behind a
  Fabric-specific projection. Where the registered module requires raw-ID
  mapping, emit the negotiated `fabric:registry/sync/direct` task in
  Configuration and wait for serverbound `fabric:registry/sync/complete`
  before Finish.
  Keep vanilla Registry Data on clientbound `0x07`. Admit exact Fabric API
  module versions only after their unmodified client and server are verified.
  [Fabric 1.21.1 registry task](https://github.com/FabricMC/fabric/blob/243a01e83aadfbd7b6c9d17d1e960a7566a0036c/fabric-registry-sync-v0/src/main/java/net/fabricmc/fabric/impl/registry/sync/RegistrySyncManager.java)
  and [direct registry payload](https://github.com/FabricMC/fabric/blob/243a01e83aadfbd7b6c9d17d1e960a7566a0036c/fabric-registry-sync-v0/src/main/java/net/fabricmc/fabric/impl/registry/sync/packet/DirectRegistryPacketHandler.java),
  [completion payload](https://github.com/FabricMC/fabric/blob/243a01e83aadfbd7b6c9d17d1e960a7566a0036c/fabric-registry-sync-v0/src/main/java/net/fabricmc/fabric/impl/registry/sync/SyncCompletePayload.java).

## Acceptance gates in implementation order

| Slice | Evidence required to advance |
|---|---|
| Scanner and startup admission | Build the synthetic JARs and record exact added/removed diagnostic and descriptor rows. Exercise both CLI and production startup with the same JAR set. A known server Fabric class resolves its intermediary owner/member/descriptor and reaches its server entrypoint. Wrong side, missing dependency, overload mismatch, nested injection point, implicit AT and declared AW cases are rejected with named reasons before any entrypoint fires. Re-run the pinned NeoForge and Fabric Seed Protect gates, including the required-target failure. |
| Protocol and registry | TCP harness drives Login → Configuration → Play. Golden frames decode and re-encode byte-for-byte for the registry packet, both custom payload directions in both phases and both meanings of `0x07`. Live trace compares the same snapshot on a real 21.1.172 reference server and megu with a pinned unmodified client. Verify known-pack omission, registry order/IDs, tags, sync completion, a dimension-key payload round trip, first chunk and visible item identity. Test a Vanilla client, optional and missing-required channels, wrong phase/direction, malformed length/codec, missing/duplicate completion and a timed-out handshake. |
| Capability and tick | Through an actual BE ticker and capability consumer, compare lookup side, simulation, partial insert/extract and return stacks to pinned NeoForge. A native hopper or menu observes committed values and process restart reads the same stacks/components. A test touches the same coordinates in two dimensions. Race fault cases include replace/unload, two concurrent writes, callback failure and cancellation. JNI instrumentation counts work by bounded batch and shows no per-slot JNI in the transfer loop. |
| Fabric reference profile | Pin Loader releases, Fabric API modules and original mod JAR hashes. Compare the registry task and payload sequence to a real matching Fabric reference with an unmodified client. Run five selected unmodified server-side Fabric mods through their normal observable feature paths before claiming the broader S4 Fabric API milestone. Report each JAR, loader version, exercised path and verdict; the audit fixtures alone cannot satisfy this gate. |
| Whole-server regression | Run the repo-native no-feature and both feature-specific checks, the audit suite, `tools/layers_check.sh`, `tools/debt_ratchet.sh` and `tools/provenance_check.py`. Record artifact hashes and runtime launch evidence separately from static results. |

Current status for all implementation/runtime gates in this file is **NOT RUN**.
The measured E3-6 deliverable is the scanner audit linked above.
