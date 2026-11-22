# Fabric 1.21.1 S1/S2 runtime adapter

Phase E Wave 4, Lane E4-15. The acceptance target is a clean-room Java 21
dedicated-server probe running synthetic mod JARs. It exercises callback decisions
and Java-local registration in a fresh JVM. The
[Fabric foundation probe](../../compat/fabric-probe/README.md) covers the separate
pinned Seed Protect path through Fabric Loader and megu.

## Admission and startup

The adapter stages private copies of the submitted JARs and checks them with
[FabricShimCheck](../../compat/fabric-probe/java/net/megu/probe/FabricShimCheck.java)
and its [policy driver](../../compat/fabric-probe/fabric_shimcheck.py). The runtime
JAR embeds the [exact symbol surface](../../compat/fabric-probe/shim-surface.json)
for admission. A plain static audit can report S0 compatibility for a legacy
surface or a simple Mixin declaration. Runtime admission requires the selected
namespace and rejects all Mixin and access-widener requests.

The adapter opens one URL class loader per admitted JAR on the staged bytes.
All JARs must pass admission and have valid server entrypoint classes and public
no-arg constructors before any initializer runs. Schema 1 metadata accepts
`server` and `*` on the dedicated server, including the default `*` when
`environment` is omitted. The adapter invokes string-valued `main` entries as
`ModInitializer` and then string-valued `server` entries as
`DedicatedServerModInitializer`. Each group follows JAR argument order and then
metadata array order. It skips `client` entrypoints and refuses to define
excluded client classes. A client-only mod, retained reference to an excluded
class, client-only member on a retained class, or a retained reference to
`class_310` or a rendering API rejects the batch.

The supported dependency declarations cover `fabricloader` profile predicates,
`minecraft` 1.21.1 and `java` 21. Custom adapters, inter-mod dependencies,
nested JARs, JAR manifest classpaths and multi-release classes are rejected.
The JVM executes trusted synthetic code with normal process permissions.
Class loader isolation scopes class lookup and static state.

## Dispatch and freeze

The named build exposes Yarn callback types such as `ActionResult`. The
intermediary build projects the same callback descriptors and Minecraft members
into the 1.21.1 intermediary namespace. Both artifacts expose the Mojang-named
`net.minecraft.core.Registry` and `BuiltInRegistries` bridge. The ABI authority
for the supported subset is the symbol surface. The projection uses
[Yarn 1.21.1+build.3](https://maven.fabricmc.net/net/fabricmc/yarn/1.21.1+build.3/)
and the callback signatures from
[Fabric API 0.116.1+1.21.1](https://maven.fabricmc.net/net/fabricmc/fabric-api/fabric-api/0.116.1+1.21.1/).

- Registry writes run on the bootstrap thread during initialization. The two
  registry facades share block and item entries. Duplicate identifiers are
  rejected and reads are available through either facade. Freeze closes writes
  before command registration and reports IDs in sorted order.
- `CommandRegistrationCallback.EVENT` receives a dedicated registration
  context and a dispatcher that collects literal names in registration order.
  Lifecycle dispatch then runs `SERVER_STARTING`, `SERVER_STARTED`, the
  requested batch and `SERVER_STOPPING`.
- Listeners run in registration order. A `tick` invokes start and end tick
  listeners. Block break stops at the first `BEFORE` callback returning false
  and skips `AFTER` on cancellation. Use block calls `UseBlockCallback.interact`
  through the Fabric `EVENT` field and stops at the first non-`PASS` result.
  `FAIL` cancels the operation. An accepted result handles it. The probe passes
  synthetic positions, a main hand and opaque world and player arguments.
- A successful batch publishes one JSON record with the surface and mod
  digests, executed entrypoints, commands, frozen registry IDs and ordered
  outcomes. The block outcomes have empty mutation lists. Admission rejection
  exits 2 with diagnostics. Input or callback failure exits 1 without publishing
  a partial batch. Fixture logs go to stderr.

## Build and verify

Run from the repository root with OpenJDK 21.

```sh
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
export JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData"

python3 compat/fabric-probe/build.py --java-home /usr/lib/jvm/java-21-openjdk
python3 tests/fabric_shim_test.py
cargo check --offline --locked --tests --features fabric-probe,neoforge-events
bash tools/layers_check.sh && bash tools/debt_ratchet.sh \
  && python3 tools/provenance_check.py && git diff --check
```

To run a submitted synthetic JAR with the named callback ABI, use the command
below. Select `fabric-runtime-intermediary.jar` for intermediary. Pass `-` for
an empty batch or a comma-separated sequence of `tick`, `break:x:y:z` and
`use:x:y:z`.

```sh
/usr/lib/jvm/java-21-openjdk/bin/java -jar compat/fabric-probe/.build/fabric-runtime.jar \
  compat/fabric-probe/fabric_shimcheck.py tick,break:1:64:2,use:1:64:2 /path/to/synthetic-mod.jar
```
