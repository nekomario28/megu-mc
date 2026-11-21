# Operator guide

Running an mc-spine server: first-time setup, modpack pre-flight, runtime
knobs, Docker, world migration and status probing. Every command below is
copied from the binaries in this tree — each binary's `--help` is the
authoritative flag list.

Five binaries ship in this repository:

| binary | built from | job |
|---|---|---|
| `mc-spine` | root package ([../src/main.rs](../src/main.rs)) | the game server |
| `mc-spine-init` | [../server-files](../server-files) | bootstrap + offline admin of a server directory |
| `mc-spine-mod-check` | [../mod-adapter-core](../mod-adapter-core) | modpack pre-flight checker |
| `mc-spine-world-migrate` | [../state-store](../state-store) | inspect and migrate world data between 1.21.1 and 26.3 |
| `mc-spine-client` | [../client-app](../client-app) | native client; `--ping` doubles as a status probe |

The server and the tools target two Minecraft versions: `1.21.1`
(protocol 767, DataVersion 3955) and `26.3` (protocol 777, DataVersion
5023). What each piece does with them is stated where it is used; §6 is
honest about today's coverage.

## 1. First-time server setup

`mc-spine-init` writes the same files a Vanilla server directory carries
(`eula.txt`, `server.properties`, `ops.json`, `whitelist.json`,
`banned-players.json`, `banned-ips.json`, `usercache.json`) and edits them
offline while the server is stopped. It never clobbers an existing file;
on an existing `server.properties` only the flags you pass are applied.

```sh
# Create the directory and its files. --mc-version pins the target
# (default: 26.3, the latest vanilla).
mc-spine-init init --dir /srv/spine --mc-version 1.21.1 \
    --port 25565 --motd "mc-spine" --max-players 20

# init writes eula.txt with eula=false — the tool never accepts it for
# you. Accepting the Minecraft EULA (https://aka.ms/MinecraftEULA) is the
# operator's act: set eula=true yourself.
$EDITOR /srv/spine/eula.txt

# Read-only pre-flight. Exit 0 = READY, 2 = EULA missing/not accepted,
# 1 = a server file is unreadable or corrupt. rcon.password is always
# redacted in the report.
mc-spine-init doctor --dir /srv/spine

# Offline administration (all take --json for machine output).
mc-spine-init op --dir /srv/spine <player> --level 4
mc-spine-init deop --dir /srv/spine <player>
mc-spine-init whitelist --dir /srv/spine on
mc-spine-init whitelist --dir /srv/spine add <player>
mc-spine-init whitelist --dir /srv/spine list
mc-spine-init ban --dir /srv/spine <player> --reason "griefing"
mc-spine-init pardon --dir /srv/spine <player>
```

`op` defaults to the directory's `op-permission-level` (else 4);
`--bypass-limit` sets `bypassesPlayerLimit`. `ban` records an offline-UUID
entry; `--reason`, `--created` and `--source` have fixed defaults the
`--help` text lists.

## 2. Modpack pre-flight

Before a pack is downloaded or launched, `mc-spine-mod-check` reports
whether it can run on this server. It accepts a Modrinth `.mrpack`
archive, a standalone `modrinth.index.json`, or a CurseForge
`manifest.json` / pack `.zip`.

```sh
mc-spine-mod-check --modpack pack.mrpack --mc-version 1.21.1
mc-spine-mod-check --modpack pack.mrpack --mc-version 26.3
```

The report says which files run natively, which client-only files are
skipped on a dedicated server, which Java JARs still need a bytecode
inspection, and which redundant performance mods are skipped
automatically. `--flagship <ids>` names mod ids handled by a flagship
native adapter (default `create`); `--profiles <ids>` names ids covered
by declarative offload profiles; `--json` prints the machine report.

Exit codes: `0` the pack can run (natively, with automatic skips, or
pending a bytecode inspection), `2` a version mismatch or bad arguments,
`1` an unreadable or unsafe pack.

**About the automatic skips.** `sodium`, `lithium`, `modernfix`,
`distanthorizons`, `ferritecore` and similar performance mods are never
loaded: `mc-spine` already ships equivalent optimization categories as
its own original clean-room Rust code. The skip list is a mod-id
blocklist — `BUILTIN_SUPERSEDED_PERF_MODS` in
[../mod-adapter-core/src/fallback.rs](../mod-adapter-core/src/fallback.rs)
— so no third-party mod code is copied, linked, bundled or derived from.
Skipping those ids does not affect the rest of the pack.

The same classifier is available as a Python tool —
`python3 tools/mod_launcher.py --modpack pack.mrpack --mc-version 1.21.1`
— which is what the container entrypoint runs (§4).

## 3. Running the server

Start it in the server working directory; the world saves to
`world_overlay.bin` (plus per-dimension companions) there.

```sh
cd /srv/spine
mc-spine            # or: MC_SPINE_PORT=25565 mc-spine
```

Settings resolve in three layers, highest wins: **environment →
`server.toml` → a dropped-in Vanilla `server.properties`** (the last one
is read, never created). `server.toml` is optional; `MC_SPINE_CONFIG`
points it elsewhere. A running server re-reads `server.toml` on the
vanilla `/reload` command.

Environment knobs the runtime actually reads:

| variable | default | effect |
|---|---|---|
| `MC_SPINE_PORT` | 25565 | game listener port |
| `MC_SPINE_BIND_ADDR` | 127.0.0.1 | game listener address |
| `MC_SPINE_ALLOW_PUBLIC_BIND` | off | required to bind any non-loopback address |
| `MC_SPINE_MOTD` | "mc-spine" | server-list description and LAN announcement text |
| `MC_SPINE_MAX_PLAYERS` | 20 | player cap (also the `status` reply) |
| `MC_SPINE_SEED` | unset | signed 64-bit world seed |
| `MC_SPINE_ONLINE_MODE` | off | `1`: encryption request/response plus the session check. Fails closed without a keypair/session provider — the flag alone is not a keypair |
| `MC_SPINE_VELOCITY_FORWARDING` | off | `1`: Velocity modern forwarding. The proxy must answer `velocity:player_info` with an HMAC-SHA256 identity checked against `MC_SPINE_VELOCITY_SECRET` (required when this is on). Checked before online mode — a proxy already terminated the client's real authentication, so the two are never stacked |
| `MC_SPINE_ALLOWLIST` | off | comma/space-separated player names and/or UUIDs permitted to log in |
| `MC_SPINE_BACKUP_MAX_KEEP` | 0 (off) | how many `backups/world-*.bak` snapshots stay after each rotation |
| `MC_SPINE_BACKUP_INTERVAL_SECS` | 600 | minimum age before an older backup generation is rotated |
| `MC_SPINE_LAN_BROADCAST` | off | `1`: multicast "Open to LAN" announcement armed once the game listener owns a port |
| `MC_SPINE_SAFE_SPAWN` | auto | `1` forces the dry-ground spawn search; it already runs automatically on a never-saved world when the datapack worldgen is active |
| `MC_SPINE_COMPRESSION_THRESHOLD` | 256 | packet compression threshold; `0`/negative disables |
| `MC_SPINE_IDLE_TIMEOUT_SECS` | 60 | play state: no complete packet for this long drops the client |
| `MC_SPINE_LOGIN_TIMEOUT_SECS` | 15 | wall-clock budget for handshake + login + configuration |
| `MC_SPINE_MAX_CONN_PER_IP` | 4 (non-loopback) | concurrent connections per remote IP; `0` = unlimited; when set explicitly it also applies to loopback |
| `MC_SPINE_RCON_PORT` | unset | RCON listener port — only starts when set together with a nonempty `MC_SPINE_RCON_PASS` |
| `MC_SPINE_RCON_BIND_ADDR` | 127.0.0.1 | RCON bind address |
| `MC_SPINE_VERSION_DIR` | unset | kernel version pack directory for the block registry |
| `MC_SPINE_CONFIG` | `server.toml` | path of the optional config file |

**Console.** In live mode, stdin lines run as level-4 console commands:
`stop` saves the world and exits; `status` prints the operator health
line (TPS band + ms/tick, players, loaded chunks, memory). `status` is
deliberately console/RCON-only and never enters the player command tree —
tab completion stays 100% Vanilla. The same commands reach the server
over RCON when `MC_SPINE_RCON_PORT` + `MC_SPINE_RCON_PASS` are set.

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
docker build -f docker/Dockerfile -t mc-spine:alpha .
docker run --name mc-spine -e EULA=TRUE -p 25565:25565 \
    -v mc-spine-data:/data mc-spine:alpha
```

[docker/entrypoint.sh](../docker/entrypoint.sh) does, in order:

1. **EULA gate** — refuses to start unless `EULA=TRUE`. Nothing else
   counts and it is never accepted for you; a `eula.txt` is written into
   `/data` once as the same evidence a vanilla server carries.
2. **Writability check** — `/data` must be writable by uid 10001.
3. **Modpack pre-flight** — when `MC_SPINE_MODPACK` names a pack inside
   the container, `tools/mod_launcher.py --modpack … --mc-version
   "${MC_SPINE_MC_VERSION:-1.21.1}"` runs before the server may start.
4. `exec /usr/local/bin/mc-spine` with the `MC_SPINE_*` environment
   passed straight through.

The image defaults `MC_SPINE_BIND_ADDR=0.0.0.0`,
`MC_SPINE_ALLOW_PUBLIC_BIND=1` and `MC_SPINE_PORT=25565` — inside the
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

`mc-spine-world-migrate` works on `STV1` snapshots — the state-store
crate's world snapshot codec — and on JSON world-metadata documents,
between `1.21.1` and `26.3`.

```sh
# Validate an STV1 file and print the version it was saved under plus
# its dimension, chunk, block-override and block-entity counts.
mc-spine-world-migrate inspect save.stv1

# Re-seal a snapshot for another version. --out is written atomically
# and verified decodable before it is reported; --in is never modified.
mc-spine-world-migrate migrate --in old.stv1 --out new.stv1 --to 26.3

# JSON world metadata (level.dat export, chunk or datapack metadata)
# through the same pipeline; output is deterministic sorted-key JSON.
mc-spine-world-migrate migrate-json --in level-1211.json \
    --out level-263.json --to 26.3

# 26.3 -> 1.21.1 needs the explicit opt-in; downgrades can lose data the
# older format cannot represent.
mc-spine-world-migrate migrate --in new.stv1 --out old.stv1 \
    --to 1.21.1 --allow-downgrade
```

Exit codes: `0` success, `1` file-level failure (unreadable, corrupt, CRC
mismatch, malformed JSON, unknown DataVersion), `2` a refused downgrade
without `--allow-downgrade` or a bad command line. Every report takes
`--json`.

`mc-spine-client --ping` is the server-list status probe:

```sh
mc-spine-client --ping 127.0.0.1:25565
mc-spine-client --ping example.org --mc-version 26.3 --json
```

It is SRV-aware and reports the version name/protocol the server
answered (and whether it matches the offered protocol), players
online/max with the sample name list, MOTD, latency and favicon.
`--mc-version` accepts `1.21.1`/`26.3` or the protocol numbers
`767`/`777` (default `1.21.1`); `--json`/`--json-summary` prints the
machine-readable status.

## 6. Alpha caveats and security notes

- **This is Alpha.** The verified scope is what [../README.md](../README.md)
  and [../specs/ROADMAP.md](../specs/ROADMAP.md) state — treat anything
  beyond it as unverified.
- **Version coverage today.** The game listener accepts protocol 767
  (1.21.1): other handshakes get the vanilla "Outdated client! Please
  use 1.21.1." / "Outdated server!" reply. The `26.3` target lives in the
  tooling — `mc-spine-init`, `mc-spine-mod-check`,
  `mc-spine-world-migrate`, `mc-spine-client --mc-version` and the
  `mc-spine-proto` tables — and in the world-migration pipeline.
- **The default listener is unauthenticated.** Loopback bind plus
  offline-mode login means unverified names/UUIDs and plaintext traffic.
  Do not expose the server on a public interface in that state. A
  non-loopback bind requires `MC_SPINE_ALLOW_PUBLIC_BIND` and logs a
  security warning; for anything reachable, run `MC_SPINE_ONLINE_MODE=1`
  or sit the server behind a Velocity proxy
  (`MC_SPINE_VELOCITY_FORWARDING=1` + a shared
  `MC_SPINE_VELOCITY_SECRET`), and still restrict access with a
  firewall/private network and optionally `MC_SPINE_ALLOWLIST`.
- **RCON stays off** unless `MC_SPINE_RCON_PORT` *and* a nonempty
  `MC_SPINE_RCON_PASS` are set; it binds loopback by default and has
  per-IP failure lockouts. Treat the password as a secret — the tools
  never print it.
- **The EULA is never accepted for you** — not by `mc-spine-init`
  (writes `eula=false`), not by the container (`EULA=TRUE` or it refuses
  to start). The bare `mc-spine` binary does not read `eula.txt`;
  accepting the Minecraft EULA is the operator's act either way.
- [../SECURITY_NOTES.md](../SECURITY_NOTES.md) carries the full
  bind/allowlist posture; [../SECURITY.md](../SECURITY.md) is the
  reporting channel.

See also: [docs/development.md](development.md) for contributor setup and
test tiers, [docs/architecture-crates.md](architecture-crates.md) for the
crate map, and [tools/README.md](../tools/README.md) for the diagnostics
and fixture tooling.
