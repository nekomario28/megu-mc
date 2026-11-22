#!/usr/bin/env bash
# Container entrypoint for the megu release image (docker/Dockerfile).
#
# Order of operations, all fail-closed under `set -euo pipefail`:
#   1. EULA gate: refuse to start unless the operator passed EULA=TRUE.
#      Nothing here accepts the Minecraft EULA on anyone's behalf.
#   2. Optional modpack pre-flight: MEGU_MODPACK names a .mrpack/.zip
#      archive or a modrinth.index.json / CurseForge manifest.json inside
#      the container, checked by tools/mod_launcher.py before the server is
#      allowed to start.
#   3. exec the server with the MEGU_* environment passed straight
#      through (MEGU_PORT, MEGU_ONLINE_MODE,
#      MEGU_VELOCITY_FORWARDING, MEGU_BACKUP_MAX_KEEP,
#      MEGU_LAN_BROADCAST, MEGU_SAFE_SPAWN, ...).
set -euo pipefail

if [[ "${EULA:-}" != "TRUE" ]]; then
    cat >&2 <<'EOF'
megu: refusing to start - the Minecraft EULA has not been accepted.

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
data_dir="${MEGU_DATA_DIR:-/data}"
if [[ ! -w "$data_dir" ]]; then
    echo "megu: $data_dir is not writable by uid $(id -u);" >&2
    echo "  mount a volume the megu user (uid 10001) can write, e.g." >&2
    echo "  docker run -v megu-data:/data ..." >&2
    exit 1
fi

# Record the acceptance the way a vanilla server does - an eula.txt next to
# the world - so the mounted volume carries the same evidence as the env var.
if [[ ! -f "$data_dir/eula.txt" ]]; then
    printf '# Accepted via EULA=TRUE at container start.\neula=true\n' > "$data_dir/eula.txt"
fi

if [[ -n "${MEGU_MODPACK:-}" ]]; then
    echo "megu: pre-flighting modpack: $MEGU_MODPACK" >&2
    python3 /opt/megu/tools/mod_launcher.py \
        --modpack "$MEGU_MODPACK" \
        --mc-version "${MEGU_MC_VERSION:-1.21.1}"
fi

# Container networking defaults. A listener on 127.0.0.1 is unreachable
# through a published port, and the server refuses non-loopback binds
# without MEGU_ALLOW_PUBLIC_BIND. Inside the image the container
# network is the isolation boundary - the operator's -p/--publish still
# decides what is actually exposed - so both get defaults that stay
# overridable from the environment. Direct public access verifies Mojang
# identities by default. Velocity operators set MEGU_ONLINE_MODE=0 with
# MEGU_VELOCITY_FORWARDING=1 and MEGU_VELOCITY_SECRET. For offline
# development bind to 127.0.0.1 or explicitly opt into the insecure override.
: "${MEGU_BIND_ADDR:=0.0.0.0}"
: "${MEGU_ALLOW_PUBLIC_BIND:=1}"
: "${MEGU_ONLINE_MODE:=1}"
: "${MEGU_PORT:=25565}"
export MEGU_BIND_ADDR MEGU_ALLOW_PUBLIC_BIND MEGU_ONLINE_MODE MEGU_PORT

# Enforce the Rust listener's offline-public guard at the container boundary
# too, so a misconfigured instance is rejected before exec.
enabled() {
    case "${1:-}" in
        1|true|on) return 0 ;;
        *) return 1 ;;
    esac
}
case "${MEGU_BIND_ADDR,,}" in
    127.*|::1|'[::1]'|localhost) loopback=1 ;;
    *) loopback=0 ;;
esac
if [[ "$loopback" == 0 ]] \
    && ! enabled "$MEGU_ONLINE_MODE" \
    && ! enabled "${MEGU_VELOCITY_FORWARDING:-}" \
    && ! enabled "${MEGU_ALLOW_INSECURE_OFFLINE_PUBLIC:-}"; then
    echo 'megu: refusing public bind with unverified offline identities; enable MEGU_ONLINE_MODE=1, Velocity forwarding (MEGU_VELOCITY_FORWARDING=1 + MEGU_VELOCITY_SECRET), or explicitly set MEGU_ALLOW_INSECURE_OFFLINE_PUBLIC=1 only on a trusted network.' >&2
    exit 2
fi

# The image ships no Mojang data. The server still starts without it, but
# world generation and the client registry handshake need the generated
# 1.21.1 data mounted under /data - say so once, up front.
if [[ ! -f "$data_dir/config_packets.bin" || ! -f "$data_dir/assets/block_states_1211.json" ]]; then
    cat >&2 <<'EOF'
megu: note: /data has no generated 1.21.1 data (config_packets.bin,
assets/, datapacks/). The server starts, but worldgen and the client
registry handshake need data regenerated from your own 1.21.1 server.jar:

    tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar \
        --fetch-fabric-mappings --accept-eula

See docs/release-checklist.md for the full container data procedure.
EOF
fi

exec "${MEGU_BIN:-/usr/local/bin/megu}" "$@"
