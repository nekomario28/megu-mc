# 1.21.1 mod surface audit, 2026-11-20

Phase E Wave 3, Lane E3-6. Scanner input checkout
`5759cbef5e5bf9f809365c6b2a4cc2fd9d9d57a9`. The audit compiles seven synthetic
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

<!-- audit-table:start -->
| Fixture | Shimcheck | Members supported / rejected | Classes supported / rejected | Other diagnostics |
|---|---|---:|---:|---|
| `gameplay` | REJECTED | 5 / 3 | 8 / 1 | — |
| `content` | COMPATIBLE | 12 / 0 | 14 / 0 | — |
| `automation` | REJECTED | 0 / 10 | 6 / 11 | — |
| `network` | REJECTED | 3 / 9 | 6 / 10 | — |
| `transforms` | REJECTED | 0 / 1 | 4 / 0 | `UNSUPPORTED_ACCESS_TRANSFORMER` × 1, `UNSUPPORTED_MIXIN_CONFIG` × 2, `UNSUPPORTED_MIXIN_INJECTION` × 3 |
| `implicit_at` | COMPATIBLE | 0 / 0 | 1 / 0 | — |
| `fabric` | REJECTED | 0 / 1 | 0 / 3 | `MISSING_METADATA` × 1 |

2/7 JARs returned `COMPATIBLE`. Across fixture rows, 20 member references were supported and 24 rejected, with 39 supported class references and 25 rejected. Across the corpus, 18 distinct member triples were supported and 24 rejected, with 23 distinct classes supported and 25 rejected.

- Surface SHA-256 `f7cb7a534e36173d49741be6df8754a8a7bf8b855f2fb7685ce13e8ca0c6ffec`
- Scanner source SHA-256 `75c58aa1e29bd347720905f3760c367da1e3f653bead55475a8dd772d6fc255f`
<!-- audit-table:end -->

## Real-world pattern matrix

| Pattern | Source examples | What the fixture established |
|---|---|---|
| S1 gameplay/protection | Open Parties and Claims wires block break/place and command registration. [NeoForge event adapter](https://github.com/thexaero/open-parties-and-claims/blob/803955bfde9aaa7bbff6026c523ed3128109a88b/NeoForge/src/main/java/xaero/pac/common/event/CommonEventsNeoForge.java) | `gameplay` matched block cancellation, chat and command dispatch members. It rejected `BlockPos.getY()I` and both tested `LivingDamageEvent.Pre` members. The damage and chat arms extend the sample beyond the linked OPAC calls. |
| S2 blocks, items and ore | Mekanism uses a deferred block registry wrapper for ores and storage blocks. Create registers content via its own Registrate layer. [Mekanism blocks](https://github.com/mekanism/Mekanism/blob/4255aa7156dace1dd379c8bc6d52ab0275bd659c/src/main/java/mekanism/common/registries/MekanismBlocks.java), [Create blocks](https://github.com/Creators-of-Create/Create/blob/37ea4afcfe5bf85875d4b0749798a45330070a1f/src/main/java/com/simibubi/create/AllBlocks.java) | `content` matched direct `DeferredRegister`, `RegisterEvent`, `Registries.BLOCK` and `BlockBehaviour.Properties.of()` references. The fixture's narrow registration path leaves Registrate and mod-specific registry helpers unmeasured. |
| S3 tech and automation | AE2 registers sided BE item handlers. Mekanism supplies item handlers through capability-aware BEs. [AE2 capability registration](https://github.com/AppliedEnergistics/Applied-Energistics-2/blob/67d2a9b081e1b464da8431fbfd48b11a7a36eb92/src/main/java/appeng/init/InitCapabilityProviders.java), [Mekanism capability BE](https://github.com/mekanism/Mekanism/blob/4255aa7156dace1dd379c8bc6d52ab0275bd659c/src/main/java/mekanism/common/tile/base/CapabilityTileEntity.java) | `automation` rejected all ten tested members across registration/lookup, `IItemHandler`, `Container`, the BE ticker and scheduled ticks. |
| S4 networked UI, config and dimensions | Jade uses a registry-aware serverbound request codec. FTB Chunks sends a dimension key in its map-teleport payload. [Jade block request](https://github.com/Snownee/Jade/blob/8d5743b9edb0ed48fc5d65a0410e688a3671abdc/src/main/java/snownee/jade/network/RequestBlockPacket.java), [FTB Chunks teleport payload](https://github.com/FTBTeam/FTB-Chunks/blob/1d40219adef434233f976bc9f71f53d549aad9eb/common/src/main/java/dev/ftb/mods/ftbchunks/net/TeleportFromMapPacket.java) | `network` matched `ResourceLocation.fromNamespaceAndPath`, `ResourceKey.location` and the bus listener registration. It rejected the nine tested members for the registrar, payload codec/buffer and dimension access. |
| Mixin and access transformations | Lithium declares multiple Fabric Mixin configs. FerriteCore redirects and overwrites state-holder behavior. AE2 ships the customary AT file. [Lithium metadata](https://github.com/CaffeineMC/lithium/blob/749edb8e3e2ce48a37e23cd457b97d46671524fb/fabric/src/main/resources/fabric.mod.json), [FerriteCore Mixin](https://github.com/malte0811/FerriteCore/blob/fa7e105e8f21578765c94d9a07e45ddbc606b0db/Common/src/main/java/malte0811/ferritecore/mixin/fastmap/FastMapStateHolderMixin.java), [AE2 AT](https://github.com/AppliedEnergistics/Applied-Energistics-2/blob/67d2a9b081e1b464da8431fbfd48b11a7a36eb92/src/main/resources/META-INF/accesstransformer.cfg) | The scanner rejected both NeoForge configs, the explicit AT and three injection kinds in `transforms`. It admitted `implicit_at` with the same AT resource and no TOML declaration. The standalone Fabric fixture received `MISSING_METADATA` and four unsupported symbol reasons. |

These source snapshots are representative examples, with no popularity ranking
or per-mod coverage estimate.

## Gaps exposed by exact descriptors

| Fixture | Rejected owner | Member | Descriptor |
|---|---|---|---|
| `gameplay` | `net/neoforged/neoforge/event/entity/living/LivingDamageEvent$Pre` | `setNewDamage` | `(F)V` |
| `automation` | `net/minecraft/world/level/LevelAccessor` | `scheduleTick` | `(Lnet/minecraft/core/BlockPos;Lnet/minecraft/world/level/block/Block;I)V` |
| `automation` | `net/neoforged/neoforge/items/IItemHandler` | `insertItem` | `(ILnet/minecraft/world/item/ItemStack;Z)Lnet/minecraft/world/item/ItemStack;` |
| `network` | `net/neoforged/neoforge/network/event/RegisterPayloadHandlersEvent` | `registrar` | `(Ljava/lang/String;)Lnet/neoforged/neoforge/network/registration/PayloadRegistrar;` |
| `network` | `net/minecraft/world/level/Level` | `dimension` | `()Lnet/minecraft/resources/ResourceKey;` |
| `fabric` | `net/minecraft/class_1937` | `method_27983` | `()Lnet/minecraft/class_5321;` |

- `hooks.mixins.json` contains HEAD and TAIL injections only and received
  `UNSUPPORTED_MIXIN_CONFIG`. `transforms.mixins.json` additionally produced
  `UNSUPPORTED_MIXIN_INJECTION` for `Redirect`, `ModifyVariable` and
  `Overwrite`. A `javap -v` check confirmed `INVOKE` and `STORE` in the
  compiled annotation arrays. The scanner issued zero `UNSUPPORTED_MIXIN_AT`
  reasons for those arrays.
- The explicitly declared `META-INF/accesstransformer.cfg` received
  `UNSUPPORTED_ACCESS_TRANSFORMER`. The conventional-file-only fixture's
  `COMPATIBLE` verdict records a detector gap. No access transformation was
  applied.
- Fabric `@Environment(SERVER)` is present on the server entrypoint. The
  scanner also counted `net/minecraft/class_310` from the
  `@Environment(CLIENT)` class and returned an empty `client_classes` list.
  The Fabric-declared Mixin config produced no config-level diagnostic. Those
  results identify missing profile discovery and side filtering.

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
python3 tools/mod_compat_audit/audit.py --format json
python3 tools/mod_compat_audit/audit.py --format markdown
TMPDIR="$PWD/target/tmp" python3 tools/mod_compat_audit/test_audit.py
```

JSON includes every supported and unsupported member triple, class-only
reference, metadata resource path and raw scanner reason for each fixture.
Minecraft signatures were checked against the
[1.21.1 server mappings](https://piston-data.mojang.com/v1/objects/03f8985492bda0afc0898465341eb0acef35f570/server.txt)
and Fabric names against the
[1.21.1 intermediary mappings](https://maven.fabricmc.net/net/fabricmc/intermediary/1.21.1/intermediary-1.21.1-v2.jar).
Follow-up work and live reference gates are in the [S4 specification](../../specs/mod-compatibility/s4-network-capabilities-and-fabric.md).
