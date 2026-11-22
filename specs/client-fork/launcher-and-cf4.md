# CF-4 — Minecraft 1.21.1 launcher exports and native handoff

The client-fork launcher targets Minecraft **1.21.1**, protocol **767**, and
OpenJDK **21**. The Official Launcher uses a small Java bootstrapper, Prism
Launcher uses an executable native wrapper, and Modrinth .mrpack installs an
optional Fabric/NeoForge Integrated Server bridge.

## Official Launcher

With a writable temporary directory and OpenJDK 21 on PATH:

~~~bash
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
python3 tools/client_launcher/gen_launcher_profile.py \
  --out-dir "$HOME/.minecraft" --build-jar \
  --client-bin /absolute/path/to/megu-client
~~~

This creates the inherited 1.21.1 version JSON and bootstrapper library under
versions/ and libraries/. The generator emits a reproducible Java 21 bootstrapper
JAR below 32 KiB. Its main class forwards --username, --uuid, --accessToken,
--assetIndex, --gameDir, --version and other game arguments to the native client.
The bootstrapper remains running in wait mode until the native process exits.
The optional --megu-detach flag exits immediately after spawn. Prism's
wrapper below supports zero-JVM-runtime startup.

The generator accepts --client-sha256 followed by 64 hexadecimal characters,
which pins megu.clientSha256 in the JVM profile. Its --client-bin flag pins
megu.clientBin. At runtime the bootstrapper also accepts --client-bin,
--client-sha256, MEGU_CLIENT_BIN and MEGU_CLIENT_SHA256. Explicitly
configured missing or non-executable binaries and SHA-256 mismatches abort with
exit code 2 without PATH fallback. These private flags are stripped from the
native client arguments.
An explicit CLI SHA-256 pin takes priority over the JVM profile property, which
in turn takes priority over MEGU_CLIENT_SHA256. An environment variable
cannot replace a configured profile integrity pin.

For a native loopback Integrated Server use --megu-integrated with
--gameDir, MEGU_SERVER_BIN and, when needed, MEGU_SERVER_HOME.
See [CF-3 bridge](shared-kernel-and-integrated.md) for its world lifecycle.

## Prism Launcher instance

Generate directly into the **final** Prism instance directory. WrapperCommand
contains an absolute wrapper path, which is double quoted for spaces:

~~~bash
python3 tools/client_launcher/export_launcher_bundle.py \
  --format prism --out-dir "/path/to/PrismLauncher/instances/megu" \
  --client-bin /absolute/path/to/megu-client
~~~

The instance includes mmc-pack.json with the net.minecraft 1.21.1 component,
instance.cfg with OverrideCommands=true and WrapperCommand set to the generated
executable megu-wrapper. The optional client is copied to
bin/megu-client and pinned by SHA-256 in WrapperCommand. The wrapper strips
the JVM invocation and execs the native executable. Prism still provides the
authenticated game arguments and asset management.

Use --loader fabric --loader-version VERSION or
--loader neoforge --loader-version VERSION to add the respective
net.fabricmc.fabric-loader or net.neoforged component. A loader profile
includes .minecraft/mods/megu-integrated.jar. Supply --integrated-jar PATH
to reuse one, or allow the generator to build it with OpenJDK 21. A
native-only Prism instance can omit loader options.

Direct wrapper usage:

~~~bash
tools/client_launcher/megu-wrapper \
  --client-bin /absolute/path/to/megu-client \
  --client-sha256 SHA256_HEX \
  /usr/lib/jvm/java-21-openjdk/bin/java -cp minecraft.jar \
  net.minecraft.client.main.Main --username Steve --gameDir /path/to/game
~~~

Alternatively set MEGU_CLIENT_BIN, or bundle the client beside the wrapper
under bin/megu-client. With no explicit client it searches PATH. The
MEGU_CLIENT_SHA256 environment variable is accepted as an integrity pin.
Missing binaries and mismatched SHA-256 hashes exit with code 2.

## Modrinth .mrpack

Modrinth packs require an actual compatible Fabric or NeoForge loader version.

~~~bash
python3 tools/client_launcher/export_launcher_bundle.py \
  --format mrpack --out-dir /path/to/export \
  --loader neoforge --loader-version 21.1.215 \
  --integrated-jar /path/to/megu-integrated.jar
~~~

The deterministic ZIP file megu-1.21.1.mrpack has formatVersion 1,
game minecraft, dependencies.minecraft 1.21.1 and the selected loader in
modrinth.index.json. It embeds overrides/mods/megu-integrated.jar.
The standard files array is empty because the mod is bundled as an override
and has no external download. The megu.bundledOverrides extension stores
SHA-1, SHA-512 and byte size for each bundled file. It is an additional local
integrity record, outside the standard Modrinth downloadable-file schema.

ZIP entry timestamps are fixed at 1980-01-01 00:00:00, with deterministic
ordering and UNIX permissions. The --format all option exports the Prism
instance under out-dir/prism/ and .mrpack beside it.

Modrinth import installs the **Java-side mod bridge**. A .mrpack alone cannot
configure a Modrinth App native wrapper. Existing loader hooks also do not
automatically replace Minecraft's single-player UI. Prism's WrapperCommand is
the supported zero-JVM native client entry path.

## Optional greedy mesher library

~~~bash
TMPDIR="$PWD/target/tmp" cargo build \
  --manifest-path client-port/Cargo.toml --release --offline --locked
~~~

Pass --mesh-library client-port/target/release/libmegu_client_mesh.so
when exporting. The generator loads the library and checks the actual
megu_mesh_abi_version() symbol returns version 1. This revision of
client-port exports that function. The longer symbol
megu_client_mesh_abi_version is absent in the current source. The optional
library is placed in prism/native/ or
overrides/megu/native/libmegu_client_mesh.so in the .mrpack with
SHA-1 and SHA-512. The library requires explicit loading by its consumer.

## Credentials, integrity, and verification boundaries

Launcher-generated diagnostics and handoff manifests never print authentication
arguments. Known --accessToken, --clientToken, --authToken, --token and Bearer
credentials are replaced with <REDACTED> if present in an error string.
The native executable receives the original values. Its own stdout/stderr,
arbitrary files it writes, and third-party launcher console logs are under
that program's control, so native code must also avoid logging credentials.
Treat the native client as a trusted executable.

Run the offline checks with a writable temporary directory:

~~~bash
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
cargo build --offline --locked
cargo build --manifest-path client-port/Cargo.toml --release --offline --locked
python3 tests/client_launcher_test.py
python3 tests/client_prediction_wire.py
bash tools/layers_check.sh
bash tools/debt_ratchet.sh
python3 tools/provenance_check.py
git diff --check
~~~

The tests cover byte-identical exports, ZIP metadata and hashes, Prism wrapper
execution, token-safe launcher diagnostics, fail-closed SHA-256 checks, Java
bootstrapper handoff, mesher ABI validation, mock native clients, and a real
protocol-767 status handshake through the loopback bridge. Interactive
Prism/Modrinth App import and in-game world lifecycle are not verified here.

Format references

- Prism custom commands — https://www.prismlauncher.org/wiki/help-pages/custom-commands/
- MultiMC instance format — https://github.com/MultiMC/Launcher/wiki/Export-Instance
- Modrinth .mrpack format — https://support.modrinth.com/en/articles/8802351-modrinth-modpack-format-mrpack
