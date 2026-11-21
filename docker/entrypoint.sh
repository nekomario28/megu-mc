#!/usr/bin/env bash
# Container entrypoint for the mc-spine release image (docker/Dockerfile).
#
# Order of operations, all fail-closed under `set -euo pipefail`:
#   1. EULA gate: refuse to start unless the operator passed EULA=TRUE.
#      Nothing here accepts the Minecraft EULA on anyone's behalf.
#   2. Optional modpack pre-flight: MC_SPINE_MODPACK names a .mrpack/.zip
#      archive or a modrinth.index.json / CurseForge manifest.json inside
#      the container, checked by tools/mod_launcher.py before the server is
#      allowed to start.
#   3. exec the server with the MC_SPINE_* environment passed straight
#      through (MC_SPINE_PORT, MC_SPINE_ONLINE_MODE,
#      MC_SPINE_VELOCITY_FORWARDING, MC_SPINE_BACKUP_MAX_KEEP,
#      MC_SPINE_LAN_BROADCAST, MC_SPINE_SAFE_SPAWN, ...).
set -euo pipefail

if [[ "${EULA:-}" != "TRUE" ]]; then
    cat >&2 <<'EOF'
mc-spine: refusing to start - the Minecraft EULA has not been accepted.

Running a Minecraft server requires accepting the EULA
(https://aka.ms/MinecraftEULA). To state that you accept it, pass it
explicitly; nothing else counts and it is never accepted for you:

    docker run -e EULA=TRUE ...
EOF
    exit 1
fi

# /data is the server's working directory: world save, server.toml and the
# regenerated vanilla data all live there, so an unwritable mount is a
# startup failure with a readable message, not a crash later.
if [[ ! -w /data ]]; then
    echo "mc-spine: /data is not writable by uid $(id -u);" >&2
    echo "  mount a volume the mc-spine user (uid 10001) can write, e.g." >&2
    echo "  docker run -v mc-spine-data:/data ..." >&2
    exit 1
fi

# Record the acceptance the way a vanilla server does - an eula.txt next to
# the world - so the mounted volume carries the same evidence as the env var.
if [[ ! -f /data/eula.txt ]]; then
    printf '# Accepted via EULA=TRUE at container start.\neula=true\n' > /data/eula.txt
fi

if [[ -n "${MC_SPINE_MODPACK:-}" ]]; then
    echo "mc-spine: pre-flighting modpack: $MC_SPINE_MODPACK" >&2
    python3 /opt/mc-spine/tools/mod_launcher.py \
        --modpack "$MC_SPINE_MODPACK" \
        --mc-version "${MC_SPINE_MC_VERSION:-1.21.1}"
fi

# Container networking defaults. A listener on 127.0.0.1 is unreachable
# through a published port, and the server refuses non-loopback binds
# without MC_SPINE_ALLOW_PUBLIC_BIND. Inside the image the container
# network is the isolation boundary - the operator's -p/--publish still
# decides what is actually exposed - so both get defaults that stay
# overridable from the environment.
: "${MC_SPINE_BIND_ADDR:=0.0.0.0}"
: "${MC_SPINE_ALLOW_PUBLIC_BIND:=1}"
: "${MC_SPINE_PORT:=25565}"
export MC_SPINE_BIND_ADDR MC_SPINE_ALLOW_PUBLIC_BIND MC_SPINE_PORT

# The image ships no Mojang data. The server still starts without it, but
# world generation and the client registry handshake need the generated
# 1.21.1 data mounted under /data - say so once, up front.
if [[ ! -f /data/config_packets.bin || ! -f /data/assets/block_states_1211.json ]]; then
    cat >&2 <<'EOF'
mc-spine: note: /data has no generated 1.21.1 data (config_packets.bin,
assets/, datapacks/). The server starts, but worldgen and the client
registry handshake need data regenerated from your own 1.21.1 server.jar:

    tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar \
        --fetch-fabric-mappings --accept-eula

See docs/release-checklist.md for the full container data procedure.
EOF
fi

exec /usr/local/bin/mc-spine "$@"
