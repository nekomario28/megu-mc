# Fabric foundation probe

Diagnostic only: actual Fabric Loader + Mixin execute Java fixtures against mc-spine's existing Rust world. This does not implement the Minecraft API or claim existing Minecraft mod compatibility. Design: `../../specs/mod-compatibility/README.md`.

Use Java 21. Build the fixtures with `python3 compat/fabric-probe/build.py --java-home "$JAVA_HOME"`, then `cargo build --locked --features fabric-probe -j 1`. Run `verify.py --binary <absolute-built-binary> --output <receipt-path>`. Run every build and verification under the resource limits appropriate to the host; local evidence uses MemoryMax=512M, MemorySwapMax=0 and CPUQuota=50%.

Downloaded dependencies are pinned by URL and SHA-256 in `dependencies.json`; cache and generated classes/JARs are ignored. Fixtures are compiled once and loaded without editing their JARs. Mixin legitimately transforms target classes at load time. Required transformation failures are fatal.
