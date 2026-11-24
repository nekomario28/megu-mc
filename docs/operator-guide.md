# Operator guide

Megu ships **two separate programs**. They share a codebase, but they are
not the same binary and they do not share a directory or a config file:

- **`megu`** is the dedicated server. It owns the world, listens for
  players, and is the only one you expose to the network.
- **`megu-client-gpu`** is the native GPU client (Vulkan / DX12 / Metal).
  It runs on the player's machine, connects out to a server, and also
  hosts singleplayer worlds by launching a loopback `megu` process.

If you only remember one sentence: **run `megu` on the server, run
`megu-client-gpu` on the player's PC.** `megu-client` is a third,
headless/CLI client used for status probing and automation, not for
windowed play.

## Dedicated server vs. native GPU client

| | **Dedicated server (`megu`)** | **Native GPU client (`megu-client-gpu`)** |
| :--- | :--- | :--- |
| **Job** | Headless game server: world ticks, players, saves, admin commands | Windowed renderer + game client; singleplayer hosts a loopback `megu` |
| **Runs on** | Server, VPS, or a spare PC | The player's Windows / macOS / Linux machine |
| **Game port** | TCP **25599** by default; pass `--port 25565` for the usual port. Also `[server] port`, `MEGU_PORT`, `server-port` | No inbound game port. `--connect <host[:port]>` defaults to **25565** on the remote server |
| **Bind address** | `127.0.0.1` by default; a public bind requires `MEGU_ALLOW_PUBLIC_BIND=1` | Outbound only; singleplayer binds an ephemeral loopback port |
| **World / data** | The server directory (`--dir`, default cwd): `world_overlay.bin`, `world_overlay_nether.bin`, `world_overlay_end.bin`, `backups/`, `mods/`, `datapacks/` | `--gameDir` (default cwd): `saves/<world>/` for singleplayer, plus `versions/`, `mods/`, `resourcepacks/`, `megu-client.toml`, `options.txt`; assets live under `--assetsDir` |
| **Config** | `server.toml` (`[server]`, `[anticheat]`) → `server.properties` (Vanilla keys, read-only) → `MEGU_*` env (wins) | `megu-client.toml` (or imported `options.txt`), launcher args, `MEGU_*` env |
| **Mods** | `mods/*.jar` scanned when `MEGU_MODS_DIR` is set (release builds include the mod suite) | `<gameDir>/mods/*.jar` for textures/recipes and for the integrated host |
| **Admin surface** | Console, RCON (opt-in), `/megu …`, `/transfer`, `/profile` | In-game UI only; no server administration |
| **Start** | `./megu --port 25565` | `./megu-client-gpu --connect play.example.net:25565` |

The **client is never a server**: do not port-forward to it, and do not
copy `server.toml` into a client folder. The **server has no window**: run
it in a terminal, a service manager, or the provided container.

### The two Megu client binaries

| Binary | Use it for |
| :--- | :--- |
| `megu-client-gpu` | The real game client: window, rendering, live multiplayer, singleplayer. |
| `megu-client` | Headless/CLI client and status probe (`megu-client --ping host:25565`); also the home of crash-report flags and integrated-host launcher plumbing. |

`megu-installer` is setup only. It stages binaries and registers a
launcher profile; it is **not** required before opening
`megu-client-gpu`, because the GPU client bootstraps its own Vanilla
1.21.1 assets (see below).

## Quick start: dedicated server (3 steps)

### 1. Download and lay out the directory

Use either route — both create a self-contained server folder:

```sh
# Route A: the installer (from a release archive or tools/install_linux.sh)
./megu-installer server --dir ./megu-server

# Route B: the full archive + the initializer
./megu-init --dir ./megu-server
```

`megu-init` creates `world/`, `mods/`, `datapacks/`, `backups/`,
`server.toml`, `eula.txt`, `ops.json`, and `whitelist.json`. It does not
write `server.properties`.

### 2. Accept the Minecraft EULA

Read <https://aka.ms/MinecraftEULA>, then edit `./megu-server/eula.txt`:

```properties
eula=true
```

`megu` refuses to boot with exit code 2 when a present `eula.txt` is not
accepted. The container image requires `EULA=TRUE`. A **missing**
`eula.txt` is allowed for first boots and repository development runs.

### 3. Run it

```sh
cd ./megu-server
./megu --port 25565      # omit --port to listen on 25599
```

At the console, `status` prints TPS/players/chunks/memory and `stop`
saves and exits. `server.toml` is re-read on Vanilla `/reload`.

### Which config file holds what

| File | Holds | Notes |
| :--- | :--- | :--- |
| `server.properties` | Vanilla keys: `server-port`, `server-ip`, `motd`, `max-players`, `white-list` / `enforce-whitelist`, `difficulty`, `gamemode`, `view-distance`, `simulation-distance`, `online-mode`, `allow-flight`, `pvp`, `rcon.port`, `rcon.password`, `op-permission-level`, `player-idle-timeout`, `network-compression-threshold`, `level-seed` | Read if present, **never created**; lowest priority. Drop in a file from an existing Vanilla server and its recognized keys are honored. |
| `server.toml` | Megu extensions. `[server]`: `port`, `bind_addr`, `motd`, `max_players`, `seed`, `compression_threshold`, `allowlist`, `idle_timeout_secs`, `login_timeout_secs`, `backup_interval_secs`, `backup_max_keep`, `max_conn_per_ip`, `resource_pack_url`, `resource_pack_required`, `rcon_port`, `online_mode`, `velocity_forwarding`, `velocity_forwarding_secret`, `autosave_interval_secs`, `lan_broadcast`, `auto_load_balance`, `target_version`, `version_pack_dir`. `[anticheat]`: `enabled`, `antixray`, `hide_underground`, `antifullbright`, `check_ops`, `preset`, `alerts`, `exempt`, `exempt_ops`, `ops_bypass`, `anti_xray` | Optional; written with comments on first boot. Unknown tables/keys are reported and skipped. There is **no `[backup]` table** — backup knobs are the `[server] backup_*` keys. |
| `MEGU_*` environment | Same settings plus runtime-only knobs (`MEGU_ALLOW_PUBLIC_BIND`, `MEGU_RCON_PASS`, `MEGU_ANVIL_WORLD_DIR`, …) | **Highest priority**; overrides both files. `MEGU_CONFIG` points at a different `server.toml`. |

Settings precedence is **environment → `server.toml` → `server.properties`**.
The full environment table is in §3.

## Quick start: client (zero manual steps)

```sh
# Multiplayer
./megu-client-gpu --connect play.example.net:25565

# Singleplayer (launches a loopback megu automatically)
./megu-client-gpu --quickPlaySingleplayer MyWorld
```

Or launch the **Megu (1.21.1)** profile that `megu-installer` registered;
the client also accepts the standard launcher arguments (`--gameDir`,
`--assetsDir`, `--assetIndex`, `--version`, `--username`, `--uuid`,
`--accessToken`, `--quickPlayMultiplayer`, …).

**You do not need to run `megu-installer` in a terminal, or download
assets by hand, before the first window opens.** On first launch the
client:

1. Scans, in order, `<gameDir>/versions/1.21.1/1.21.1.jar`, the Official
   Launcher's `.minecraft` (native and Flatpak), the PrismLauncher library
   cache, the Modrinth App cache, and Megu's own cache
   (`~/.cache/megu`, `~/Library/Caches/megu`, or `%LOCALAPPDATA%\megu`).
   The first readable ZIP wins — an existing 1.21.1 install is reused
   instantly for textures, models, and title branding.
2. If no local JAR exists, fetches the Mojang version manifest, then the
   version JSON, then the client JAR **in the background**. The chain is
   HTTPS-only against Mojang's official hosts, streamed in 64 KiB chunks
   to a `.part` file, checked against the manifest's SHA-1 and the ZIP
   magic, and atomically renamed only after verification. Progress is
   shown on the title line; textures and branding hot-reload when it is
   ready.
3. Shows built-in placeholder branding while a download is in flight, so
   the window opens immediately instead of blocking on the network.

Set `MEGU_NO_AUTO_FETCH=1` (or `MEGU_OFFLINE=1`) to disable the network
fetch. The client never needs Java. Client settings live in
`<gameDir>/megu-client.toml`; if that file is absent, an existing Vanilla
`options.txt` is imported once.

## Quick start: modded servers

Release builds ship the `mod-suite` and `create-offload` features. Put
Fabric / NeoForge JARs in `mods/`, then point the standalone server at
them (the integrated client host does this automatically):

```sh
MEGU_MODS_DIR=./mods ./megu --port 25565

# Pre-flight a pack or folder without a JVM:
./megu-mod-check --scan-mods-dir ./mods
```

- **Jar-in-Jar.** Embedded JARs under NeoForge's `META-INF/jarjar/` and
  Fabric's nested-JAR layout are extracted recursively (depth, count, and
  total-byte caps) and their `data/` and `assets/` content is folded into
  the adapter, so bundled libraries are not silently ignored.
- **Create 6.x / Create: Aeronautics.** The zero-JVM `create-adapter`
  covers Create 6.x-generation kinetics, belts, contraptions, factory
  processing, and Aeronautics-style 6-DOF rigid-body ships with a
  deterministic `AERO` save codec. Addon profiles are data-driven rather
  than one Rust implementation per addon.
- **Anvil compatibility.** `--import-anvil <dir>` / `--export-anvil <dir>`
  convert a Vanilla 1.21.1 world once; `MEGU_ANVIL_WORLD_DIR=<dir>` keeps a
  Vanilla/NeoForge world live. Root NBT the server does not interpret
  (`Heightmaps`, `structures`, `neoforge:attachments`, `ForgeData`,
  `fabric:attachments`, `block_ticks`, …) is preserved and merged back on
  re-encode. Palette entries the registry cannot resolve (mod blocks)
  decode as air but keep a per-cell **tombstone** of the original
  `{Name, Properties}` compound, so a load → save cycle restores the mod
  block instead of erasing it.
- **Cross-mod interop.** `UnifiedTagIndex` maps `#c:` and `#forge:` tag
  namespaces onto one index, and `BlockCapabilityBus` exposes item, fluid,
  and energy capabilities across mods — all in-process, no JVM bridge.
- **Honest limits.** Arbitrary Java code and Mixin execution still need
  the JVM bridge; `megu-mod-check` reports JARs that need inspection
  instead of pretending they ran. Create support targets the 6.x
  (1.21.1) generation, and Aeronautics support is the native 6-DOF
  adapter, not a promise that every addon version runs.

## Quick start: operator commands and safety

```text
/transfer <hostname> [<port>] [<players>]        # level 3; native 1.21.1 transfer (0x73)
/megu policy list|get <key>|set <key> <value>|reset   # level 4
/megu audit [player] [radius]                    # level 4; live block audit ring
/megu rollback <player> <radius> [max_blocks]    # level 4
/megu claim here|unclaim|trust <player>|untrust <player>|info
/megu claim admin-unclaim|spawn-radius <n>|spawn-protect <n>   # level 4
/megu backup plan|status                         # level 4; no external plugin
/megu metrics                                    # level 4; Prometheus text exposition
```

- **`/transfer`** sends the Vanilla clientbound transfer packet. It
  defaults to port **25565** and targets **`@s`** when no player selector
  is given; the host must be a printable, non-space ASCII hostname.
- **Velocity modern forwarding** is opt-in: set
  `MEGU_VELOCITY_FORWARDING=1` and a shared `MEGU_VELOCITY_SECRET`. The
  proxy must answer `velocity:player_info` with an HMAC-SHA256 identity
  signed by that secret. Forwarding is checked **before** online mode;
  never stack both, and firewall the backend so only the proxy reaches it.
- **`/megu metrics`** prints a Prometheus 0.0.4 text exposition
  (`megu_tps`, MSPT percentiles, players, chunks, faults, RSS, uptime).
  There is no built-in HTTP scrape listener — capture the command output
  from the console, RCON, or your process supervisor.
- **`/megu backup plan|status`** reports the next rotation and existing
  `backups/world*.bak` generations. Rotation itself is off by default
  (`backup_max_keep = 0`); set it in `[server]` and **test a restore**
  before relying on it.

Safety posture, in one paragraph: the server binds loopback and runs
without authentication by default. Opening it to the internet requires
`MEGU_ALLOW_PUBLIC_BIND=1` (which logs a warning) **plus** real identity
— `MEGU_ONLINE_MODE=1` or a Velocity proxy — and preferably an allowlist
and a firewall. RCON stays off unless both `MEGU_RCON_PORT` and a
nonempty `MEGU_RCON_PASS` are set. See
[../SECURITY_NOTES.md](../SECURITY_NOTES.md) before exposing anything.

## The full binary set

Seven primary operator-facing binaries ship in this repository, alongside
`megu-admin` for the legacy offline administration commands:

| binary | built from | job |
|---|---|---|
| `megu` | root package ([../src/main.rs](../src/main.rs)) | the game server |
| `megu-init` | [../server-files](../server-files) | server directory bootstrap; dispatches legacy subcommands to `megu-admin` |
| `megu-admin` | [../server-files](../server-files) | offline server-file administration (`init`, `doctor`, `op`, `claims`, etc.) |
| `megu-mod-check` | [../mod-adapter-core](../mod-adapter-core) | modpack pre-flight checker |
| `megu-world-migrate` | [../state-store](../state-store) | inspect and migrate world data between 1.21.1 and 26.3 |
| `megu-client` | [../client-app](../client-app) | native client; `--ping` doubles as a status probe |
| `megu-client-gpu` | [../client-gpu](../client-gpu) | windowed wgpu client: live play, integrated singleplayer host, zero-manual Vanilla JAR bootstrap |
| `megu-voice-relay` | [../proto](../proto) | zero-dependency `VCR1` UDP proximity voice relay sidecar |

The server and the tools target two Minecraft versions: `1.21.1`
(protocol 767, DataVersion 3955) and `26.3` (protocol 777, DataVersion
5023). What each piece does with them is stated where it is used; §7 is
honest about today's coverage.

---

The numbered sections below are the detailed reference: setup edge cases,
modpack pre-flight, every runtime knob, Docker, world migration, voice
relay, alpha caveats, and the optional report relay.

## 1. First-time server setup

Direct `megu-init --dir` creates `world/`, `mods/`, `datapacks/`,
`backups/`, `server.toml`, `eula.txt`, `ops.json` and `whitelist.json`.
The defaults are `--mc-version 1.21.1` and `--port 25565`; `26.3` is
also a tooling target. The initializer preserves existing files unless
`--force` is passed and does not write `server.properties`. Read the
Minecraft EULA before using `--accept-eula`; without that flag, a new
`eula.txt` contains `eula=false`.

```sh
megu-init --dir /srv/spine --mc-version 1.21.1 --port 25565 --accept-eula
megu-init --dir /srv/spine-263 --mc-version 26.3 --accept-eula
megu-init --dir /srv/spine --dry-run --json
```

Direct `--dry-run` plans without writing files; `--json` emits the
initialization report. To accept an existing `eula=false` with this
initializer, use `--force --accept-eula` and review the overwrite plan
first: `--force` also replaces other generated configuration files.

**One-command onboarding.** The Python launcher bootstraps a local server
directory, checks the EULA and optional modpack, and starts `megu` only
when the pre-flight succeeds. Review the Minecraft EULA before opting in.

```sh
python3 tools/megu_start.py --dir ./server --mc-version 1.21.1 --accept-eula
python3 tools/megu_start.py --dir ./server --modpack pack.mrpack \
    --accept-eula --lang auto --dry-run --json
python3 tools/megu_start.py --dir ./server-263 --mc-version 26.3 \
    --accept-eula --dry-run --json
```

`megu_start.py` uses `1.21.1` by default and delegates initialization
to a built or PATH-resolved `megu-init` when available, after its
Python EULA and `server.properties` setup. `--lang` accepts `auto`,
`en`, or `ja`. `--json` emits a
`megu-start-plan/1` plan; `--dry-run` avoids launching the server while
still initializing `eula.txt` and `server.properties` in the chosen
directory. A modpack is checked, not downloaded or installed. A plan
with `action_items` must be resolved before starting the server.

Legacy `megu-init init|doctor|op|deop|ban|pardon|whitelist|claims`
commands dispatch to the sibling `megu-admin` executable. Install
both binaries together to use these offline administration commands;
the legacy `init` subcommand has its own options.

```sh
# Read-only pre-flight. Exit 0 = READY, 2 = EULA missing/not accepted,
# 1 = a server file is unreadable or corrupt. rcon.password is always
# redacted in the report.
megu-init doctor --dir /srv/spine

# Offline administration (all take --json for machine output).
megu-init op --dir /srv/spine <player> --level 4
megu-init deop --dir /srv/spine <player>
megu-init whitelist --dir /srv/spine on
megu-init whitelist --dir /srv/spine add <player>
megu-init whitelist --dir /srv/spine list
megu-init ban --dir /srv/spine <player> --reason "griefing"
megu-init pardon --dir /srv/spine <player>
```

`op` defaults to the directory's `op-permission-level` (else 4);
`--bypass-limit` sets `bypassesPlayerLimit`. `ban` records an offline-UUID
entry; `--reason`, `--created` and `--source` have fixed defaults the
`--help` text lists.

The `claims` subcommand administers the offline OpenPAC/FTB-Chunks-style
claims store (`openpac_claims.json` / `openpac_parties.json`, written
atomically beside the admin files). Only those two files are read and
written — `eula.txt` and the admin lists are never touched. Dimensions
are normalized like any registry id (`overworld` and
`minecraft:overworld` are the same chunk key), and chunk coordinates
may be negative.

```sh
# List every claim (optionally --dimension <dim>), plus the forceload
# count; parties prints parties with members, allies and invites.
megu-init claims --dir /srv/spine list [--json]
megu-init claims --dir /srv/spine parties

# Spawn-protection style server claim on chunk (3, -2), force-loaded.
megu-init claims --dir /srv/spine server-claim overworld 3 -2 --forceload

# Drop any claim on that chunk as admin.
megu-init claims --dir /srv/spine unclaim overworld 3 -2
```

## 2. Modpack pre-flight

**Modrinth installer.** The bundled `tools/megu_mods.py` searches Modrinth,
installs a chosen mod with required dependencies, or resolves missing
dependencies of installed JARs:

```sh
python3 tools/megu_mods.py search sodium --mc-version 1.21.1
python3 tools/megu_mods.py add <slug> --mods-dir ./mods --dry-run
python3 tools/megu_mods.py add <slug> --mods-dir ./mods --yes
python3 tools/megu_mods.py resolve-deps --mods-dir ./mods --dry-run
python3 tools/megu_mods.py resolve-deps --mods-dir ./mods --yes
```

Installs require explicit interactive consent or `--yes`. `--dry-run`
downloads nothing. Downloads use the exact HTTPS `cdn.modrinth.com`
allowlist and verify the declared size plus SHA-512 and/or SHA-256 (whichever
hashes Modrinth provides) before atomic installation. Built-in superseded
performance mods are skipped by default; `add --force` overrides the skip.

Before a pack is downloaded or launched, `megu-mod-check` reports
whether it can run on this server. It accepts a Modrinth `.mrpack`
archive, a standalone `modrinth.index.json`, or a CurseForge
`manifest.json` / pack `.zip`.

```sh
megu-mod-check --modpack pack.mrpack --mc-version 1.21.1
megu-mod-check --modpack pack.mrpack --mc-version 26.3
```

The report says which files run natively, which client-only files are
skipped on a dedicated server, which Java JARs still need a bytecode
inspection, and which redundant performance mods are skipped
automatically. `--flagship <ids>` names mod ids handled by a flagship
native adapter (default `create`); `--profiles <ids>` names ids covered
by declarative offload profiles; `--json` prints the machine report.

A mod whose only unsupported surface is a fully liftable Mixin set —
standard `@Inject` HEAD/TAIL/RETURN sites on vanilla lifecycle methods —
is promoted from Tier 3 to Tier 2 with exact event bindings by the
static lifter in
[../mod-adapter-core/src/mixin_lifter.rs](../mod-adapter-core/src/mixin_lifter.rs).
The lifter is fail-closed: `@Overwrite`/`@Redirect`/`@Modify*`/
`@Accessor`/`@Invoker`, `locals` capture, non-trivial injection points,
and unknown targets all keep the mod at Tier 3 — it never guesses.

**Scan an installed `mods/` folder without a JVM.** This inspects the
bytecode of each JAR, detects Farmer's Delight and compatible culinary
recipes/tags from addons, and skips the 36 built-in superseded performance
mod ids before scanning. Safe `@Inject` sites can be lifted from Tier 3 to
Tier 2; other hooks remain flagged for inspection.

```sh
megu-mod-check --scan-mods-dir ./mods --json
megu-mod-check --scan-mods-dir ./mods --include-client-mixins --json
```

Client mixins are excluded by default on dedicated servers. For directory
scans, exit `0` means no JAR requires Tier 3, `2` means at least one does
(or invalid arguments), and `1` means the directory or JAR could not be
read or parsed.

**Automatic adaptation.** Inspect an individual JAR or an installed mods
directory and optionally write native bundle candidates and a report:

```sh
megu-mod-check --adapt-jar ./mods/example.jar --out ./adapted --json
megu-mod-check --adapt-mods-dir ./mods --out ./adapted --json
megu-mod-check --scaffold-native examplemod --out ./examplemod.bundle.json
python3 tools/mod_launcher.py --mods-dir ./mods --auto-adapt --dry-run --json
python3 tools/mod_launcher.py --adapt-mods-dir ./mods --dry-run --json
```

`--adapt-jar` and `--adapt-mods-dir` classify each JAR as an automatic
skip, native bundle, Tier 2 shim or Tier 3 fallback with explicit
blockers. With `--out`, adaptation writes `<mod_id>.bundle.json` and
`adaptation-report.json`. `--scaffold-native` writes a starter
`megu-mod-bundle/1` document. The launcher uses the same scan for
startup planning; `--adapt-mods-dir` implies `--auto-adapt`. These paths
do not establish compatibility for every Java mod. For the bundle format
and supported extension points, see [mod-author-guide.md](mod-author-guide.md).

The same pre-flight also runs entirely in the browser: [../website](../website)
is a zero-build static page (no bundler, no CDN) whose dropzone accepts a
`.mrpack` or pack `.zip`, extracts the manifest client-side with a
bounds-checked ZIP reader, and applies the same classifier and superseded
perf-mod catalog — no bytes leave the tab. `website/check_site.py`
verifies the page offline.

Exit codes: `0` the pack can run (natively, with automatic skips, or
pending a bytecode inspection), `2` a version mismatch or bad arguments,
`1` an unreadable or unsafe pack.

**About the automatic skips.** `sodium`, `lithium`, `modernfix`,
`distanthorizons`, `ferritecore` and similar performance mods are never
loaded: `megu` already ships equivalent optimization categories as
its own original clean-room Rust code. The skip list is a mod-id
blocklist — `BUILTIN_SUPERSEDED_PERF_MODS` in
[../mod-adapter-core/src/fallback.rs](../mod-adapter-core/src/fallback.rs)
— so no third-party mod code is copied, linked, bundled or derived from.
Skipping those ids does not affect the rest of the pack.

The same classifier is available as a Python tool —
`python3 tools/mod_launcher.py --modpack pack.mrpack --mc-version 1.21.1`
— which is what the container entrypoint runs (§4).

**Optional bounded Java delegate runtime.** The pure-Rust `.class` subset
VM handles only verified, allowlisted instructions. For Tier 3/invasive
Java hooks, an operator may prepare a headless JRE from their own JDK 21+
using [`build_mini_jre.py`](../tools/build_mini_jre.py):

```sh
python3 tools/build_mini_jre.py --out ./mini-jre \
    --profile neoforge-delegate --xms-mib 32 --xmx-mib 256 --dry-run --json
python3 tools/build_mini_jre.py --out ./mini-jre \
    --profile neoforge-delegate --xms-mib 32 --xmx-mib 256
```

`minimal` is the smaller profile. The output path must be new, and the
tool installs nothing until invoked. A built JRE is a delegate runtime
component; a real third-party modpack run still requires Wave 3 E2E proof.

## 3. Running the server

Start it in the server working directory; the world saves to
`world_overlay.bin` (plus per-dimension companions) there.

```sh
cd /srv/spine
megu            # or: MEGU_PORT=25565 megu
```

Settings resolve in three layers, highest wins: **environment →
`server.toml` → a dropped-in Vanilla `server.properties`** (the last one
is read, never created). `server.toml` is optional; `MEGU_CONFIG`
points it elsewhere. A running server re-reads `server.toml` on the
vanilla `/reload` command.

Environment knobs the runtime actually reads:

| variable | default | effect |
|---|---|---|
| `MEGU_PORT` | 25599 | game listener port (`--port 25565` for the conventional port) |
| `MEGU_BIND_ADDR` | 127.0.0.1 | game listener address |
| `MEGU_ALLOW_PUBLIC_BIND` | off | required to bind any non-loopback address |
| `MEGU_MOTD` | "megu" | server-list description and LAN announcement text |
| `MEGU_MAX_PLAYERS` | 20 | player cap (also the `status` reply) |
| `MEGU_SEED` | unset | signed 64-bit world seed |
| `MEGU_ONLINE_MODE` | off | `1`: encryption request/response plus the session check. Fails closed without a keypair/session provider — the flag alone is not a keypair |
| `MEGU_VELOCITY_FORWARDING` | off | `1`: Velocity modern forwarding. The proxy must answer `velocity:player_info` with an HMAC-SHA256 identity checked against `MEGU_VELOCITY_SECRET` (required when this is on). Checked before online mode — a proxy already terminated the client's real authentication, so the two are never stacked |
| `MEGU_ALLOWLIST` | off | comma/space-separated player names and/or UUIDs permitted to log in |
| `MEGU_BACKUP_MAX_KEEP` | 0 (off) | how many `backups/world-*.bak` snapshots stay after each rotation |
| `MEGU_BACKUP_INTERVAL_SECS` | 600 | minimum age before an older backup generation is rotated |
| `MEGU_LAN_BROADCAST` | off | `1`: multicast "Open to LAN" announcement armed once the game listener owns a port |
| `MEGU_SAFE_SPAWN` | auto | `1` forces the dry-ground spawn search; it already runs automatically on a never-saved world when the datapack worldgen is active |
| `MEGU_COMPRESSION_THRESHOLD` | 256 | packet compression threshold; `0`/negative disables |
| `MEGU_ZLIB_LEVEL` | 1 | compressed packet zlib level, clamped to `1..=9` |
| `MEGU_STRICT_WORLDGEN` | off | `1`: fail the datapack worldgen load on the first parse failure instead of omitting that entry |
| `MEGU_IDLE_TIMEOUT_SECS` | 60 | play state: no complete packet for this long drops the client |
| `MEGU_LOGIN_TIMEOUT_SECS` | 15 | wall-clock budget for handshake + login + configuration |
| `MEGU_MAX_CONN_PER_IP` | 4 (non-loopback) | concurrent connections per remote IP; `0` = unlimited; when set explicitly it also applies to loopback |
| `MEGU_RCON_PORT` | unset | RCON listener port — only starts when set together with a nonempty `MEGU_RCON_PASS` |
| `MEGU_RCON_BIND_ADDR` | 127.0.0.1 | RCON bind address |
| `MEGU_VERSION_DIR` | unset | kernel version pack directory for the block registry |
| `MEGU_CONFIG` | `server.toml` | path of the optional config file |
| `MEGU_ENABLE_QUERY` | off | `1`/`true` enables the GameSpy4 UDP Query responder; `enable-query=true` in `server.properties` also enables it |
| `MEGU_QUERY_PORT` | game port | UDP Query port; overrides `query.port` in `server.properties` |
| `MEGU_ANVIL_WORLD_DIR` | unset | load a vanilla-format Anvil world at boot and write it on autosave/shutdown; back up `playerdata/<uuid>.dat` with the world |

**GameSpy4 (GS4) Query.** Enable the opt-in UDP status/rules responder
with `MEGU_ENABLE_QUERY=1 megu`, or add `enable-query=true` to
`server.properties`. It uses the game port for UDP unless
`MEGU_QUERY_PORT` or `query.port` selects another port. Open the
corresponding UDP port if the query service should be remotely reachable.

**Console.** In live mode, stdin lines run as level-4 console commands:
`stop` saves the world and exits; `status` prints the operator health
line (TPS band + ms/tick, players, loaded chunks, memory). `status` is
deliberately console/RCON-only and never enters the player command tree —
tab completion stays 100% Vanilla. The same commands reach the server
over RCON when `MEGU_RCON_PORT` + `MEGU_RCON_PASS` are set.

**Level-4 server policies.** Operators can inspect or change the running
server's opt-in policy with `/megu policy`; settings are in-memory and
`reset` returns them to defaults:

```text
/megu policy list
/megu policy get minimap_mode
/megu policy set minimap_mode fair_play
/megu policy set allow_cave_map false
/megu policy set allow_mob_radar false
/megu policy set player_radar_scope party_only
/megu policy set allow_waypoint_teleport false
/megu policy set mine_all sneak_only
/megu policy set cut_all sneak_only
/megu policy reset
```

The five map controls are `minimap_mode` (`unrestricted` by default,
`fair_play`, `disabled`), `allow_cave_map`, `allow_mob_radar`,
`player_radar_scope` (`all` by default, `party_only`, `none`), and
`allow_waypoint_teleport`. Boolean switches take `true|false`; mine/cut
modes take `off` (default), `sneak_only`, `while_not_sneaking`, or
`always`. Additional keys are visible in `list`, including `xp_merge_mode`
and `max_frame_mib`. Access is checked at level 4 for both parsing and
execution. The integrated client provides corresponding host-world flags,
for example:

```sh
megu-client --fair-play --minimap-mode fair_play \
    --allow-cave-map false --allow-mob-radar false \
    --player-radar party_only --allow-waypoint-teleport false
```

`--fair-play` is opt-in. `--minimap-mode` defaults to `unrestricted`.
The client forwards host-world choices to its integrated server. Death-slot
restore currently has tested internal pickup logic but no general operator
toggle wired into the running server.

**Vanilla world import/export.** `--import-anvil <vanilla_world_dir>`
reads a vanilla 1.21.1 world (`level.dat` + `region/*.mca` + `DIM-1`/
`DIM1`), applies it in memory and persists it through the normal save
path; `--export-anvil <out_dir>` writes the saved world back out as a
vanilla directory. Both run once and exit — they are not server modes.

## 4. Docker quickstart

[docker/Dockerfile](../docker/Dockerfile) is multi-stage: the pinned Rust
toolchain builds `cargo build --release --locked`, and the slim runtime
ships only the binary, licence texts, the public tools and the
entrypoint — **no Mojang data**. The container runs as uid 10001 with
`/data` as the working directory and exposes `25565/tcp`.

```sh
docker build -f docker/Dockerfile -t megu:alpha .
docker run --name megu -e EULA=TRUE -p 25565:25565 \
    -v megu-data:/data megu:alpha
```

[docker/entrypoint.sh](../docker/entrypoint.sh) does, in order:

1. **EULA gate** — refuses to start unless `EULA=TRUE`. Nothing else
   counts and it is never accepted for you; a `eula.txt` is written into
   `/data` once as the same evidence a vanilla server carries.
2. **Writability check** — `/data` must be writable by uid 10001.
3. **Modpack pre-flight** — when `MEGU_MODPACK` names a pack inside
   the container, `tools/mod_launcher.py --modpack … --mc-version
   "${MEGU_MC_VERSION:-1.21.1}"` runs before the server may start.
4. `exec /usr/local/bin/megu` with the `MEGU_*` environment
   passed straight through.

The image defaults `MEGU_BIND_ADDR=0.0.0.0`,
`MEGU_ALLOW_PUBLIC_BIND=1` and `MEGU_PORT=25565` — inside the
image the container network is the isolation boundary and the operator's
`-p` still decides exposure; all three stay overridable from the
environment.

The server reads `assets/`, `datapacks/` and `config_packets.bin` from
its working directory. Without them it still starts, but worldgen and the
client registry handshake have no data — mount a `/data` volume that
carries the output of
[tools/vanilla_data/gen_vanilla_data.sh](../tools/vanilla_data/gen_vanilla_data.sh)
run against your own 1.21.1 server.jar. The full container data procedure
is [docs/release-checklist.md](release-checklist.md) §5.

## 5. World inspection, migration and status probing

`megu-world-migrate` works on `STV1` snapshots — the state-store
crate's world snapshot codec — and on JSON world-metadata documents,
between `1.21.1` and `26.3`.

```sh
# Validate an STV1 file and print the version it was saved under plus
# its dimension, chunk, block-override and block-entity counts.
megu-world-migrate inspect save.stv1

# Inspect a vanilla save directory directly: level.dat (gzip-wrapped or
# raw NBT) for the level name, saved-by version, seed and data packs,
# plus a region/, DIM-1/region/ and DIM1/region/ header scan counting
# populated chunks and classifying compression methods. Pure-Rust,
# read-only.
megu-world-migrate inspect-anvil /path/to/vanilla-world --json

# Migrate a real level.dat (gzip or raw NBT) to the 26.3 DataVersion.
# --in stays untouched; --backup-old preserves any existing --out.
megu-world-migrate migrate-level --in ./world/level.dat \
    --out ./converted/level.dat --to 26.3 --backup-old --json

# Re-seal a snapshot for another version. --out is written atomically
# and verified decodable before it is reported; --in is never modified.
megu-world-migrate migrate --in old.stv1 --out new.stv1 --to 26.3

# JSON world metadata (level.dat export, chunk or datapack metadata)
# through the same pipeline; output is deterministic sorted-key JSON.
megu-world-migrate migrate-json --in level-1211.json \
    --out level-263.json --to 26.3

# 26.3 -> 1.21.1 needs the explicit opt-in; downgrades can lose data the
# older format cannot represent.
megu-world-migrate migrate --in new.stv1 --out old.stv1 \
    --to 1.21.1 --allow-downgrade
```

For a live vanilla-format world, start the server with
`MEGU_ANVIL_WORLD_DIR=/path/to/vanilla-world megu`. The Anvil
import runs after the overlay load and wins on overlapping data; saves
write back to that directory. Back up an existing world before enabling
live writes, including its `playerdata/<uuid>.dat` entries. Verify player
inventory, identity and location restoration on a disposable copy before
moving a production world.

Exit codes: `0` success, `1` file-level failure (unreadable, corrupt, CRC
mismatch, malformed JSON, unknown DataVersion), `2` a refused downgrade
without `--allow-downgrade` or a bad command line. Every report takes
`--json`.

`megu-client --ping` is the server-list status probe:

```sh
megu-client --ping 127.0.0.1:25565
megu-client --ping example.org --mc-version 26.3 --json
```

It is SRV-aware and reports the version name/protocol the server
answered (and whether it matches the offered protocol), players
online/max with the sample name list, MOTD, latency and favicon.
`--mc-version` accepts `1.21.1`/`26.3` or the protocol numbers
`767`/`777` (default `1.21.1`); `--json`/`--json-summary` prints the
machine-readable status.

**Crash reporting flags.** `megu-client` installs a redacted panic hook at
startup, writes local `crash-<fp>.md` reports, and supports `ask` (default),
`auto_crash_only`, and `off` policies via:

```sh
megu-client --crash-report-mode ask|auto_crash_only|off
megu-client --report-endpoint https://<host>/api/report
megu-client --crash-reports-dir <dir> --flush-crash-reports [--confirm-send]
```

`--report-endpoint` enforces HTTPS (or loopback HTTP for local testing) and
fails closed with exit code 2 on unsafe URLs. Under `ask` mode, pending reports
are never transmitted without `--confirm-send`.

## 6. Proximity voice relay

`megu-voice-relay` is the zero-dependency UDP sidecar for the
clean-room `VCR1` proximity voice protocol in
[../proto/src/voice_relay.rs](../proto/src/voice_relay.rs). Serve mode
(the default) binds a datagram socket, authenticates players by shared
secret, and forwards microphone frames to the players in earshot —
distance and dimension culling plus a per-sender rate cap. It binds
`127.0.0.1:24454` by default; a non-loopback `--bind` is refused with
exit 2 unless `--allow-public-bind` is passed — the same opt-in posture
as `MEGU_ALLOW_PUBLIC_BIND` on the game listener.

```sh
megu-voice-relay                                # serve on 127.0.0.1:24454
megu-voice-relay --bind 0.0.0.0:24454 --allow-public-bind
megu-voice-relay --max-distance 64 --whisper-distance 24 --max-pps 128

# Offline checks that open no public socket.
megu-voice-relay --self-test                    # loopback auth + proximity, exit 0
megu-voice-relay --bench-packets 100000 --json  # p50/p99 routing latency + packets/s
```

The serve loop starts with an empty player table: until a game host
feeds it player state every `Auth` fails closed — the fail-safe
posture, not a bug — while the socket still exercises the live
decode/drop path and reports metrics on stderr. `--bench-packets N`
never opens a socket; it routes N frames over a synthetic 32-player
table spread across three dimensions and prints
p50/p99/mean/max latency and packets-per-second (text, or JSON with
`--json`). Exit codes: `0` success, `1` a bind error or failed
self-test, `2` a bad command line or a refused public bind.

**Friend invite relay (`FRL2`).** A host can connect outward to a
`FRL2` relay and share its issued invite code. Guests connect without
router port forwarding using the existing client flags:

```sh
megu-client --join-code MEGU-XXXX-XXXX --relay relay.example.org:24455
```

The code must be issued by a running host/relay; the example code is a
placeholder. A relay address is required with `--join-code`, supplied
either by `--relay` or the `MEGU_RELAY` environment variable. Invite
codes and host credentials do not travel the link in clear: guests prove
possession of the code with a nonce-bound HMAC (`key_id`/`tag`), and
hosts revoke with a MAC instead of resending their session key. The
inner Minecraft stream itself is forwarded opaquely — the relay cannot
read it, but an active MITM between peer and relay could still splice
it; treat `FRL2` as proof-of-possession, not a tunnel cipher.

**GPU backend selection.** The native client supports the following
backend preferences. `classic-low-spec` selects the 16 KiB UBO-batched
path for legacy Intel HD/UHD GPUs with limited shader support.

```sh
megu-client --gpu-backend auto
megu-client --gpu-backend mesh-shader
megu-client --gpu-backend compute-indirect
megu-client --gpu-backend classic-low-spec
megu-client --gpu-backend classic
```

The optional `megu-client-gpu` binary accepts a separate CLI:

```sh
megu-client-gpu --headless-probe
megu-client-gpu --offline-demo
megu-client-gpu --connect 127.0.0.1:25565
```

`--headless-probe` enumerates GPU adapters and the selected pipeline without
opening a window. `--offline-demo` opens a built-in scene without a server.
`--connect` joins a live multiplayer session (default port 25565), and
`--quickPlaySingleplayer <world>` launches a loopback `megu` host. Ship this
binary with `release_bundle.py build --extra-binary <path/to/megu-client-gpu>`
(or `.exe` on Windows).

**External shader packs.** Supply a shaderpack ZIP or directory and,
optionally, a named `profile.<NAME>` in its `shaders.properties`:

```sh
megu-client --shader-pack ./shaderpacks/example.zip
megu-client --shader-pack ./shaderpacks/example --shader-profile quality
```

OptiFine/Iris packs, DistantHorizons LOD packs and native Megu WGSL
packs are parsed by the shaderpack runtime and integrated with the
client's RenderGraph. If a pack requires unavailable GPU features,
the client reports the limitation and falls back to built-in rendering.
No third-party shaders are bundled.

## 7. Alpha caveats and security notes

- **This is Alpha.** The verified scope is what [../README.md](../README.md)
  and [../specs/ROADMAP.md](../specs/ROADMAP.md) state — treat anything
  beyond it as unverified.
- **Version coverage today.** The game listener accepts protocol 767
  (1.21.1): other handshakes get the vanilla "Outdated client! Please
  use 1.21.1." / "Outdated server!" reply. The `26.3` target lives in the
  tooling — `megu-init`, `megu-mod-check`,
  `megu-world-migrate`, `megu-client --mc-version` and the
  `megu-proto` tables — and in the world-migration pipeline.
- **The default listener is unauthenticated.** Loopback bind plus
  offline-mode login means unverified names/UUIDs and plaintext traffic.
  Do not expose the server on a public interface in that state. A
  non-loopback bind requires `MEGU_ALLOW_PUBLIC_BIND` and logs a
  security warning; for anything reachable, run `MEGU_ONLINE_MODE=1`
  or sit the server behind a Velocity proxy
  (`MEGU_VELOCITY_FORWARDING=1` + a shared
  `MEGU_VELOCITY_SECRET`), and still restrict access with a
  firewall/private network and optionally `MEGU_ALLOWLIST`.
- **RCON stays off** unless `MEGU_RCON_PORT` *and* a nonempty
  `MEGU_RCON_PASS` are set; it binds loopback by default and has
  per-IP failure lockouts. Treat the password as a secret — the tools
  never print it.
- **EULA acceptance requires an operator action.** Direct `megu-init`
  writes `eula=true` only with explicit `--accept-eula` when creating or
  replacing `eula.txt`. The container requires `EULA=TRUE` to start.
  The bare `megu` binary reads `eula.txt` from its working directory (or
  `--dir`) and exits 2 if the file exists but is not accepted; a missing
  `eula.txt` is allowed for first boots and development runs.
- [../SECURITY_NOTES.md](../SECURITY_NOTES.md) carries the full
  bind/allowlist posture; [../SECURITY.md](../SECURITY.md) is the
  reporting channel.

## 8. Cloudflare Pages report relay (optional)

The static [`website/`](../website) can be deployed to Cloudflare Pages
with the Pages Functions `POST /api/report` and `GET /api/latest`.
The reporting endpoint accepts a validated `megu-report/1` JSON payload
(4 KiB maximum), removes common sensitive strings, and creates or
deduplicates GitHub Issues. `GET /api/latest` reads the public release.

Four Cloudflare free-tier components used by this setup are **Pages**
(static site), **Pages Functions** (API execution), **Workers KV**
(`CRASH_INDEX` dedup state), and **Turnstile** (optional challenge). Free
tier limits and current eligibility are subject to Cloudflare's terms.

1. Create a Pages project with `website/` as the root and use
   [`website/wrangler.toml`](../website/wrangler.toml) as the template.
2. Create a KV namespace `CRASH_INDEX` and set its ID in the Pages
   project's KV binding. It is required to deduplicate fingerprinted
   crash/bug reports.
3. Set the non-secret `GITHUB_REPO=owner/name` variable if needed. Add
   `GITHUB_TOKEN` as a **Pages secret**, using a fine-grained token with
   `issues:write` for that repository.
4. Optionally enable Turnstile and register `TURNSTILE_SECRET_KEY` as a
   Pages secret. Once enabled, requests without a valid
   `turnstile_token` are denied.

**Zero-secret rule:** never commit tokens, Turnstile secret values, or
local `.env` credentials. Use the Pages dashboard or
`wrangler pages secret put <NAME> --project-name megu-site`.
Check API responses and GitHub Issue permissions before enabling public
report submission. The endpoint is a relay, not a full crash telemetry
service.

See also: [docs/development.md](development.md) for contributor setup and
test tiers, [docs/architecture-crates.md](architecture-crates.md) for the
crate map, and [tools/README.md](../tools/README.md) for the diagnostics
and fixture tooling.
