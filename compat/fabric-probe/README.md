# Fabric foundation probe

Diagnostic only: actual Fabric Loader + Mixin execute Java fixtures against mc-spine's existing Rust world. This does not implement the Minecraft API or claim existing Minecraft mod compatibility. Design: `../../specs/mod-compatibility/README.md`.

Use Java 21. Build the fixtures with `python3 compat/fabric-probe/build.py --java-home "$JAVA_HOME"`, then `cargo build --locked --features fabric-probe -j 1`. Run `verify.py --binary <absolute-built-binary> --output <receipt-path>`. Run every build and verification under the resource limits appropriate to the host; local evidence uses MemoryMax=512M, MemorySwapMax=0 and CPUQuota=50%.

Downloaded dependencies are pinned by URL and SHA-256 in `dependencies.json`; cache and generated classes/JARs are ignored. Fixtures are compiled once and loaded without editing their JARs. Mixin legitimately transforms target classes at load time. Required transformation failures are fatal.

For the selected existing Seed Protect contract, append `--seedprotect` to both build.py and verify.py. This adds pinned Yarn name conversion for the host facade and loads the original published mod JAR. `--fabric-trample` exercises only an already-eligible farmland-to-dirt branch; it is not normal server integration or full Vanilla landing semantics. See `../../specs/mod-compatibility/slices/02-seedprotect.md` for the remaining gates.

## Normal server integration

The same pinned original Seed Protect JAR now handles eligible player and adult-cow farmland conversion in normal server startup. Build `--features fabric-probe`; set `MC_SPINE_FABRIC_CLASSPATH` to the absolute path of `.build/classpath-seedprotect.txt` and use the supported original JAR alone in the server's `mods/`. Startup initializes actual Fabric Loader/Mixin and validates the required target before networking begins. Native callers synchronously wait for a dedicated JVM owner; the final Rust commit compares the initial farmland state under its world mutex. Existing fall damage stays separate from conversion cancellation.

`server_verify.py` uses normal TCP play packets and cow ticks, then checks save/reload. Its `--neoforge` option adds the separately built actual NeoForge event adapter; it does not run the full FML loader. Real Vanilla/Fabric reference checks use an adult cow on dry farmland, verify the live result and decode the saved MCA block palette. Player/adult-cow conversion is the supported scope; crop/loot, complete collision and other entities remain open.

## Static compatibility audit

`fabric_shimcheck.py [--surface shim-surface.json] [--java-home PATH] MOD.jar...`
audits only the declared server surface for Minecraft 1.21.1 and the pinned
Fabric Loader 0.15.11/0.19.5 candidates. It reads the exact submitted JARs,
parses their Fabric metadata and declared mixin configs, and scans class
references with ASM. It prints deterministic JSON; exit 0 means every submitted
JAR was admitted, exit 2 means at least one was rejected, and exit 1 means the
gate could not process its inputs. This static gate does not prove loader
startup, runtime callback behavior, or compatibility with Fabric API modules
outside the declared surface. `python3 tests/fabric_shim_test.py` compiles
synthetic JARs and exercises the admission and rejection cases.

## Synthetic S1/S2 runtime

The build also produces standalone Java 21 adapters for the named and
intermediary callback ABIs. See the [Fabric S1/S2 runtime spec](../../specs/mod-compatibility/fabric-s1-s2.md)
for the admission rules, batch format, and verification commands.
