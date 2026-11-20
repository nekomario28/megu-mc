# 1.21.1 mod surface audit, 2026-11-20

Phase E Wave 4, Lane E4-10. Measured implementation HEAD
`ed66761cc7c18f610a192c6f6a4c17f8986770aa`, based on E4-1
`1d104ae83abd3b1172f8ec4ca2f9b3797d18ecd0`. The audit compiles seven synthetic
JARs with OpenJDK 21.0.13.1 and scans them together using the checkout's
`tools/shimcheck.py`, ASM 9.10.1 and NeoForge 21.1.172 surface. The real-world
examples below are source checks used to select patterns. Published mod JARs
and live server behavior were **NOT RUN**.

## Measured scanner results

Each row deduplicates exact JVM members `(owner, name, descriptor)` and
class-only references separately. Members include constructors and fields.
The scanner counts its Minecraft, NeoForge and Brigadier namespaces.
Other diagnostics are reported separately from symbol coverage. Shimcheck
exited 2 for the combined corpus and produced a complete JSON report.

The E4-1 rerun measured 5/3 supported/rejected members for `gameplay`, 6/4
for `automation` and 6/6 for `network`. Block capability lookup, inventory
members, the play registrar and the payload type constructor already passed
at that revision. `implicit_at` passed and the scanner emitted zero
`UNSUPPORTED_MIXIN_AT` diagnostics for `transforms`.

<!-- audit-table:start -->
| Fixture | Shimcheck | Members supported / rejected | Classes supported / rejected | Other diagnostics |
|---|---|---:|---:|---|
| `gameplay` | COMPATIBLE | 8 / 0 | 9 / 0 | — |
| `content` | COMPATIBLE | 12 / 0 | 14 / 0 | — |
| `automation` | REJECTED | 7 / 3 | 14 / 3 | — |
| `network` | REJECTED | 11 / 1 | 16 / 0 | — |
| `transforms` | REJECTED | 1 / 0 | 4 / 0 | `UNSUPPORTED_ACCESS_TRANSFORMER` × 1, `UNSUPPORTED_MIXIN_AT` × 2, `UNSUPPORTED_MIXIN_CONFIG` × 2, `UNSUPPORTED_MIXIN_INJECTION` × 3 |
| `implicit_at` | REJECTED | 0 / 0 | 1 / 0 | `UNSUPPORTED_ACCESS_TRANSFORMER` × 1 |
| `fabric` | REJECTED | 0 / 1 | 0 / 3 | `MISSING_METADATA` × 1 |

2/7 JARs returned `COMPATIBLE`. Across fixture rows, 39 member references were supported and 5 rejected, with 58 supported class references and 6 rejected. Across the corpus, 37 distinct member triples were supported and 5 rejected, with 42 distinct classes supported and 6 rejected.

- Surface SHA-256 `55367c1927df426b7dc4c841d50c9cddeba40e71a7d34b6625c4f911ac7596ec`
- Scanner source SHA-256 `d3ab9f64eaed829495de78267a96b5654cc1c4dd2eb9a2333e2870ef256e6a05`
<!-- audit-table:end -->

## Real-world pattern matrix

| Pattern | Source examples | What the fixture established |
|---|---|---|
| S1 gameplay/protection | Open Parties and Claims wires block break/place and command registration. [NeoForge event adapter](https://github.com/thexaero/open-parties-and-claims/blob/803955bfde9aaa7bbff6026c523ed3128109a88b/NeoForge/src/main/java/xaero/pac/common/event/CommonEventsNeoForge.java) | `gameplay` matched block cancellation, chat, command dispatch, `BlockPos.getY()` and both tested `LivingDamageEvent.Pre` methods. The damage and chat arms extend the sample beyond the linked OPAC calls. |
| S2 blocks, items and ore | Mekanism uses a deferred block registry wrapper for ores and storage blocks. Create registers content via its own Registrate layer. [Mekanism blocks](https://github.com/mekanism/Mekanism/blob/4255aa7156dace1dd379c8bc6d52ab0275bd659c/src/main/java/mekanism/common/registries/MekanismBlocks.java), [Create blocks](https://github.com/Creators-of-Create/Create/blob/37ea4afcfe5bf85875d4b0749798a45330070a1f/src/main/java/com/simibubi/create/AllBlocks.java) | `content` matched direct `DeferredRegister`, `RegisterEvent`, `Registries.BLOCK` and `BlockBehaviour.Properties.of()` references. The fixture's narrow registration path leaves Registrate and mod-specific registry helpers unmeasured. |
| S3 tech and automation | AE2 registers sided BE item handlers. Mekanism supplies item handlers through capability-aware BEs. [AE2 capability registration](https://github.com/AppliedEnergistics/Applied-Energistics-2/blob/67d2a9b081e1b464da8431fbfd48b11a7a36eb92/src/main/java/appeng/init/InitCapabilityProviders.java), [Mekanism capability BE](https://github.com/mekanism/Mekanism/blob/4255aa7156dace1dd379c8bc6d52ab0275bd659c/src/main/java/mekanism/common/tile/base/CapabilityTileEntity.java) | `automation` matched scheduled tick, capability lookup, inventory and related members. BE ticker, dirty tracking and registration by BE type still fail the static gate. |
| S4 networked UI, config and dimensions | Jade uses a registry-aware serverbound request codec. FTB Chunks sends a dimension key in its map-teleport payload. [Jade block request](https://github.com/Snownee/Jade/blob/8d5743b9edb0ed48fc5d65a0410e688a3671abdc/src/main/java/snownee/jade/network/RequestBlockPacket.java), [FTB Chunks teleport payload](https://github.com/FTBTeam/FTB-Chunks/blob/1d40219adef434233f976bc9f71f53d549aad9eb/common/src/main/java/dev/ftb/mods/ftbchunks/net/TeleportFromMapPacket.java) | `network` matched play payload registration, member codecs, the buffer resource-key operations and dimension access. Configuration-phase registration still fails the static gate. |
| Mixin and access transformations | Lithium declares multiple Fabric Mixin configs. FerriteCore redirects and overwrites state-holder behavior. AE2 ships the customary AT file. [Lithium metadata](https://github.com/CaffeineMC/lithium/blob/749edb8e3e2ce48a37e23cd457b97d46671524fb/fabric/src/main/resources/fabric.mod.json), [FerriteCore Mixin](https://github.com/malte0811/FerriteCore/blob/fa7e105e8f21578765c94d9a07e45ddbc606b0db/Common/src/main/java/malte0811/ferritecore/mixin/fastmap/FastMapStateHolderMixin.java), [AE2 AT](https://github.com/AppliedEnergistics/Applied-Energistics-2/blob/67d2a9b081e1b464da8431fbfd48b11a7a36eb92/src/main/resources/META-INF/accesstransformer.cfg) | The scanner rejected both NeoForge configs, the explicit AT, three injection kinds and two injection points in `transforms`. The conventional AT resource also rejected `implicit_at`. The standalone Fabric fixture received `MISSING_METADATA` and four unsupported symbol reasons. |

These source snapshots are representative examples, with no popularity ranking
or per-mod coverage estimate.

## Gaps exposed by exact descriptors

| Fixture | Rejected owner | Member | Descriptor |
|---|---|---|---|
| `automation` | `net/minecraft/world/level/block/entity/BlockEntity` | `setChanged` | `()V` |
| `automation` | `net/minecraft/world/level/block/entity/BlockEntityTicker` | `tick` | `(Lnet/minecraft/world/level/Level;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;Lnet/minecraft/world/level/block/entity/BlockEntity;)V` |
| `automation` | `net/neoforged/neoforge/capabilities/RegisterCapabilitiesEvent` | `registerBlockEntity` | `(Lnet/neoforged/neoforge/capabilities/BlockCapability;Lnet/minecraft/world/level/block/entity/BlockEntityType;Lnet/neoforged/neoforge/capabilities/ICapabilityProvider;)V` |
| `network` | `net/neoforged/neoforge/network/registration/PayloadRegistrar` | `configurationToClient` | `(Lnet/minecraft/network/protocol/common/custom/CustomPacketPayload$Type;Lnet/minecraft/network/codec/StreamCodec;Lnet/neoforged/neoforge/network/handling/IPayloadHandler;)Lnet/neoforged/neoforge/network/registration/PayloadRegistrar;` |
| `fabric` | `net/minecraft/class_1937` | `method_27983` | `()Lnet/minecraft/class_5321;` |

- `hooks.mixins.json` contains HEAD and TAIL injections only and received
  `UNSUPPORTED_MIXIN_CONFIG`. `transforms.mixins.json` additionally produced
  `UNSUPPORTED_MIXIN_INJECTION` for `Redirect`, `ModifyVariable` and
  `Overwrite`, plus exact `UNSUPPORTED_MIXIN_AT` reasons for `INVOKE` and
  `STORE`. A separate ShimCheck regression covers those points inside
  `@Inject(at = {...})` without a config-level diagnostic.
- Both AT fixtures received one `UNSUPPORTED_ACCESS_TRANSFORMER` for
  `META-INF/accesstransformer.cfg`. The TOML declaration and archive entry in
  `transforms` point to the same resource.
- Fabric `@Environment(SERVER)` is present on the server entrypoint. The
  scanner also counted `net/minecraft/class_310` from the
  `@Environment(CLIENT)` class and returned an empty `client_classes` list.
  The Fabric-declared Mixin config produced no config-level diagnostic. Those
  results identify missing profile discovery and side filtering.

Separate adapter tests dispatch the two tick phases and mutable pre-absorption
damage through the NeoForge game bus, decode the S3 schedule proposal with the
Rust codec, and check the S4 level snapshot and dimension payload codec. The
schedule facade accepts the active S3 cell and its S2-registered block. The
dimension decoder accepts the three native dimension keys. Production tick and
damage dispatch, world scheduler application, and published mod runs were
**NOT RUN**.

## Reproduce

Run from the repository root with Java 21 and Python 3. Each run stages its
compiler inputs, JARs and an unmodified copy of the shimcheck wrapper in
`target/tmp`. It compiles the checked-out ASM scanner against the repository
pin. The synthetic JARs use pinned NeoForge/Bus/FML and Fabric Loader/Mixin
compile inputs with local compile-only Minecraft and Brigadier declarations.
No dependency or declaration class is packaged in a fixture JAR. On a clean
checkout the harness downloads SHA-256-pinned dependencies to its scratch
directory. Use `--java-home` to select an alternate JDK.

```sh
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
python3 tools/mod_compat_audit/audit.py --java-home /usr/lib/jvm/java-21-openjdk --format json
python3 tools/mod_compat_audit/audit.py --java-home /usr/lib/jvm/java-21-openjdk --format markdown
TMPDIR="$PWD/target/tmp" python3 tools/mod_compat_audit/test_audit.py
```

JSON includes every supported and unsupported member triple, class-only
reference, metadata resource path and raw scanner reason for each fixture.
Minecraft signatures were checked against the
[1.21.1 server mappings](https://piston-data.mojang.com/v1/objects/03f8985492bda0afc0898465341eb0acef35f570/server.txt)
and Fabric names against the
[1.21.1 intermediary mappings](https://maven.fabricmc.net/net/fabricmc/intermediary/1.21.1/intermediary-1.21.1-v2.jar).
Follow-up work and live reference gates are in the [S4 specification](../../specs/mod-compatibility/s4-network-capabilities-and-fabric.md).
