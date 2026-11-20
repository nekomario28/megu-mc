# 1.21.1 mod surface audit, 2026-11-20

Phase E Wave 4, Lane E4-24 (NF-7), based on E4-10
`0e903f0`. The audit compiles seven synthetic
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
E4-10 measured 8/0 for `gameplay`, 12/0 for `content`, 7/3 for
`automation`, and 11/1 for `network`. E4-24 closed the four remaining
NeoForge member gaps.

<!-- audit-table:start -->
| Fixture | Shimcheck | Members supported / rejected | Classes supported / rejected | Other diagnostics |
|---|---|---:|---:|---|
| `gameplay` | COMPATIBLE | 8 / 0 | 9 / 0 | — |
| `content` | COMPATIBLE | 12 / 0 | 14 / 0 | — |
| `automation` | COMPATIBLE | 10 / 0 | 17 / 0 | — |
| `network` | COMPATIBLE | 12 / 0 | 16 / 0 | — |
| `transforms` | REJECTED | 1 / 0 | 4 / 0 | `UNSUPPORTED_ACCESS_TRANSFORMER` × 1, `UNSUPPORTED_MIXIN_AT` × 2, `UNSUPPORTED_MIXIN_CONFIG` × 2, `UNSUPPORTED_MIXIN_INJECTION` × 3 |
| `implicit_at` | REJECTED | 0 / 0 | 1 / 0 | `UNSUPPORTED_ACCESS_TRANSFORMER` × 1 |
| `fabric` | REJECTED | 0 / 1 | 0 / 3 | `MISSING_METADATA` × 1 |

4/7 JARs returned `COMPATIBLE`. Across fixture rows, 43 member references were supported and 1 rejected, with 61 supported class references and 3 rejected. Across the corpus, 41 distinct member triples were supported and 1 rejected, with 45 distinct classes supported and 3 rejected.

- Surface SHA-256 `af6f6d1fb7a093cd1cbb8e282519b7af32ac6a9fc89939212875f0c68cdada8c`
- Scanner source SHA-256 `d3ab9f64eaed829495de78267a96b5654cc1c4dd2eb9a2333e2870ef256e6a05`
<!-- audit-table:end -->

## Real-world pattern matrix

| Pattern | Source examples | What the fixture established |
|---|---|---|
| S1 gameplay/protection | Open Parties and Claims wires block break/place and command registration. [NeoForge event adapter](https://github.com/thexaero/open-parties-and-claims/blob/803955bfde9aaa7bbff6026c523ed3128109a88b/NeoForge/src/main/java/xaero/pac/common/event/CommonEventsNeoForge.java) | `gameplay` matched block cancellation, chat, command dispatch, `BlockPos.getY()` and both tested `LivingDamageEvent.Pre` methods. The damage and chat arms extend the sample beyond the linked OPAC calls. |
| S2 blocks, items and ore | Mekanism uses a deferred block registry wrapper for ores and storage blocks. Create registers content via its own Registrate layer. [Mekanism blocks](https://github.com/mekanism/Mekanism/blob/4255aa7156dace1dd379c8bc6d52ab0275bd659c/src/main/java/mekanism/common/registries/MekanismBlocks.java), [Create blocks](https://github.com/Creators-of-Create/Create/blob/37ea4afcfe5bf85875d4b0749798a45330070a1f/src/main/java/com/simibubi/create/AllBlocks.java) | `content` matched direct `DeferredRegister`, `RegisterEvent`, `Registries.BLOCK` and `BlockBehaviour.Properties.of()` references. The fixture's narrow registration path leaves Registrate and mod-specific registry helpers unmeasured. |
| S3 tech and automation | AE2 registers sided BE item handlers. Mekanism supplies item handlers through capability-aware BEs. [AE2 capability registration](https://github.com/AppliedEnergistics/Applied-Energistics-2/blob/67d2a9b081e1b464da8431fbfd48b11a7a36eb92/src/main/java/appeng/init/InitCapabilityProviders.java), [Mekanism capability BE](https://github.com/mekanism/Mekanism/blob/4255aa7156dace1dd379c8bc6d52ab0275bd659c/src/main/java/mekanism/common/tile/base/CapabilityTileEntity.java) | `automation` matched scheduled ticks, capability lookup, inventory, block entity tickers, dirty marking and capability registration by block entity type. |
| S4 networked UI, config and dimensions | Jade uses a registry-aware serverbound request codec. FTB Chunks sends a dimension key in its map-teleport payload. [Jade block request](https://github.com/Snownee/Jade/blob/8d5743b9edb0ed48fc5d65a0410e688a3671abdc/src/main/java/snownee/jade/network/RequestBlockPacket.java), [FTB Chunks teleport payload](https://github.com/FTBTeam/FTB-Chunks/blob/1d40219adef434233f976bc9f71f53d549aad9eb/common/src/main/java/dev/ftb/mods/ftbchunks/net/TeleportFromMapPacket.java) | `network` matched play and clientbound configuration payload registration, member codecs, the buffer resource-key operations and dimension access. |
| Mixin and access transformations | Lithium declares multiple Fabric Mixin configs. FerriteCore redirects and overwrites state-holder behavior. AE2 ships the customary AT file. [Lithium metadata](https://github.com/CaffeineMC/lithium/blob/749edb8e3e2ce48a37e23cd457b97d46671524fb/fabric/src/main/resources/fabric.mod.json), [FerriteCore Mixin](https://github.com/malte0811/FerriteCore/blob/fa7e105e8f21578765c94d9a07e45ddbc606b0db/Common/src/main/java/malte0811/ferritecore/mixin/fastmap/FastMapStateHolderMixin.java), [AE2 AT](https://github.com/AppliedEnergistics/Applied-Energistics-2/blob/67d2a9b081e1b464da8431fbfd48b11a7a36eb92/src/main/resources/META-INF/accesstransformer.cfg) | The scanner rejected both NeoForge configs, the explicit AT, three injection kinds and two injection points in `transforms`. The conventional AT resource also rejected `implicit_at`. The standalone Fabric fixture received `MISSING_METADATA` and four distinct unsupported symbols. |

These source snapshots are representative examples, with no popularity ranking
or per-mod coverage estimate.

## Member gaps measured in E4-10

| Fixture | Owner | Member | Descriptor | E4-24 |
|---|---|---|---|---|
| `automation` | `net/minecraft/world/level/block/entity/BlockEntity` | `setChanged` | `()V` | Supported |
| `automation` | `net/minecraft/world/level/block/entity/BlockEntityTicker` | `tick` | `(Lnet/minecraft/world/level/Level;Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/state/BlockState;Lnet/minecraft/world/level/block/entity/BlockEntity;)V` | Supported |
| `automation` | `net/neoforged/neoforge/capabilities/RegisterCapabilitiesEvent` | `registerBlockEntity` | `(Lnet/neoforged/neoforge/capabilities/BlockCapability;Lnet/minecraft/world/level/block/entity/BlockEntityType;Lnet/neoforged/neoforge/capabilities/ICapabilityProvider;)V` | Supported |
| `network` | `net/neoforged/neoforge/network/registration/PayloadRegistrar` | `configurationToClient` | `(Lnet/minecraft/network/protocol/common/custom/CustomPacketPayload$Type;Lnet/minecraft/network/codec/StreamCodec;Lnet/neoforged/neoforge/network/handling/IPayloadHandler;)Lnet/neoforged/neoforge/network/registration/PayloadRegistrar;` | Supported |
| `fabric` | `net/minecraft/class_1937` | `method_27983` | `()Lnet/minecraft/class_5321;` | Rejected |

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

Separate adapter tests dispatch block entity tickers through the Java S3
batch, confirm dirty flags on the active S3 and S4 views, and reject stale
views. The S4 JNI fixture resolves item capabilities registered against the
canonical S2 block ID. The Java S4 dispatch test round-trips Configuration
replies and rejects mismatched phases, oversized payloads and excess depth.
Dirty flags are scoped to the Java views. Production tick and damage dispatch,
world scheduler application, Configuration transport, and published mod runs
were **NOT RUN**.

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
